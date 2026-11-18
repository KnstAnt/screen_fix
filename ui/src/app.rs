use std::{collections::BTreeMap, num::NonZeroU16, path::PathBuf, thread, time::Duration};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;

use screen_fix_core::{
    AttemptStage, Remedy,
    config::{
        CapturePace, ClipboardMode, DisplayKey, ExecutableIdentity, MAX_SELECTED_MONITORS,
        ResolvedCaptureConfig, ResolvedProfile, SelectedMonitor,
    },
    package::diagnostics::{DiagnosticSnapshot, Failure, FailureComponent, ReportOutcome},
    platform,
    protocol::MonitorId,
    session::{
        CommandError, PauseReason, RecordingSource, SessionCommand, SessionHandle, SessionOutcome, SessionPhase,
        SessionResult, SessionStatus, SourceWait, StartupStep, StatusSummary,
    },
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use screen_fix_core::session::hotkey::{ControlOutcome, DEBUGGER_FUNCTION, HotkeyActivation, HotkeyBindings, RecorderAction};

/// The Recorder's state. Fields drop in declaration order: the running
/// attempt's handle (which submits Stop) before the indicator window and the
/// recorder hotkeys.
#[derive(Default)]
pub(crate) struct ScreenFix {
    state: RecordingState,
    last_status: Option<SessionStatus>,
    indicator: Indicator,
    close_after_recording: bool,
    incomplete_pending: bool,
    setup: Setup,
    command_error: Option<CommandError>,
    hotkeys: HotkeyState,
    #[cfg(feature = "fixture-save")]
    fixture: FixtureState,
}

/// The native recorder window that hosts the recording indicator, and the
/// indicator's failure.
#[derive(Default)]
struct Indicator {
    window: Option<platform::RecorderWindow>,
    window_error: Option<String>,
    error: Option<String>,
    /// The failure's diagnostic, until it is reported with the attempt's result.
    diagnostic: Option<DiagnosticSnapshot>,
}

impl Indicator {
    /// The indicator failure as reported with the attempt's result.
    fn issue(&self) -> &str {
        self.error.as_deref().unwrap_or("The recording indicator failed.")
    }
}

/// The setup form: what the next attempt records, and where.
#[derive(Default)]
struct Setup {
    record_clipboard: bool,
    record_pen: bool,
    approved_executable: Option<ExecutableIdentity>,
    approved_windows: Option<Result<platform::ApprovedWindows, String>>,
    probe: Option<Receiver<SetupInventory>>,
    display_error: Option<String>,
    manual_monitors: bool,
    displays: Vec<platform::Display>,
    display_ids: BTreeMap<DisplayKey, MonitorId>,
    selected: Vec<SelectedMonitor>,
    pace: CapturePace,
    folder: PathBuf,
}

impl Setup {
    /// `snapshot` with the chosen output folder.
    fn snapshot(&self, snapshot: DiagnosticSnapshot) -> DiagnosticSnapshot {
        snapshot.with_output_root(self.folder.clone())
    }

    /// A selected display is no longer connected.
    fn has_disconnected(&self) -> bool {
        self.selected.iter().any(|entry| !is_connected(&self.displays, &entry.key))
    }
}

#[derive(Default)]
struct HotkeyState {
    bindings: HotkeyBindings,
    registered: Option<platform::RecorderHotkeys>,
    error: Option<String>,
    probe: Option<Receiver<Result<platform::RecorderHotkeys, String>>>,
}

/// The pending reply and the last result of the fixture Save producer.
#[cfg(feature = "fixture-save")]
#[derive(Default)]
struct FixtureState {
    reply: Option<Receiver<screen_fix_core::capture::save::FixtureSaveResult>>,
    message: Option<String>,
}

struct SetupInventory {
    displays: Result<Vec<platform::Display>, String>,
    approved_windows: Option<Result<platform::ApprovedWindows, String>>,
    resolved_executable: Option<Result<ExecutableIdentity, screen_fix_core::CaptureError>>,
}

#[derive(Default)]
enum RecordingState {
    #[default]
    Idle,
    Running { session: SessionHandle, status: SessionStatus, stop_requested: bool },
    Reporting { message: String, result: Receiver<String>, remedy: Option<Remedy> },
    Complete(Result<SessionResult, Failed>),
}

/// A failed attempt's report and the action that can resolve it, if known.
struct Failed {
    report: String,
    remedy: Option<Remedy>,
}

impl eframe::App for ScreenFix {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Some(error) = self.indicator.window.as_mut().and_then(|window| window.take_indicator_error()) {
            self.fail_indicator(error.to_string(), error.into_diagnostic());
        }
        self.attach_window(frame);
        if self.indicator.window.is_none() && self.indicator.error.is_none()
            && matches!(self.state, RecordingState::Running { .. })
        {
            self.fail_indicator("The recording indicator window is unavailable.".into(),
                diagnostic(FailureComponent::Interface, "Maintain the recording indicator"));
        }
        self.poll_inventory(ctx);
        self.poll_hotkeys(ctx);
        match &mut self.state {
            RecordingState::Running { session, status, .. } => {
                if let Some(latest) = session.status() {
                    *status = latest;
                }
                match session.try_result() {
                    Ok(Ok(message)) => {
                        self.incomplete_pending = message.outcome == SessionOutcome::Incomplete;
                        self.last_status = Some(session.status().unwrap_or_else(|| status.clone()));
                        let report = self.indicator.diagnostic.take().map(|snapshot| {
                            recording_error(format!("{}\n{}", self.indicator.issue(), message.message), snapshot, None, ctx)
                        });
                        self.state = RecordingState::Complete(Ok(message));
                        if let Some(report) = report { self.state = report; }
                    }
                    Ok(Err(error)) => {
                        self.last_status = Some(session.status().unwrap_or_else(|| status.clone()));
                        let mut message = match error.attempt_stage() {
                            AttemptStage::Admitted => {
                                // An admitted attempt, standard or elevated (M1-01).
                                self.incomplete_pending = true;
                                match error.retained_recording() {
                                    Some(retained) if retained.manifest_confirmed => format!(
                                        "Recording failed after capture started. Saved the failed attempt with {} verified video frames.\n{}\n{error}",
                                        retained.verified_video_frames, retained.root.display()),
                                    _ => format!("Recording failed after capture started. The failed recording manifest could not be confirmed.\n{error}"),
                                }
                            }
                            AttemptStage::TargetRejected => format!("Recording did not start.\n{error}"),
                            AttemptStage::ElevatedUnavailable => format!("Elevated capture unavailable. Recording did not start.\n{error}"),
                            AttemptStage::BeforeAdmission => error.to_string(),
                        };
                        // The indicator failure is reported with the attempt's own failure.
                        if self.indicator.diagnostic.take().is_some() {
                            message.push_str(&format!("\n{}", self.indicator.issue()));
                        }
                        let remedy = error.remedy();
                        self.state = recording_error(message, self.setup.snapshot(error.into_diagnostic()), remedy, ctx);
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.last_status = Some(session.status().unwrap_or_else(|| status.clone()));
                        self.state = recording_error(
                            "The session controller exited without returning a result.".into(),
                            self.setup.snapshot(diagnostic(FailureComponent::Controller, "Read the session result")),
                            None, ctx,
                        );
                    }
                    Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                }
            }
            RecordingState::Reporting { message, result, remedy } => match result.try_recv() {
                Ok(report) => self.state = RecordingState::Complete(Err(Failed { report, remedy: *remedy })),
                Err(TryRecvError::Disconnected) => self.state = RecordingState::Complete(Err(Failed {
                    report: with_report(message, ReportOutcome::Unavailable { native_code: None }), remedy: *remedy,
                })),
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            },
            _ => {}
        }

        if self.close_after_recording && matches!(&self.state, RecordingState::Complete(Ok(result))
            if matches!(result.outcome, SessionOutcome::Complete | SessionOutcome::Cancelled)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.close_after_recording &= self.incomplete_pending || matches!(self.state, RecordingState::Running { .. });

        if matches!(self.state, RecordingState::Idle | RecordingState::Complete(_)) {
            if let Some(hotkeys) = self.hotkeys.registered.as_ref() {
                // The hotkey pump requests a repaint for a Start activation; it
                // does not report the previous attempt's hotkeys clearing.
                if let Some(activation) = hotkeys.take_start() {
                    self.start_recording(ctx, Some(activation));
                } else if !hotkeys.ready_for_start() {
                    ctx.request_repaint_after(Duration::from_millis(40));
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("screen_fix");
            egui::ScrollArea::vertical().show(ui, |ui| self.recording_panel(ui, ctx, frame));
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.indicator.window = None;
    }
}

impl ScreenFix {
    pub(crate) fn new(creation: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        app.attach_window(creation);
        app.refresh_inventory(&creation.egui_ctx);
        app.register_hotkeys(&creation.egui_ctx);
        app
    }

    /// Keep an indicator failure for the attempt's report and stop the
    /// running attempt, if any.
    fn fail_indicator(&mut self, message: String, snapshot: DiagnosticSnapshot) {
        self.indicator.error = Some(message);
        self.indicator.diagnostic = Some(self.setup.snapshot(snapshot));
        if let RecordingState::Running { session, .. } = &self.state {
            let _ = session.submit(SessionCommand::Stop);
        }
    }

    fn attach_window(&mut self, owner: &impl HasWindowHandle) {
        let Ok(handle) = owner.window_handle() else {
            self.indicator.window = None;
            self.indicator.window_error = Some("The native recorder window is unavailable.".into());
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            self.indicator.window = None;
            self.indicator.window_error = Some("The recorder viewport did not provide a Windows window.".into());
            return;
        };
        let id = screen_fix_core::protocol::WindowId(handle.hwnd.get() as usize as u64);
        if self.indicator.window.as_ref().is_some_and(|window| window.id() == id) {
            self.indicator.window_error = None;
            return;
        }
        self.indicator.window = None;
        match platform::attach_recorder_window(id) {
            Ok(mut window) => {
                let observed = match &self.state {
                    RecordingState::Running { session, .. } => window.observe(session),
                    _ => Ok(()),
                };
                self.indicator.window = Some(window);
                self.indicator.window_error = None;
                if let Err(error) = observed {
                    self.fail_indicator(error.to_string(), error.into_diagnostic());
                }
            }
            Err(error) => self.indicator.window_error = Some(error.to_string()),
        }
    }

    fn recording_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let running = self.incomplete_pending || matches!(self.state, RecordingState::Running { .. } | RecordingState::Reporting { .. });
        if let Some(error) = self.indicator.error.as_ref().or(self.indicator.window_error.as_ref()) {
            ui.label(error);
        }
        if !running {
            self.setup_panel(ui, ctx, frame);
        }
        match &self.state {
            RecordingState::Running { status, stop_requested, .. } => {
                status_panel(ui, status, *stop_requested || status.phase == SessionPhase::Stopping);
            }
            _ => {
                if let Some(status) = &self.last_status {
                    show_windows_status(ui, status);
                }
            }
        }
        self.session_controls(ui);
        match &self.state {
            RecordingState::Idle | RecordingState::Running { .. } => {}
            RecordingState::Reporting { message, .. } => {
                ui.label(message.as_str());
                ui.label("Preparing diagnostic report…");
            }
            RecordingState::Complete(Ok(result)) => {
                ui.label(result.message.as_str());
            }
            RecordingState::Complete(Err(failed)) => {
                ui.label(failed.report.as_str());
                if let Some(Remedy::InstallMediaFeaturePack) = failed.remedy {
                    ui.hyperlink_to("Open Windows Optional features", "ms-settings:optionalfeatures");
                }
                if ui.button("Copy diagnostics").clicked() {
                    ui.ctx().copy_text(failed.report.clone());
                }
            }
        }
        self.acknowledge_panel(ui);
    }

    /// The setup form and Start, shown while no attempt runs.
    fn setup_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame: &mut eframe::Frame) {
        ui.label("Capture stays on the selected displays. Recordings are stored locally.");
        ui.label("Displays");
        for item in &self.setup.displays {
            let mut selected = self.setup.selected.iter().any(|entry| entry.key == item.key);
            if ui.add_enabled(selected || self.setup.selected.len() < MAX_SELECTED_MONITORS,
                egui::Checkbox::new(&mut selected, &item.label)).changed()
            {
                self.setup.manual_monitors = true;
                if selected {
                    self.setup.selected.push(SelectedMonitor {
                        id: self.setup.display_ids[&item.key],
                        key: item.key.clone(),
                    });
                } else {
                    self.setup.selected.retain(|entry| entry.key != item.key);
                }
            }
        }
        for missing in self.setup.selected.iter().filter(|entry| !is_connected(&self.setup.displays, &entry.key)) {
            ui.label(format!("Selected monitor {} is disconnected.", missing.id.0));
        }
        if self.setup.has_disconnected() && ui.button("Remove disconnected selections").clicked() {
            self.setup.manual_monitors = true;
            let displays = &self.setup.displays;
            self.setup.selected.retain(|entry| is_connected(displays, &entry.key));
        }
        egui::ComboBox::from_label("Capture rate")
            .selected_text(match self.setup.pace { CapturePace::Fps30 => "30 FPS", CapturePace::Fps60 => "60 FPS pilot" })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.setup.pace, CapturePace::Fps30, "30 FPS");
                ui.selectable_value(&mut self.setup.pace, CapturePace::Fps60, "60 FPS pilot");
            });
        if let Some(error) = &self.setup.display_error {
            ui.label(error);
        }
        if ui.add_enabled(self.setup.probe.is_none(), egui::Button::new("Refresh displays and windows")).clicked() {
            self.refresh_inventory(ctx);
        }
        if self.setup.probe.is_some() {
            ui.label("Checking displays and approved windows…");
        }
        ui.label("Output directory");
        ui.horizontal(|ui| {
            let mut path = self.setup.folder.to_string_lossy().into_owned();
            let field = egui::TextEdit::singleline(&mut path)
                .hint_text("Choose a folder or paste its path");
            if ui.add_sized([(ui.available_width() - 82.0).max(40.0), 22.0], field).changed() {
                self.setup.folder = PathBuf::from(path.trim());
            }
            if ui.button("Browse…").clicked() {
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Choose an output folder")
                    .set_parent(frame);
                if !self.setup.folder.as_os_str().is_empty() {
                    dialog = dialog.set_directory(&self.setup.folder);
                }
                if let Some(folder) = dialog.pick_folder() {
                    self.setup.folder = folder;
                }
            }
        });
        ui.checkbox(&mut self.setup.record_clipboard, "Log clipboard changes");
        ui.checkbox(&mut self.setup.record_pen, "Record pen input");
        ui.horizontal(|ui| {
            if ui.add_enabled(self.setup.probe.is_none(), egui::Button::new("Approve an executable…")).clicked() {
                let chosen = rfd::FileDialog::new()
                    .set_title("Choose the application whose input may be recorded")
                    .set_parent(frame)
                    .add_filter("Executables", &["exe"])
                    .pick_file();
                if let Some(chosen) = chosen {
                    self.start_inventory(ctx, Some(chosen));
                }
            }
            if self.setup.approved_executable.is_some()
                && ui.add_enabled(self.setup.probe.is_none(), egui::Button::new("Clear approval")).clicked()
            {
                self.setup.approved_executable = None;
                self.setup.approved_windows = None;
                if !self.setup.manual_monitors {
                    self.setup.selected.clear();
                }
            }
        });
        ui.label(self.setup.approved_executable.as_ref().map_or(
            "Video and input recording require an approved executable in the foreground.", |entry| entry.executable(),
        ));
        match &self.setup.approved_windows {
            Some(Ok(windows)) => {
                let unselected = windows.monitors.iter()
                    .filter(|key| !self.setup.selected.iter().any(|entry| &entry.key == *key)).count();
                coverage_lines(ui, unselected, windows.evidence_incomplete);
                if windows.monitors.is_empty() {
                    ui.label("No visible approved window was found on a connected monitor.");
                }
            }
            Some(Err(error)) => { ui.label(format!("Approved window locations unknown. {error}")); }
            None => {}
        }
        if self.setup.approved_executable.is_some() && self.setup.manual_monitors
            && ui.add_enabled(self.setup.probe.is_none(), egui::Button::new("Use displays with approved windows")).clicked()
        {
            self.setup.manual_monitors = false;
            self.refresh_inventory(ctx);
        }
        self.hotkey_settings(ui, ctx);
        ui.collapsing("M1 recording scope", |ui| {
            ui.label("App-pack version, adapter and shortcut checks, task duration, start file, goal and task consent are not part of M1.");
            ui.label("The 60 FPS profile is a sculpt and paint pilot. Recording rates depend on the per-monitor encoder check.");
        });
        let issue = self.start_issue();
        if let Some(issue) = issue {
            ui.label(issue);
        }
        let pause_key = self.hotkeys.bindings.function(RecorderAction::TogglePause);
        ui.label(format!("Before typing private information in the approved application, pause recording with Ctrl+Alt+F{pause_key}. Resume only when the private information is no longer visible."));
        ui.label("M1 does not hide the recorder or other windows from captured displays. The recording indicator can appear in video and stills.");
        if ui.add_enabled(issue.is_none(), egui::Button::new("Start recording")).clicked() {
            self.start_recording(ctx, None);
        }
    }

    /// The running attempt's commands: fixture saves, Pause, Resume and Stop.
    fn session_controls(&mut self, ui: &mut egui::Ui) {
        let RecordingState::Running { session, status, stop_requested } = &mut self.state else {
            return;
        };
        let stopping = *stop_requested || status.phase == SessionPhase::Stopping;
        // Developer-only trusted fixture Save producer (M1-08). It is compiled into the
        // Recorder and calls only the public confirm_fixture_save seam with the
        // approved executable; the monitors follow the approved window geometry.
        #[cfg(feature = "fixture-save")]
        {
            use screen_fix_core::capture::save::{FixtureSave, SaveKind};
            let fixture = &mut self.fixture;
            if let Some(reply) = &fixture.reply && let Ok(result) = reply.try_recv() {
                fixture.message = Some(match result {
                    Ok(id) => format!("Fixture save {} confirmed.", id.0),
                    Err(error) => format!("Fixture save not confirmed: {error:?}."),
                });
                fixture.reply = None;
            }
            if !stopping && status.phase == SessionPhase::Recording && let Some(executable) = &self.setup.approved_executable {
                for (label, kind) in [("Confirm fixture save", SaveKind::Checkpoint), ("Confirm final fixture save", SaveKind::Final)] {
                    if ui.add_enabled(fixture.reply.is_none(), egui::Button::new(label)).clicked() {
                        let confirmation = FixtureSave { kind, executable: executable.clone(), affected_monitors: None };
                        match session.confirm_fixture_save(confirmation) {
                            Ok(reply) => {
                                fixture.reply = Some(reply);
                                fixture.message = None;
                            }
                            Err(error) => fixture.message = Some(format!("Fixture save not confirmed: {error:?}.")),
                        }
                    }
                }
            }
            if let Some(message) = &fixture.message {
                ui.label(message.as_str());
            }
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
                CommandError::Busy => "The controller is busy. Try the command again.",
                CommandError::PauseUnavailable => "This pause command is unavailable.",
                CommandError::ControllerUnavailable => "The session controller is unavailable.",
            });
        }
        if ui.ctx().input(|input| input.viewport().close_requested()) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.command_error = session.submit(SessionCommand::Stop).err();
            *stop_requested |= self.command_error.is_none();
            self.close_after_recording = *stop_requested;
        }
    }

    /// An incomplete recording must be acknowledged before the next attempt.
    fn acknowledge_panel(&mut self, ui: &mut egui::Ui) {
        if !self.incomplete_pending {
            return;
        }
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
            if let Some(window) = &mut self.indicator.window { window.clear_indicator(); }
            self.incomplete_pending = false;
            if matches!(self.state, RecordingState::Complete(_)) {
                self.state = RecordingState::Idle;
            }
            if std::mem::take(&mut self.close_after_recording) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn start_issue(&self) -> Option<&'static str> {
        let indicator = &self.indicator;
        if indicator.window.is_none() || indicator.window_error.is_some() || indicator.error.is_some() {
            return Some("The recording indicator is unavailable. Restart Screen Fix before recording.");
        }
        if self.incomplete_pending {
            return Some("Acknowledge the incomplete recording result below before starting again.");
        }
        let setup = &self.setup;
        if setup.approved_executable.is_none() {
            return Some("Approve an executable before starting.");
        }
        if setup.probe.is_some() {
            return Some("Wait for display and window discovery to finish.");
        }
        if setup.selected.is_empty() || setup.has_disconnected() {
            return Some("Select one or more connected displays.");
        }
        if setup.folder.as_os_str().is_empty() {
            return Some("Choose the output directory.");
        }
        if self.hotkeys.probe.is_some() {
            return Some("Wait for recorder hotkey registration.");
        }
        let Some(hotkeys) = self.hotkeys.registered.as_ref().filter(|hotkeys| hotkeys.available()) else {
            return Some("Register all recorder hotkeys before starting.");
        };
        if !hotkeys.ready_for_start() {
            return Some("Waiting for the previous attempt's hotkeys to clear.");
        }
        None
    }

    fn refresh_inventory(&mut self, ctx: &egui::Context) {
        self.start_inventory(ctx, None);
    }

    fn start_inventory(&mut self, ctx: &egui::Context, chosen: Option<PathBuf>) {
        if self.setup.probe.is_some() {
            return;
        }
        let approved = self.setup.approved_executable.clone();
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
            Ok(_) => self.setup.probe = Some(receive),
            Err(error) => self.setup.display_error = Some(format!("Could not start display discovery: {error}.")),
        }
    }

    fn poll_inventory(&mut self, ctx: &egui::Context) {
        let Some(receive) = &self.setup.probe else { return; };
        let inventory = match receive.try_recv() {
            Ok(inventory) => inventory,
            Err(TryRecvError::Disconnected) => SetupInventory {
                displays: Err("The display and window inventory worker exited.".into()),
                approved_windows: self.setup.approved_executable.as_ref().map(|_| Err("The window inventory worker exited.".into())),
                resolved_executable: None,
            },
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
        };
        if let Some(resolved) = inventory.resolved_executable {
            match resolved {
                Ok(executable) => self.setup.approved_executable = Some(executable),
                Err(error) => {
                    let remedy = error.remedy();
                    self.state = recording_error(error.to_string(), self.setup.snapshot(error.into_diagnostic()), remedy, ctx);
                }
            }
        }
        let setup = &mut self.setup;
        match inventory.displays {
            Ok(displays) => {
                for display in &displays {
                    let next = MonitorId(u32::try_from(setup.display_ids.len()).unwrap_or(u32::MAX));
                    setup.display_ids.entry(display.key.clone()).or_insert(next);
                }
                setup.displays = displays;
                setup.display_error = None;
            }
            Err(error) => {
                setup.displays.clear();
                setup.display_error = Some(error);
            }
        }
        if !setup.manual_monitors {
            setup.selected.clear();
            if let Some(Ok(windows)) = &inventory.approved_windows {
                setup.selected.extend(windows.monitors.iter().filter_map(|key| {
                    let id = *setup.display_ids.get(key)?;
                    is_connected(&setup.displays, key).then(|| SelectedMonitor { id, key: key.clone() })
                }).take(MAX_SELECTED_MONITORS));
            }
        }
        setup.approved_windows = inventory.approved_windows;
        setup.probe = None;
    }

    fn register_hotkeys(&mut self, ctx: &egui::Context) {
        if self.hotkeys.probe.is_some() {
            return;
        }
        let previous = self.hotkeys.registered.take();
        let bindings = self.hotkeys.bindings;
        let (send, receive) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        match thread::Builder::new().name("setup-hotkeys".into()).spawn(move || {
            drop(previous);
            let repaint = ctx.clone();
            let result = platform::RecorderHotkeys::open(bindings, std::sync::Arc::new(move || repaint.request_repaint()))
                .map_err(|error| error.to_string());
            let _ = send.try_send(result);
            ctx.request_repaint();
        }) {
            Ok(_) => self.hotkeys.probe = Some(receive),
            Err(error) => self.hotkeys.error = Some(format!("Could not start hotkey registration: {error}.")),
        }
    }

    fn poll_hotkeys(&mut self, ctx: &egui::Context) {
        let Some(receive) = &self.hotkeys.probe else { return; };
        let result = match receive.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Disconnected) => Err("The hotkey registration worker exited.".into()),
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
        };
        self.hotkeys.probe = None;
        match result {
            Ok(hotkeys) => {
                self.hotkeys.registered = Some(hotkeys);
                self.hotkeys.error = None;
            }
            Err(error) => { self.hotkeys.error = Some(error); }
        }
    }

    fn hotkey_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let previous = self.hotkeys.bindings;
        ui.collapsing("Recorder hotkeys", |ui| {
            ui.add_enabled_ui(self.hotkeys.probe.is_none(), |ui| {
                for action in RecorderAction::ALL {
                    let mut function = self.hotkeys.bindings.function(action);
                    egui::ComboBox::from_label(action.name())
                        .selected_text(format!("Ctrl+Alt+F{function}"))
                        .show_ui(ui, |ui| {
                            for choice in 1..=24 {
                                ui.selectable_value(&mut function, choice, format!("Ctrl+Alt+F{choice}"));
                            }
                        });
                    self.hotkeys.bindings.set_function(action, function);
                }
            });
        });
        if RecorderAction::ALL.into_iter().any(|action| self.hotkeys.bindings.function(action) == DEBUGGER_FUNCTION) {
            ui.label("Windows reserves F12 for debugger use. If registration fails, choose a different function key under Recorder hotkeys.");
        }
        let available = self.hotkeys.registered.as_ref().is_some_and(platform::RecorderHotkeys::available);
        if self.hotkeys.probe.is_some() {
            ui.label("Checking and registering recorder hotkeys…");
        } else {
            if !available {
                ui.label(self.hotkeys.error.as_deref().unwrap_or("Recorder hotkeys are unavailable."));
            }
            if self.hotkeys.bindings != previous || !available && ui.button("Retry hotkey registration").clicked() {
                self.register_hotkeys(ctx);
            }
        }
    }

    fn start_recording(&mut self, ctx: &egui::Context, activation: Option<HotkeyActivation>) {
        if !matches!(self.state, RecordingState::Idle | RecordingState::Complete(_))
            || self.start_issue().is_some()
        {
            return;
        }
        let Some(hotkeys) = &self.hotkeys.registered else { return };
        self.last_status = None;
        self.command_error = None;
        let setup = &self.setup;
        let clipboard = if setup.record_clipboard { ClipboardMode::OptIn } else { ClipboardMode::Disabled };
        let approved = setup.approved_executable.iter().cloned().collect();
        let config = ResolvedProfile::m1_stub(NonZeroU16::MIN, approved, clipboard, setup.pace)
            .and_then(|profile| ResolvedCaptureConfig::new(
                setup.folder.clone(),
                setup.selected.clone(),
                profile.with_pen_hid(setup.record_pen),
                Vec::new(),
            ));
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                self.state = recording_error(
                    error.to_string(),
                    self.setup.snapshot(diagnostic(FailureComponent::Interface, "Resolve the recording configuration")),
                    None, ctx,
                );
                return;
            }
        };
        match platform::start_recording_with_hotkeys(config, hotkeys, activation) {
            Ok(session) => {
                let observed = self.indicator.window.as_mut().map_or(Ok(()), |window| window.observe(&session));
                self.setup.record_clipboard = false;
                self.state = RecordingState::Running { session, status: SessionStatus::default(), stop_requested: false };
                if let Err(error) = observed {
                    self.fail_indicator(error.to_string(), error.into_diagnostic());
                }
            }
            Err(error) => {
                let remedy = error.remedy();
                self.state = recording_error(error.to_string(), self.setup.snapshot(error.into_diagnostic()), remedy, ctx);
            }
        }
    }
}

