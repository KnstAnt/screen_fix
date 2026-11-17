use std::{collections::BTreeMap, num::NonZeroU16, path::PathBuf, thread, time::Duration};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;

use screen_fix_core::{
    config::{
        CapturePace, ClipboardMode, EffectiveCapabilities, MAX_SELECTED_MONITORS, ResolvedCaptureConfig,
        ResolvedProfile, SelectedMonitor,
    },
    package::diagnostics::{DiagnosticSnapshot, Failure, FailureComponent, ReportOutcome},
    platform,
    protocol::MonitorId,
    session::{PauseReason, SessionCommand, SessionHandle, SessionOutcome, SessionPhase, SessionResult, SessionStatus, SourceStatus, StartupStep},
};

#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(target_os = "windows")]
use screen_fix_core::session::hotkey::{ControlOutcome, HotkeyActivation, HotkeyBindings, RecorderAction};

pub(crate) struct ScreenFix {
    state: RecordingState,
    last_status: Option<SessionStatus>,
    #[cfg(target_os = "windows")]
    native_window: Option<platform::RecorderWindow>,
    #[cfg(target_os = "windows")]
    native_window_error: Option<String>,
    #[cfg(target_os = "windows")]
    indicator_error: Option<String>,
    #[cfg(target_os = "windows")]
    indicator_diagnostic: Option<DiagnosticSnapshot>,
    close_after_recording: bool,
    incomplete_pending: bool,
    record_clipboard: bool,
    record_pen: bool,
    #[cfg(target_os = "windows")]
    approved_executable: Option<screen_fix_core::config::ExecutableIdentity>,
    #[cfg(target_os = "windows")]
    approved_windows: Option<Result<platform::ApprovedWindows, String>>,
    #[cfg(target_os = "windows")]
    setup_probe: Option<Receiver<SetupInventory>>,
    #[cfg(target_os = "windows")]
    display_error: Option<String>,
    #[cfg(target_os = "windows")]
    manual_monitors: bool,
    displays: Vec<platform::Display>,
    display_ids: BTreeMap<String, MonitorId>,
    selected: Vec<SelectedMonitor>,
    pace: CapturePace,
    command_error: Option<screen_fix_core::session::CommandError>,
    folder: PathBuf,
    #[cfg(target_os = "windows")]
    hotkey_bindings: HotkeyBindings,
    #[cfg(target_os = "windows")]
    hotkeys: Option<platform::RecorderHotkeys>,
    #[cfg(target_os = "windows")]
    hotkey_error: Option<String>,
    #[cfg(target_os = "windows")]
    hotkey_probe: Option<Receiver<Result<platform::RecorderHotkeys, String>>>,
}

impl Default for ScreenFix {
    fn default() -> Self {
        Self {
            state: RecordingState::Idle,
            last_status: None,
            #[cfg(target_os = "windows")]
            native_window: None,
            #[cfg(target_os = "windows")]
            native_window_error: None,
            #[cfg(target_os = "windows")]
            indicator_error: None,
            #[cfg(target_os = "windows")]
            indicator_diagnostic: None,
            close_after_recording: false,
            incomplete_pending: false,
            record_clipboard: false,
            record_pen: false,
            #[cfg(target_os = "windows")]
            approved_executable: None,
            #[cfg(target_os = "windows")]
            approved_windows: None,
            #[cfg(target_os = "windows")]
            setup_probe: None,
            #[cfg(target_os = "windows")]
            display_error: None,
            #[cfg(target_os = "windows")]
            manual_monitors: false,
            displays: Vec::new(),
            display_ids: BTreeMap::new(),
            selected: Vec::new(),
            pace: CapturePace::Fps30,
            command_error: None,
            folder: PathBuf::new(),
            #[cfg(target_os = "windows")]
            hotkey_bindings: HotkeyBindings::default(),
            #[cfg(target_os = "windows")]
            hotkeys: None,
            #[cfg(target_os = "windows")]
            hotkey_error: None,
            #[cfg(target_os = "windows")]
            hotkey_probe: None,
        }
    }
}

#[cfg(target_os = "windows")]
struct SetupInventory {
    displays: Result<Vec<platform::Display>, String>,
    approved_windows: Option<Result<platform::ApprovedWindows, String>>,
    resolved_executable: Option<Result<screen_fix_core::config::ExecutableIdentity, screen_fix_core::CaptureError>>,
}

#[derive(Default)]
enum RecordingState {
    #[default]
    Idle,
    Running { session: SessionHandle, status: SessionStatus, stop_requested: bool },
    Reporting { message: String, result: Receiver<String> },
    Complete(Result<SessionResult, String>),
}

