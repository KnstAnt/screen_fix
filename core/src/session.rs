//! Commands, status, and boundary values for one recording attempt.

use std::sync::mpsc;

pub mod hotkey;

use crate::{
    CaptureError,
    package::diagnostics::ReportOutcome,
    protocol::{AdmissionEpoch, ClockOrigin, MonitorId, QpcTick, SessionTimeUs, TokenIdentityCheck, TokenIdentityFailure},
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
    ElevationTransition,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStage {
    Foreground,
    Consent,
    Readiness,
    Activation,
    Capture,
    Cleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFailureReason {
    DeploymentRequired,
    InstalledPath,
    InstalledPermissions,
    InstalledSignature,
    ReleaseMismatch,
    ProcessIdentity,
    TokenIdentity,
    Arguments,
    Busy,
    ConsentDenied,
    ConsentLaunch,
    ConsentDeadline,
    ReadinessDeadline,
    CleanupDeadline,
    Cancelled,
    PeerExited,
    PipeSecurity,
    PipeConnection,
    PipePeer,
    Transport,
    Protocol,
    Cryptography,
    Supervisor,
    LeaseExpired,
    Witness,
    TargetChanged,
    TargetAccess,
    ScopeClosed,
    RequiredRegistration,
    StandardHelperUnavailable,
    RequiredHook,
    NativeProducer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceFailure {
    pub stage: SourceStage,
    pub reason: SourceFailureReason,
    pub admitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<crate::protocol::ipc::HelperFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_identity: Option<TokenIdentityFailure>,
}

impl SourceFailure {
    pub(crate) fn without_report_detail(mut self) -> Self {
        self.token_identity = None;
        if let Some(native) = &mut self.native {
            native.raw_input_registration = None;
        }
        self
    }

    pub fn guidance(self) -> &'static str {
        use SourceFailureReason::*;
        match self.reason {
            DeploymentRequired | InstalledPath | InstalledPermissions | InstalledSignature | ReleaseMismatch =>
                "Install or repair the matching signed Recorder and collector using the official machine-wide installer.",
            TokenIdentity => match self.token_identity.map(|detail| detail.check) {
                Some(TokenIdentityCheck::ProtectedProcess) =>
                    "The target application uses Windows process protection. Select a supported application.",
                Some(TokenIdentityCheck::DifferentUser) =>
                    "Run Screen Fix and the approved application under the same signed-in Windows account.",
                Some(TokenIdentityCheck::DifferentLogon) =>
                    "Open Screen Fix and the approved application from the same interactive Windows sign-in.",
                Some(TokenIdentityCheck::SystemAccount) =>
                    "Use a signed-in Windows user account for Screen Fix and the approved application.",
                Some(TokenIdentityCheck::UnsupportedIntegrity) =>
                    "The approved application runs at a higher integrity level than Screen Fix. Open the approved application normally from the interactive desktop and retry.",
                Some(TokenIdentityCheck::NoInteractiveSession | TokenIdentityCheck::DifferentSession | TokenIdentityCheck::NotInteractive) =>
                    "Run Screen Fix and the approved application in the same unlocked interactive Windows session.",
                Some(TokenIdentityCheck::UnsupportedSplitPair | TokenIdentityCheck::LinkedLogonMismatch) =>
                    "Windows did not report a supported linked token pair. Reopen the approved application in the same interactive sign-in and retry.",
                _ => "Windows could not verify the process token. Check the diagnostic report for the failed verification step.",
            },
            ConsentDenied =>
                "Windows elevation consent was declined. Start again and approve the prompt if you want to record the elevated application.",
            ConsentDeadline =>
                "Windows elevation consent timed out. Start again and answer the Windows prompt before it expires.",
            ReadinessDeadline =>
                "The elevated collector did not become ready in time. Bring the approved application forward and retry.",
            CleanupDeadline =>
                "The elevated collector did not finish closing in time. Check the diagnostic report and restart the Recorder before retrying.",
            TargetChanged | ProcessIdentity | ScopeClosed =>
                "The approved application or its capture scope changed. Bring the approved window to the foreground and retry.",
            TargetAccess | Witness =>
                "The collector could not verify access to the approved window on the interactive desktop. Return to that desktop and retry.",
            ConsentLaunch | Busy | PeerExited | Supervisor | LeaseExpired | PipeSecurity | PipeConnection | PipePeer
            | Transport | Protocol | Arguments | Cryptography | RequiredRegistration | RequiredHook | NativeProducer =>
                "The protected collector or its required input channel failed. Check the diagnostic report and restart the Recorder before retrying.",
            StandardHelperUnavailable =>
                "The standard input helper could not resume capture. Restart the Recorder before trying again.",
            Cancelled => "The protected capture request was cancelled. Start again to request a new capture.",
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn elevated(stage: SourceStage, failure: screen_fix_elevated::Failure, admitted: bool) -> Self {
        use screen_fix_elevated::Failure;
        use SourceFailureReason as Reason;
        let reason = match failure {
            Failure::DeploymentRequired => Reason::DeploymentRequired,
            Failure::InstalledPath => Reason::InstalledPath,
            Failure::InstalledPermissions => Reason::InstalledPermissions,
            Failure::InstalledSignature => Reason::InstalledSignature,
            Failure::ReleaseMismatch => Reason::ReleaseMismatch,
            Failure::ProcessIdentity => Reason::ProcessIdentity,
            Failure::TokenIdentity(_) => Reason::TokenIdentity,
            Failure::Arguments => Reason::Arguments,
            Failure::Busy => Reason::Busy,
            Failure::ConsentDenied => Reason::ConsentDenied,
            Failure::ConsentLaunch => Reason::ConsentLaunch,
            Failure::ConsentDeadline => Reason::ConsentDeadline,
            Failure::ReadinessDeadline => Reason::ReadinessDeadline,
            Failure::CleanupDeadline => Reason::CleanupDeadline,
            Failure::Cancelled => Reason::Cancelled,
            Failure::PeerExited => Reason::PeerExited,
            Failure::PipeSecurity => Reason::PipeSecurity,
            Failure::PipeConnection => Reason::PipeConnection,
            Failure::PipePeer => Reason::PipePeer,
            Failure::Transport => Reason::Transport,
            Failure::Protocol => Reason::Protocol,
            Failure::Cryptography => Reason::Cryptography,
            Failure::Supervisor => Reason::Supervisor,
            Failure::LeaseExpired => Reason::LeaseExpired,
            Failure::Witness => Reason::Witness,
            Failure::TargetChanged => Reason::TargetChanged,
            Failure::TargetAccess => Reason::TargetAccess,
            Failure::ScopeClosed => Reason::ScopeClosed,
            Failure::RequiredRegistration => Reason::RequiredRegistration,
            Failure::StandardHelperUnavailable => Reason::StandardHelperUnavailable,
            Failure::NativeOwnerLost | Failure::NativeProducer(_) => Reason::NativeProducer,
        };
        let native = match failure {
            Failure::NativeProducer(failure) => Some(failure),
            _ => None,
        };
        let token_identity = match failure {
            Failure::TokenIdentity(detail) => Some(detail),
            _ => None,
        };
        Self { stage, reason, admitted, native, token_identity }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceStatus {
    #[default]
    Unselected,
    Ordinary,
    AwaitingConsent,
    AwaitingReadiness,
    Activating,
    Enhanced,
    Unavailable(SourceFailure),
    FailedPrefix(SourceFailure),
}

impl SourceStatus {
    #[cfg(target_os = "windows")]
    pub(crate) fn failure(self) -> Option<SourceFailure> {
        match self {
            Self::Unavailable(failure) | Self::FailedPrefix(failure) => Some(failure),
            _ => None,
        }
    }


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
    pub source: SourceStatus,
    pub ready_video: Vec<ReadyVideo>,
    pub input_helper_ready: bool,
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
            source: SourceStatus::Unselected,
            ready_video: Vec::new(),
            input_helper_ready: false,
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
