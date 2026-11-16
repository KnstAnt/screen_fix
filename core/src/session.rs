//! Commands, status, and boundary values for one recording attempt.

use std::sync::mpsc;

pub mod hotkey;

use crate::{
    CaptureError,
    package::diagnostics::ReportOutcome,
    protocol::{AdmissionEpoch, ClockOrigin, MonitorId, QpcTick, SessionTimeUs},
};

#[cfg(target_os = "windows")]
pub(crate) mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Starting,
    Recording,
    Paused,
    Stopping,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    Complete,
    Cancelled,
    Incomplete,
}

#[derive(Debug)]
pub struct SessionResult {
    pub outcome: SessionOutcome,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    User,
    LockScreen,
    Sleep,
    SecureDesktop,
    LowDisk,
}

/// Only the source that owns a pause condition may clear that reason. In
/// particular, a user Resume clears User and requests a new disk-space check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    Pause(PauseReason),
    Resume(PauseReason),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    PauseUnavailable,
    Busy,
    ControllerUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdmissionBoundary {
    pub epoch: AdmissionEpoch,
    pub qpc: QpcTick,
    pub time: SessionTimeUs,
}

#[derive(Debug, Clone)]
pub struct VideoReport {
    pub monitor: MonitorId,
    pub outcome: ReportOutcome,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct DiskPreflight {
    pub available_bytes: u64,
    pub minimum_bytes: u64,
    /// M1 has no task duration from which to calculate this reserve.
    pub duration_reserve_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub enum StartupStep {
    Windows,
    ApprovedApplication,
    OutputSpace,
    MediaFoundation,
    DisplaysAndEncoders,
    InputHelper,
    Ready,
    VerifyingAdmission,
}

#[derive(Debug, Clone, Copy)]
pub struct WindowCoverage {
    pub unselected: usize,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ReadyVideo {
    pub monitor: MonitorId,
    pub physical_size: [u32; 2],
    pub encoded_size: [u32; 2],
    pub fps: u32,
    pub software: bool,
}

#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub phase: SessionPhase,
    pub outcome: Option<SessionOutcome>,
    /// Stop reached its deadline after the owner was authorized to publish Complete.
    pub final_publication_pending: bool,
    pub countdown_seconds: Option<u8>,
    pub startup: StartupStep,
    pub ready_video: Vec<ReadyVideo>,
    pub input_ready: Option<crate::protocol::ipc::InputRegistration>,
    pub windows: Option<crate::platform::WindowsEvidence>,
    pub window_coverage: Option<WindowCoverage>,
    pub disk_available_bytes: Option<u64>,
    pub pause_reasons: Vec<PauseReason>,
    pub clock: Option<ClockOrigin>,
    pub opened: Option<AdmissionBoundary>,
    pub closed: Option<AdmissionBoundary>,
    pub stop: Option<AdmissionBoundary>,
    pub video_unavailable: Vec<MonitorId>,
    pub video_reports: Vec<VideoReport>,
    pub input_unavailable: Option<&'static str>,
    pub input_report: Option<ReportOutcome>,
    pub hotkeys_unavailable: bool,
    pub last_hotkey: Option<hotkey::ControlOutcome>,
}

impl Default for SessionStatus {
    fn default() -> Self {
        Self {
            phase: SessionPhase::Starting,
            outcome: None,
            final_publication_pending: false,
            countdown_seconds: None,
            startup: StartupStep::Windows,
            ready_video: Vec::new(),
            input_ready: None,
            windows: None,
            window_coverage: None,
            disk_available_bytes: None,
            pause_reasons: Vec::new(),
            clock: None,
            opened: None,
            closed: None,
            stop: None,
            video_unavailable: Vec::new(),
            video_reports: Vec::new(),
            input_unavailable: None,
            input_report: None,
            hotkeys_unavailable: false,
            last_hotkey: None,
        }
    }
}

pub struct SessionHandle {
    result: mpsc::Receiver<Result<SessionResult, CaptureError>>,
    #[cfg(target_os = "windows")]
    control: windows::Control,
}

impl SessionHandle {
    #[cfg(target_os = "windows")]
    pub(crate) fn indicator_control(&self) -> windows::Control {
        self.control.clone()
    }
    /// A short-lived status lock can be busy. Keep displaying the last snapshot.
    pub fn status(&self) -> Option<SessionStatus> {
        #[cfg(target_os = "windows")]
        { self.control.status() }
        #[cfg(not(target_os = "windows"))]
        { None }
    }

    pub fn submit(&self, command: SessionCommand) -> Result<(), CommandError> {
        #[cfg(target_os = "windows")]
        { self.control.submit(command) }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = command;
            Err(CommandError::ControllerUnavailable)
        }
    }

    /// Submit the fixture's independent confirmation to this recording attempt.
    /// The reply acknowledges routing, while stills.jsonl records image outcomes.
    pub fn confirm_fixture_save(
        &self, confirmation: crate::capture::save::FixtureSave,
    ) -> Result<mpsc::Receiver<crate::capture::save::FixtureSaveResult>, crate::capture::save::FixtureSaveError> {
        #[cfg(target_os = "windows")]
        { self.control.confirm_fixture_save(confirmation) }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = confirmation;
            Err(crate::capture::save::FixtureSaveError::ControllerUnavailable)
        }
    }

    pub fn try_result(&self) -> Result<Result<SessionResult, CaptureError>, mpsc::TryRecvError> {
        self.result.try_recv()
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = self.submit(SessionCommand::Stop);
    }
}