impl eframe::App for ScreenFix {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "windows")]
        {
            if let Some(error) = self.native_window.as_mut().and_then(|window| window.take_indicator_error()) {
                self.indicator_error = Some(error.to_string());
                self.indicator_diagnostic = Some(error.into_diagnostic().with_output_root(self.folder.clone()));
                if let RecordingState::Running { session, .. } = &self.state {
                    let _ = session.submit(SessionCommand::Stop);
                }
            }
            self.attach_window(_frame);
            if self.native_window.is_none() && self.indicator_error.is_none() {
                if let RecordingState::Running { session, .. } = &self.state {
                    self.indicator_error = Some("The recording indicator window is unavailable.".into());
                    self.indicator_diagnostic = Some(diagnostic(FailureComponent::Interface,
                        "Maintain the recording indicator").with_output_root(self.folder.clone()));
                    let _ = session.submit(SessionCommand::Stop);
                }
            }
            self.poll_inventory(ctx);
            self.poll_hotkeys(ctx);
        }
        match &mut self.state {
            RecordingState::Running { session, status, .. } => {
                if let Some(latest) = session.status() {
                    *status = latest;
                }
                match session.try_result() {
                    Ok(Ok(message)) => {
                        self.incomplete_pending = message.outcome == SessionOutcome::Incomplete;
                        self.last_status = Some(session.status().unwrap_or_else(|| status.clone()));
                        #[cfg(target_os = "windows")]
                        let report = self.indicator_diagnostic.take().map(|snapshot| {
                            let issue = self.indicator_error.as_deref().unwrap_or("The recording indicator failed.");
                            recording_error(format!("{issue}\n{}", message.message), snapshot, Some(ctx))
                        });
                        self.state = RecordingState::Complete(Ok(message));
                        #[cfg(target_os = "windows")]
                        if let Some(report) = report { self.state = report; }
                    }
                    Ok(Err(error)) => {
                        self.last_status = Some(session.status().unwrap_or_else(|| status.clone()));
                        let message = match error.source_failure() {
                            Some(failure) if failure.admitted => {
                                self.incomplete_pending = true;
                                if let Some(retained) = error.retained_recording() {
                                    format!("Recording failed after capture started. Saved the failed attempt with {} verified video frames.\n{}\n{error}",
                                        retained.verified_video_frames, retained.root.display())
                                } else {
                                    format!("Recording failed after capture started. The failed recording manifest could not be confirmed.\n{error}")
                                }
                            }
                            Some(failure) if failure.reason == screen_fix_core::session::SourceFailureReason::TokenIdentity
                                && failure.stage == screen_fix_core::session::SourceStage::Foreground =>
                            {
                                format!("Recording did not start.\n{error}")
                            }
                            Some(_) => format!("Elevated capture unavailable. Recording did not start.\n{error}"),
                            None => error.to_string(),
                        };
                        self.state = recording_error(message, error.into_diagnostic().with_output_root(self.folder.clone()), Some(ctx));
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.last_status = Some(session.status().unwrap_or_else(|| status.clone()));
                        self.state = recording_error(
                            "The session controller exited without returning a result.".into(),
                            diagnostic(FailureComponent::Controller, "Read the session result").with_output_root(self.folder.clone()),
                            Some(ctx),
                        );
                    }
                    Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                }
            }
            RecordingState::Reporting { message, result } => match result.try_recv() {
                Ok(report) => self.state = RecordingState::Complete(Err(report)),
                Err(TryRecvError::Disconnected) => self.state = RecordingState::Complete(Err(with_report(
                    message, ReportOutcome::Unavailable { native_code: None },
                ))),
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            },
            _ => {}
        }

        if self.close_after_recording && matches!(&self.state, RecordingState::Complete(Ok(result))
            if matches!(result.outcome, SessionOutcome::Complete | SessionOutcome::Cancelled)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.close_after_recording &= self.incomplete_pending || matches!(self.state, RecordingState::Running { .. });

        #[cfg(target_os = "windows")]
        if matches!(self.state, RecordingState::Idle | RecordingState::Complete(_)) {
            if let Some(hotkeys) = self.hotkeys.as_ref() {
                if let Some(activation) = hotkeys.take_start() {
                    self.start_recording(ctx, Some(activation));
                }
                ctx.request_repaint_after(Duration::from_millis(40));
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("screen_fix");
            egui::ScrollArea::vertical().show(ui, |ui| self.recording_panel(ui, ctx, _frame));
        });
    }

    #[cfg(target_os = "windows")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.native_window = None;
    }
}