/// The running attempt's status lines.
fn status_panel(ui: &mut egui::Ui, status: &SessionStatus, stopping: bool) {
    let summary = status.summary();
    match summary {
        StatusSummary::Starting(wait) if !stopping => {
            let blocked: Vec<_> = status.pause_reasons.iter().copied()
                .filter(|reason| matches!(reason, PauseReason::LockScreen | PauseReason::SecureDesktop | PauseReason::Sleep))
                .map(pause_label)
                .collect();
            if !blocked.is_empty() {
                ui.label(format!("Countdown waiting: {}. It restarts when this clears.", blocked.join(", ")));
            } else if let Some(seconds) = status.countdown_seconds {
                if seconds == 0 {
                    ui.label("Waiting for Windows to become ready.");
                } else {
                    ui.label(format!("Switch to the approved application. Recording starts in {seconds}…"));
                }
            } else if wait.is_none() {
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
        StatusSummary::Recording(recording) if !stopping => {
            ui.label(if recording.input_lost {
                "Recording · input unavailable"
            } else {
                match recording.source {
                    RecordingSource::Standard => "Recording · standard capture",
                    RecordingSource::Enhanced => "Recording · enhanced capture",
                    RecordingSource::Activating => "Waiting for certified elevated capture…",
                    RecordingSource::FailedPrefix => "Closing the failed recording prefix…",
                    RecordingSource::Other => "Recording",
                }
            });
            if recording.video_lost {
                ui.label(format!("Video unavailable on {} monitor(s)", status.video_unavailable.len()));
            }
            degraded_line(ui, &status.degraded);
        }
        StatusSummary::Paused(_) if !stopping => {
            ui.label("Recording paused");
            degraded_line(ui, &status.degraded);
            for reason in &status.pause_reasons {
                ui.label(pause_label(*reason));
            }
        }
        StatusSummary::Stopping if status.final_publication_pending => {
            ui.label("Final publication pending. Waiting for Windows…");
        }
        StatusSummary::Failed => { ui.label("Recording failed. Closing recording resources…"); }
        StatusSummary::Finished { .. } => { ui.label("Recording finished"); }
        _ => { ui.label("Stopping and finalizing the recording…"); }
    }
    if !stopping && let StatusSummary::Starting(Some(wait)) | StatusSummary::Paused(Some(wait)) = summary {
        ui.label(match wait {
            SourceWait::Consent => "Waiting for Windows elevation consent…",
            SourceWait::Readiness => "Verifying the elevated capture source…",
        });
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
        coverage_lines(ui, coverage.unselected, coverage.incomplete);
    }
    show_windows_status(ui, status);
    if let Some(operation) = status.input_unavailable {
        ui.label(operation);
        if let Some(report) = &status.input_report {
            ui.label(report.to_string());
        } else {
            ui.label("Preparing input diagnostic report…");
        }
    }
    if status.hotkeys_unavailable {
        ui.label("Recorder hotkeys are unavailable. Use the Stop button.");
    }
    match status.last_hotkey {
        Some(ControlOutcome::CheckpointRequested) => { ui.label("Checkpoint requested. Saving requires independent file confirmation."); }
        Some(ControlOutcome::CheckpointUnavailable) => { ui.label("Checkpoint requests require active recording."); }
        Some(ControlOutcome::PauseUnavailable) => { ui.label("Pause becomes available when recording starts."); }
        _ => {}
    }
    for report in &status.video_reports {
        ui.label(format!("Monitor {} · {}", report.monitor.0, report.outcome));
    }
    if status.video_reports.len() < status.video_unavailable.len() {
        ui.label("Preparing video diagnostic report…");
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

fn pause_label(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::User => "Paused manually",
        PauseReason::LockScreen => "Windows session locked",
        PauseReason::Sleep => "System suspended",
        PauseReason::SecureDesktop => "Input desktop unavailable",
        PauseReason::LowDisk => "Disk space unavailable or below the recording limit",
        PauseReason::ElevationTransition => "Switching capture source",
    }
}

/// The approved application's windows outside the selected monitors.
fn coverage_lines(ui: &mut egui::Ui, unselected: usize, incomplete: bool) {
    if unselected != 0 {
        ui.label(format!("The approved application has a window on {unselected} unselected monitor(s)."));
    }
    if incomplete {
        ui.label("Some approved window locations could not be verified.");
    }
}

fn is_connected(displays: &[platform::Display], key: &DisplayKey) -> bool {
    displays.iter().any(|item| item.key == *key)
}

/// Activities that stopped while the attempt continues (§6.2 class L).
fn degraded_line(ui: &mut egui::Ui, degraded: &[screen_fix_core::session::DegradedActivity]) {
    if !degraded.is_empty() {
        let activities: Vec<_> = degraded.iter().map(|activity| activity.label()).collect();
        ui.label(format!("Recording degraded: {}", activities.join(", ")));
    }
}

fn recording_error(
    message: String, snapshot: DiagnosticSnapshot, remedy: Option<Remedy>, ctx: &egui::Context,
) -> RecordingState {
    let (sender, result) = mpsc::sync_channel(1);
    let issue = message.clone();
    let ctx = ctx.clone();
    match std::thread::Builder::new().name("recording-error-report".into()).spawn(move || {
        let _ = sender.try_send(with_report(&issue, snapshot.write()));
        ctx.request_repaint();
    }) {
        Ok(_) => RecordingState::Reporting { message, result, remedy },
        Err(error) => RecordingState::Complete(Err(Failed {
            report: with_report(&message, ReportOutcome::Unavailable { native_code: error.raw_os_error() }),
            remedy,
        })),
    }
}

pub(crate) fn diagnostic(component: FailureComponent, operation: &'static str) -> DiagnosticSnapshot {
    DiagnosticSnapshot::new(Failure::new(component, operation, None))
}

pub(crate) fn with_report(message: &str, report: ReportOutcome) -> String {
    format!("{message}\n{report}")
}