impl ScreenFix {
    pub(crate) fn new(_creation: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        #[cfg(target_os = "windows")]
        {
            app.attach_window(_creation);
            app.refresh_inventory(&_creation.egui_ctx);
            app.register_hotkeys(&_creation.egui_ctx);
        }
        app
    }

    #[cfg(target_os = "windows")]
    fn attach_window(&mut self, owner: &impl HasWindowHandle) {
        let Ok(handle) = owner.window_handle() else {
            self.native_window = None;
            self.native_window_error = Some("The native recorder window is unavailable.".into());
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            self.native_window = None;
            self.native_window_error = Some("The recorder viewport did not provide a Windows window.".into());
            return;
        };
        let id = screen_fix_core::protocol::WindowId(handle.hwnd.get() as usize as u64);
        if self.native_window.as_ref().is_some_and(|window| window.id() == id) {
            self.native_window_error = None;
            return;
        }
        self.native_window = None;
        match platform::attach_recorder_window(id) {
            Ok(mut window) => {
                if let RecordingState::Running { session, .. } = &self.state {
                    if let Err(error) = window.observe(session) {
                        self.indicator_error = Some(error.to_string());
                        self.indicator_diagnostic = Some(error.into_diagnostic().with_output_root(self.folder.clone()));
                        let _ = session.submit(SessionCommand::Stop);
                    }
                }
                self.native_window = Some(window);
                self.native_window_error = None;
            }
            Err(error) => self.native_window_error = Some(error.to_string()),
        }
    }

    fn recording_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let running = self.incomplete_pending || matches!(self.state, RecordingState::Running { .. } | RecordingState::Reporting { .. });
        #[cfg(target_os = "windows")]
        if let Some(error) = self.indicator_error.as_ref().or(self.native_window_error.as_ref()) {
            ui.label(error);
        }
        if platform::VIDEO_RECORDING_AVAILABLE && !running {
            ui.label("Capture stays on the selected displays. Recordings are stored locally.");
            ui.label("Displays");
            for item in &self.displays {
                let mut selected = self.selected.iter().any(|entry| entry.key == item.key);
                if ui.add_enabled(selected || self.selected.len() < MAX_SELECTED_MONITORS,
                    egui::Checkbox::new(&mut selected, &item.label)).changed()
                {
                    #[cfg(target_os = "windows")]
                    { self.manual_monitors = true; }
                    if selected {
                        self.selected.push(SelectedMonitor {
                            id: self.display_ids[item.key.as_str()],
                            key: item.key.clone(),
                        });
                    } else {
                        self.selected.retain(|entry| entry.key != item.key);
                    }
                }
            }
            for missing in self.selected.iter().filter(|entry| !self.displays.iter().any(|item| item.key == entry.key)) {
                ui.label(format!("Selected monitor {} is disconnected.", missing.id.0));
            }
            if self.selected.iter().any(|entry| !self.displays.iter().any(|item| item.key == entry.key))
                && ui.button("Remove disconnected selections").clicked()
            {
                #[cfg(target_os = "windows")]
                { self.manual_monitors = true; }
                self.selected.retain(|entry| self.displays.iter().any(|item| item.key == entry.key));
            }
            egui::ComboBox::from_label("Capture rate")
                .selected_text(match self.pace { CapturePace::Fps30 => "30 FPS", CapturePace::Fps60 => "60 FPS pilot" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.pace, CapturePace::Fps30, "30 FPS");
                    ui.selectable_value(&mut self.pace, CapturePace::Fps60, "60 FPS pilot");
                });
            #[cfg(target_os = "windows")]
            {
                if let Some(error) = &self.display_error {
                    ui.label(error);
                }
                if ui.add_enabled(self.setup_probe.is_none(), egui::Button::new("Refresh displays and windows")).clicked() {
                    self.refresh_inventory(ctx);
                }
                if self.setup_probe.is_some() {
                    ui.label("Checking displays and approved windows…");
                }
            }
            ui.label("Output directory");
            ui.horizontal(|ui| {
                let mut path = self.folder.to_string_lossy().into_owned();
                let field = egui::TextEdit::singleline(&mut path)
                    .hint_text("Choose a folder or paste its path");
                if ui.add_sized([(ui.available_width() - 82.0).max(40.0), 22.0], field).changed() {
                    self.folder = PathBuf::from(path.trim());
                }
                #[cfg(target_os = "windows")]
                if ui.button("Browse…").clicked() {
                    let mut dialog = rfd::FileDialog::new()
                        .set_title("Choose an output folder")
                        .set_parent(_frame);
                    if !self.folder.as_os_str().is_empty() {
                        dialog = dialog.set_directory(&self.folder);
                    }
                    if let Some(folder) = dialog.pick_folder() {
                        self.folder = folder;
                    }
                }
            });
            ui.checkbox(&mut self.record_clipboard, "Log clipboard changes");
            ui.checkbox(&mut self.record_pen, "Record pen input");
        }
        #[cfg(target_os = "windows")]
        {
            if !running {
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.setup_probe.is_none(), egui::Button::new("Approve an executable…")).clicked() {
                        let chosen = rfd::FileDialog::new()
                            .set_title("Choose the application whose input may be recorded")
                            .set_parent(_frame)
                            .add_filter("Executables", &["exe"])
                            .pick_file();
                        if let Some(chosen) = chosen {
                            self.start_inventory(ctx, Some(chosen));
                        }
                    }
                    if self.approved_executable.is_some() && ui.add_enabled(self.setup_probe.is_none(), egui::Button::new("Clear approval")).clicked() {
                        self.approved_executable = None;
                        self.approved_windows = None;
                        if !self.manual_monitors {
                            self.selected.clear();
                        }
                    }
                });
                ui.label(self.approved_executable.as_ref().map_or(
                    "Video and input recording require an approved executable in the foreground.", |entry| entry.executable(),
                ));
                match &self.approved_windows {
                    Some(Ok(windows)) => {
                        let unselected = windows.monitors.iter().filter(|key| !self.selected.iter().any(|entry| &entry.key == *key)).count();
                        if unselected != 0 {
                            ui.label(format!("The approved application has a window on {unselected} unselected monitor(s)."));
                        }
                        if windows.evidence_incomplete {
                            ui.label("Some approved window locations could not be verified.");
                        }
                        if windows.monitors.is_empty() {
                            ui.label("No visible approved window was found on a connected monitor.");
                        }
                    }
                    Some(Err(error)) => { ui.label(format!("Approved window locations unknown. {error}")); }
                    None => {}
                }
                if self.approved_executable.is_some() && self.manual_monitors
                    && ui.add_enabled(self.setup_probe.is_none(), egui::Button::new("Use displays with approved windows")).clicked()
                {
                    self.manual_monitors = false;
                    self.refresh_inventory(ctx);
                }
                self.hotkey_settings(ui, ctx);
                ui.collapsing("M1 recording scope", |ui| {
                    ui.label("App-pack version, adapter and shortcut checks, task duration, start file, goal and task consent are not part of M1.");
                    ui.label("The 60 FPS profile is a sculpt and paint pilot. Recording rates depend on the per-monitor encoder check.");
                });
            }
            if !running {
                let issue = self.start_issue();
                if let Some(issue) = issue {
                    ui.label(issue);
                }
                let pause_key = self.hotkey_bindings.function(RecorderAction::TogglePause);
                ui.label(format!("Before typing private information in the approved application, pause recording with Ctrl+Alt+F{pause_key}. Resume only when the private information is no longer visible."));
                ui.label("M1 does not hide the recorder or other windows from captured displays. The recording indicator can appear in video and stills.");
                if ui.add_enabled(issue.is_none(), egui::Button::new("Start recording")).clicked() {
                    self.start_recording(ctx, None);
                }
            }
        }

        if !platform::VIDEO_RECORDING_AVAILABLE {
            ui.label("Recording is unavailable on this platform.");
        }
        if !matches!(self.state, RecordingState::Running { .. }) {
            if let Some(status) = &self.last_status {
                Self::show_windows_status(ui, status);
            }
        }
        match &mut self.state {
            RecordingState::Idle => {}
            RecordingState::Running { session, status, stop_requested } => {
                let stopping = *stop_requested || matches!(status.phase, SessionPhase::Stopping);
                match status.phase {
                    SessionPhase::Starting if !stopping => {
                        if let Some(seconds) = status.countdown_seconds {
                            if seconds == 0 {
                                ui.label("Waiting for Windows to become ready.");
                            } else {
                                ui.label(format!("Switch to the approved application. Recording starts in {seconds}…"));
                            }
                        } else if !matches!(status.source, SourceStatus::AwaitingConsent | SourceStatus::AwaitingReadiness) {
                            ui.label(match status.startup {
                                StartupStep::Windows => "Checking Windows compatibility…",
                                StartupStep::ApprovedApplication => "Checking the approved application and its permissions…",
                                StartupStep::OutputSpace => "Checking free space in the output folder…",
                                StartupStep::MediaFoundation => "Starting Windows Media Foundation…",
                                StartupStep::DisplaysAndEncoders => "Checking selected displays and qualifying their H.264 encoders…",
                                StartupStep::InputHelper => "Waiting for the input helper and its keyboard and mouse hooks…",
                                StartupStep::Ready => "Ready. Preparing the countdown…",
                                StartupStep::VerifyingAdmission => "Keep the approved application in front. Verifying readiness…",
                            });
                        }
                    }
                    SessionPhase::Recording if !stopping => {
                        if status.input_unavailable.is_some() {
                            ui.label("Recording · input unavailable");
                        } else {
                            ui.label(match status.source {
                                SourceStatus::Ordinary => "Recording · standard capture",
                                SourceStatus::Enhanced => "Recording · enhanced capture",
                                SourceStatus::Activating => "Waiting for certified elevated capture…",
                                SourceStatus::FailedPrefix(_) => "Closing the failed recording prefix…",
                                _ => "Recording",
                            });
                        }
                        if !status.video_unavailable.is_empty() {
                            ui.label(format!("Video unavailable on {} monitor(s)", status.video_unavailable.len()));
                        }
                    }
                    SessionPhase::Paused if !stopping => {
                        ui.label("Recording paused");
                        for reason in &status.pause_reasons {
                            ui.label(match reason {
                                PauseReason::User => "Paused manually",
                                PauseReason::LockScreen => "Windows session locked",
                                PauseReason::Sleep => "System suspended",
                                PauseReason::SecureDesktop => "Input desktop unavailable",
                                PauseReason::LowDisk => "Disk space unavailable or below the recording limit",
                                PauseReason::ElevationTransition => "Switching capture source",
                            });
                        }
                    }
                    SessionPhase::Stopping if status.final_publication_pending => {
                        ui.label("Final publication pending. Waiting for Windows…");
                    }
                    SessionPhase::Failed => { ui.label("Recording failed. Closing recording resources…"); }
                    SessionPhase::Finished => { ui.label("Recording finished"); }
                    _ => { ui.label("Stopping and finalizing the recording…"); }
                }
                if !stopping && matches!(status.phase, SessionPhase::Starting | SessionPhase::Paused) {
                    match status.source {
                        SourceStatus::AwaitingConsent => { ui.label("Waiting for Windows elevation consent…"); }
                        SourceStatus::AwaitingReadiness => { ui.label("Verifying the elevated capture source…"); }
                        _ => {}
                    }
                }
                if status.phase == SessionPhase::Starting {
                    if let Some(bytes) = status.disk_available_bytes {
                        ui.label(format!("Output volume free space: {:.1} GB", bytes as f64 / 1_000_000_000.0));
                    }
                    if let Some(registration) = status.input_ready.filter(|_| status.input_helper_ready) {
                        ui.label("Standard input helper, Raw Input and keyboard and mouse hooks ready.");
                        if registration.pen_hid {
                            ui.label("Pen HID registration ready. Device and driver evidence will be observed during capture.");
                        }
                    }
                }
                for video in &status.ready_video {
                    if status.video_unavailable.contains(&video.monitor) {
                        continue;
                    }
                    if video.software || status.phase == SessionPhase::Starting {
                        let [width, height] = video.encoded_size;
                        let [physical_width, physical_height] = video.physical_size;
                        let encoder = if video.software { "Software H.264 fallback" } else { "Hardware H.264" };
                        ui.label(format!("Monitor {} · {physical_width}×{physical_height} → {width}×{height} · {encoder} · {} FPS",
                            video.monitor.0, video.fps));
                    }
                }
                if let Some(coverage) = status.window_coverage {
                    if coverage.unselected != 0 {
                        ui.label(format!("The approved application has a window on {} unselected monitor(s).", coverage.unselected));
                    }
                    if coverage.incomplete {
                        ui.label("Some approved window locations could not be verified.");
                    }
                }
                Self::show_windows_status(ui, status);
                if let Some(operation) = status.input_unavailable {
                    ui.label(operation);
                    if let Some(report) = &status.input_report {
                        ui.label(report.to_string());
                    } else {
                        ui.label("Preparing input diagnostic report…");
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    if status.hotkeys_unavailable {
                        ui.label("Recorder hotkeys are unavailable. Use the Stop button.");
                    }
                    match status.last_hotkey {
                        Some(ControlOutcome::CheckpointRequested) => { ui.label("Checkpoint requested. Saving requires independent file confirmation."); }
                        Some(ControlOutcome::CheckpointUnavailable) => { ui.label("Checkpoint requests require active recording."); }
                        Some(ControlOutcome::PauseUnavailable) => { ui.label("Pause becomes available when recording starts."); }
                        _ => {}
                    }
                }
                for report in &status.video_reports {
                    ui.label(format!("Monitor {} · {}", report.monitor.0, report.outcome));
                }
                if status.video_reports.len() < status.video_unavailable.len() {
                    ui.label("Preparing video diagnostic report…");
                }
                if !stopping && status.phase == SessionPhase::Recording && ui.button("Pause recording").clicked() {
                    self.command_error = session.submit(SessionCommand::Pause(PauseReason::User)).err();
                }
                let can_resume = status.pause_reasons.iter()
                    .any(|reason| matches!(reason, PauseReason::User | PauseReason::LowDisk));
                if !stopping && !status.pause_reasons.is_empty()
                    && ui.add_enabled(can_resume, egui::Button::new("Resume recording")).clicked()
                {
                    self.command_error = session.submit(SessionCommand::Resume(PauseReason::User)).err();
                }
                let active = !stopping && matches!(status.phase, SessionPhase::Starting | SessionPhase::Recording | SessionPhase::Paused);
                if ui.add_enabled(active, egui::Button::new("Stop recording")).clicked() {
                    self.command_error = session.submit(SessionCommand::Stop).err();
                    *stop_requested |= self.command_error.is_none();
                }
                if let Some(error) = self.command_error {
                    ui.label(match error {
                        screen_fix_core::session::CommandError::Busy => "The controller is busy. Try the command again.",
                        screen_fix_core::session::CommandError::PauseUnavailable => "This pause command is unavailable.",
                        screen_fix_core::session::CommandError::ControllerUnavailable => "The session controller is unavailable.",
                    });
                }
                if ui.ctx().input(|input| input.viewport().close_requested()) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.command_error = session.submit(SessionCommand::Stop).err();
                    *stop_requested |= self.command_error.is_none();
                    self.close_after_recording = *stop_requested;
                }
            }
            RecordingState::Reporting { message, .. } => {
                ui.label(message.as_str());
                ui.label("Preparing diagnostic report…");
            }
            RecordingState::Complete(Ok(result)) => {
                ui.label(result.message.as_str());
            }
            RecordingState::Complete(Err(message)) => {
                ui.label(message.as_str());
                #[cfg(target_os = "windows")]
                if message.contains("ms-settings:optionalfeatures") {
                    ui.hyperlink_to("Open Windows Optional features", "ms-settings:optionalfeatures");
                }
                if ui.button("Copy diagnostics").clicked() {
                    ui.ctx().copy_text(message.clone());
                }
            }
        }
        if self.incomplete_pending {
            if ui.ctx().input(|input| input.viewport().close_requested()) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_after_recording = true;
            }
            ui.label("Review the incomplete recording before starting another attempt.");
            let acknowledge = if self.close_after_recording {
                "Acknowledge and exit"
            } else {
                "Acknowledge incomplete recording"
            };
            if matches!(self.state, RecordingState::Complete(_) | RecordingState::Reporting { .. })
                && ui.button(acknowledge).clicked()
            {
                #[cfg(target_os = "windows")]
                if let Some(window) = &mut self.native_window { window.clear_indicator(); }
                self.incomplete_pending = false;
                if matches!(self.state, RecordingState::Complete(_)) {
                    self.state = RecordingState::Idle;
                }
                if std::mem::take(&mut self.close_after_recording) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn show_windows_status(ui: &mut egui::Ui, status: &SessionStatus) {
        if let Some(windows) = &status.windows {
            let revision = windows.update_revision.map_or_else(|| "unknown".into(), |value| value.to_string());
            ui.collapsing("Windows capability", |ui| {
                ui.label(format!("{} · {} · {} · build {} · revision {revision}",
                    windows.product, windows.version.as_deref().unwrap_or("version unknown"), windows.architecture, windows.build));
                if windows.edition_id.is_none() {
                    ui.label("Windows edition could not be verified.");
                }
            });
            match windows.security {
                platform::SecuritySupport::Eligible => {}
                platform::SecuritySupport::Ended => { ui.label("Security updates have ended for this Windows build and edition."); }
                platform::SecuritySupport::EsuUnknown => { ui.label("Standard security updates have ended. Extended Security Updates enrollment is unknown."); }
                platform::SecuritySupport::Unknown => { ui.label("Security update support for this Windows build and edition is unknown."); }
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn start_issue(&self) -> Option<&'static str> {
        if self.native_window.is_none() || self.native_window_error.is_some() || self.indicator_error.is_some() {
            return Some("The recording indicator is unavailable. Restart Screen Fix before recording.");
        }
        if self.incomplete_pending {
            return Some("Acknowledge the incomplete recording result below before starting again.");
        }
        if self.approved_executable.is_none() {
            return Some("Approve an executable before starting.");
        }
        if self.setup_probe.is_some() {
            return Some("Wait for display and window discovery to finish.");
        }
        if self.selected.is_empty() || self.selected.iter().any(|entry| !self.displays.iter().any(|item| item.key == entry.key)) {
            return Some("Select one or more connected displays.");
        }
        if self.folder.as_os_str().is_empty() {
            return Some("Choose the output directory.");
        }
        if self.hotkey_probe.is_some() {
            return Some("Wait for recorder hotkey registration.");
        }
        let Some(hotkeys) = self.hotkeys.as_ref().filter(|hotkeys| hotkeys.available()) else {
            return Some("Register all recorder hotkeys before starting.");
        };
        if !hotkeys.ready_for_start() {
            return Some("Waiting for the previous attempt's hotkeys to clear.");
        }
        None
    }

    #[cfg(target_os = "windows")]
    fn refresh_inventory(&mut self, ctx: &egui::Context) {
        self.start_inventory(ctx, None);
    }

    #[cfg(target_os = "windows")]
    fn start_inventory(&mut self, ctx: &egui::Context, chosen: Option<PathBuf>) {
        if self.setup_probe.is_some() {
            return;
        }
        let approved = self.approved_executable.clone();
        let (send, receive) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        match thread::Builder::new().name("setup-inventory".into()).spawn(move || {
            let resolved_executable = chosen.map(|path| platform::resolve_executable(&path));
            let approved = resolved_executable.as_ref().and_then(|result| result.as_ref().ok()).cloned().or(approved);
            let displays = platform::displays().map_err(|error| error.to_string());
            let approved_windows = approved.as_ref().map(|identity| {
                platform::approved_windows(identity).map_err(|error| error.to_string())
            });
            let _ = send.try_send(SetupInventory { displays, approved_windows, resolved_executable });
            ctx.request_repaint();
        }) {
            Ok(_) => self.setup_probe = Some(receive),
            Err(error) => self.display_error = Some(format!("Could not start display discovery: {error}.")),
        }
    }

    #[cfg(target_os = "windows")]
    fn poll_inventory(&mut self, ctx: &egui::Context) {
        let Some(receive) = &self.setup_probe else { return; };
        let inventory = match receive.try_recv() {
            Ok(inventory) => inventory,
            Err(TryRecvError::Disconnected) => SetupInventory {
                displays: Err("The display and window inventory worker exited.".into()),
                approved_windows: self.approved_executable.as_ref().map(|_| Err("The window inventory worker exited.".into())),
                resolved_executable: None,
            },
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
        };
        if let Some(resolved) = inventory.resolved_executable {
            match resolved {
                Ok(executable) => self.approved_executable = Some(executable),
                Err(error) => self.state = recording_error(error.to_string(), error.into_diagnostic().with_output_root(self.folder.clone()), Some(ctx)),
            }
        }
        match inventory.displays {
            Ok(displays) => {
                for display in &displays {
                    let next = MonitorId(self.display_ids.len() as u32);
                    self.display_ids.entry(display.key.as_str().to_owned()).or_insert(next);
                }
                self.displays = displays;
                self.display_error = None;
            }
            Err(error) => {
                self.displays.clear();
                self.display_error = Some(error);
            }
        }
        if !self.manual_monitors {
            self.selected.clear();
            if let Some(Ok(windows)) = &inventory.approved_windows {
                self.selected.extend(windows.monitors.iter().filter_map(|key| {
                    let id = *self.display_ids.get(key.as_str())?;
                    self.displays.iter().any(|item| item.key == *key).then(|| SelectedMonitor { id, key: key.clone() })
                }).take(MAX_SELECTED_MONITORS));
            }
        }
        self.approved_windows = inventory.approved_windows;
        self.setup_probe = None;
    }

    #[cfg(target_os = "windows")]
    fn register_hotkeys(&mut self, ctx: &egui::Context) {
        if self.hotkey_probe.is_some() {
            return;
        }
        let previous = self.hotkeys.take();
        let bindings = self.hotkey_bindings;
        let (send, receive) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        match thread::Builder::new().name("setup-hotkeys".into()).spawn(move || {
            drop(previous);
            let result = platform::RecorderHotkeys::open(bindings).map_err(|error| error.to_string());
            let _ = send.try_send(result);
            ctx.request_repaint();
        }) {
            Ok(_) => self.hotkey_probe = Some(receive),
            Err(error) => self.hotkey_error = Some(format!("Could not start hotkey registration: {error}.")),
        }
    }

    #[cfg(target_os = "windows")]
    fn poll_hotkeys(&mut self, ctx: &egui::Context) {
        let Some(receive) = &self.hotkey_probe else { return; };
        let result = match receive.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Disconnected) => Err("The hotkey registration worker exited.".into()),
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
        };
        self.hotkey_probe = None;
        match result {
            Ok(hotkeys) => {
                self.hotkeys = Some(hotkeys);
                self.hotkey_error = None;
            }
            Err(error) => { self.hotkey_error = Some(error); }
        }
    }

    #[cfg(target_os = "windows")]
    fn hotkey_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let previous = self.hotkey_bindings;
        ui.collapsing("Recorder hotkeys", |ui| {
            ui.add_enabled_ui(self.hotkey_probe.is_none(), |ui| {
                for action in RecorderAction::ALL {
                    let mut function = self.hotkey_bindings.function(action);
                    egui::ComboBox::from_label(action.name())
                        .selected_text(format!("Ctrl+Alt+F{function}"))
                        .show_ui(ui, |ui| {
                            for choice in 1..=24 {
                                ui.selectable_value(&mut function, choice, format!("Ctrl+Alt+F{choice}"));
                            }
                        });
                    self.hotkey_bindings.set_function(action, function);
                }
            });
        });
        if RecorderAction::ALL.into_iter().any(|action| self.hotkey_bindings.function(action) == 12) {
            ui.label("Windows reserves F12 for debugger use. If registration fails, choose a different function key under Recorder hotkeys.");
        }
        let available = self.hotkeys.as_ref().is_some_and(platform::RecorderHotkeys::available);
        if self.hotkey_probe.is_some() {
            ui.label("Checking and registering recorder hotkeys…");
        } else {
            if !available {
                ui.label(self.hotkey_error.as_deref().unwrap_or("Recorder hotkeys are unavailable."));
            }
            if self.hotkey_bindings != previous || !available && ui.button("Retry hotkey registration").clicked() {
                self.register_hotkeys(ctx);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn start_recording(&mut self, ctx: &egui::Context, activation: Option<HotkeyActivation>) {
        if !matches!(self.state, RecordingState::Idle | RecordingState::Complete(_))
            || self.start_issue().is_some()
        {
            return;
        }
        let Some(hotkeys) = &self.hotkeys else { return };
        self.last_status = None;
        self.command_error = None;
        let clipboard = if self.record_clipboard { ClipboardMode::OptIn } else { ClipboardMode::Disabled };
        let approved = self.approved_executable.iter().cloned().collect();
        let config = ResolvedProfile::m1_stub(NonZeroU16::MIN, approved, clipboard, self.pace)
            .and_then(|profile| ResolvedCaptureConfig::new(
                self.folder.clone(),
                self.selected.clone(),
                profile.with_pen_hid(self.record_pen),
                Vec::new(),
                EffectiveCapabilities::awaiting_probe(),
            ));
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                self.state = recording_error(
                    error.to_string(), diagnostic(FailureComponent::Interface, "Resolve the recording configuration").with_output_root(self.folder.clone()), Some(ctx),
                );
                return;
            }
        };
        self.state = match platform::start_recording_with_hotkeys(config, hotkeys, activation) {
            Ok(session) => {
                if let Some(window) = &mut self.native_window {
                    if let Err(error) = window.observe(&session) {
                        self.indicator_error = Some(error.to_string());
                        self.indicator_diagnostic = Some(error.into_diagnostic().with_output_root(self.folder.clone()));
                        let _ = session.submit(SessionCommand::Stop);
                    }
                }
                self.record_clipboard = false;
                RecordingState::Running { session, status: SessionStatus::default(), stop_requested: false }
            }
            Err(error) => recording_error(error.to_string(), error.into_diagnostic().with_output_root(self.folder.clone()), Some(ctx)),
        };
    }
}

fn recording_error(
    message: String, snapshot: DiagnosticSnapshot, ctx: Option<&egui::Context>,
) -> RecordingState {
    let (sender, result) = mpsc::sync_channel(1);
    let issue = message.clone();
    let ctx = ctx.cloned();
    match std::thread::Builder::new().name("recording-error-report".into()).spawn(move || {
        let _ = sender.try_send(with_report(&issue, snapshot.write()));
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }) {
        Ok(_) => RecordingState::Reporting { message, result },
        Err(error) => RecordingState::Complete(Err(with_report(
            &message, ReportOutcome::Unavailable { native_code: error.raw_os_error() },
        ))),
    }
}

pub(crate) fn diagnostic(component: FailureComponent, operation: &'static str) -> DiagnosticSnapshot {
    DiagnosticSnapshot::new(Failure::new(component, operation, None))
}

pub(crate) fn with_report(message: &str, report: ReportOutcome) -> String {
    format!("{message}\n{report}")
}
