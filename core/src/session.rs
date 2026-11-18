//! Commands, status, and boundary values for one recording attempt.

use std::sync::mpsc;

pub mod hotkey;

use crate::{
    CaptureError,
    package::diagnostics::ReportOutcome,
    protocol::{MonitorId, SourceFailureReason, TokenIdentityCheck},
};

pub use crate::package::source::{AdmissionBoundary, DiskPreflight, PauseReason, SourceFailure, SourceStage};

pub(crate) mod fault;
pub(crate) mod labels;

// The platform controller behind a `SessionHandle`: the only
// `platform::windows` item that this platform-neutral API names.
use crate::platform::windows::controller::Control;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionPhase {
    #[default]
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

/// An activity that stopped for the rest of the attempt while recording continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DegradedActivity {
    pub component: crate::package::diagnostics::FailureComponent,
    pub monitor: Option<MonitorId>,
    /// Input or one monitor's video; its loss makes the attempt incomplete.
    pub required: bool,
}

impl DegradedActivity {
    pub fn label(&self) -> String {
        match self.monitor {
            Some(monitor) => format!("{} (monitor {})", self.component.label(), monitor.0),
            None => self.component.label().to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoReport {
    pub monitor: MonitorId,
    pub outcome: ReportOutcome,
}

/// What the user can do about `failure`. This is Recorder UI text, not part
/// of the manifest vocabulary.
pub(crate) fn source_failure_guidance(failure: SourceFailure) -> &'static str {
    use SourceFailureReason::*;
    match failure.reason {
        DeploymentRequired | InstalledPath | InstalledPermissions | InstalledSignature | ReleaseMismatch =>
            "Install or repair the matching signed Recorder and collector using the official machine-wide installer.",
        RevocationUnknown =>
            "Windows could not confirm from its cached revocation data that the installed publisher certificate is not revoked. Check the diagnostic report, then retry after Windows has refreshed its certificate revocation data.",
        TokenIdentity => match failure.token_identity.map(|detail| detail.check) {
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
    pub(crate) fn failure(self) -> Option<SourceFailure> {
        match self {
            Self::Unavailable(failure) | Self::FailedPrefix(failure) => Some(failure),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum StartupStep {
    #[default]
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

/// A snapshot of the attempt for the Recorder UI.
#[derive(Debug, Clone, Default)]
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
    /// The active pause reasons, in `PauseReason` order.
    pub pause_reasons: Vec<PauseReason>,
    pub video_unavailable: Vec<MonitorId>,
    pub video_reports: Vec<VideoReport>,
    pub input_unavailable: Option<&'static str>,
    pub input_report: Option<ReportOutcome>,
    pub hotkeys_unavailable: bool,
    pub last_hotkey: Option<hotkey::ControlOutcome>,
    /// Activities lost while the attempt continues (§6.2 class L), in report order.
    pub degraded: Vec<DegradedActivity>,
}

impl SessionStatus {
    /// The attempt's state as the Recorder UI and the recording indicator
    /// present it. Each surface chooses its own wording.
    pub fn summary(&self) -> StatusSummary {
        let wait = match self.source {
            SourceStatus::AwaitingConsent => Some(SourceWait::Consent),
            SourceStatus::AwaitingReadiness => Some(SourceWait::Readiness),
            _ => None,
        };
        match self.phase {
            SessionPhase::Starting => StatusSummary::Starting(wait),
            SessionPhase::Recording => StatusSummary::Recording(RecordingSummary {
                source: match self.source {
                    SourceStatus::Ordinary => RecordingSource::Standard,
                    SourceStatus::Enhanced => RecordingSource::Enhanced,
                    SourceStatus::Activating => RecordingSource::Activating,
                    SourceStatus::FailedPrefix(_) => RecordingSource::FailedPrefix,
                    _ => RecordingSource::Other,
                },
                input_lost: self.input_unavailable.is_some(),
                video_lost: !self.video_unavailable.is_empty(),
            }),
            SessionPhase::Paused => StatusSummary::Paused(wait),
            SessionPhase::Stopping => StatusSummary::Stopping,
            SessionPhase::Finished => StatusSummary::Finished { incomplete: self.outcome == Some(SessionOutcome::Incomplete) },
            SessionPhase::Failed => StatusSummary::Failed,
        }
    }
}

/// `SessionStatus::summary`: the phase with the details that its labels show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSummary {
    /// Before Start, possibly waiting for the elevated source.
    Starting(Option<SourceWait>),
    Recording(RecordingSummary),
    /// Paused, possibly waiting for the next elevated source.
    Paused(Option<SourceWait>),
    Stopping,
    Finished { incomplete: bool },
    Failed,
}

/// The elevated source step that a starting or paused attempt waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceWait {
    Consent,
    Readiness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingSummary {
    pub source: RecordingSource,
    /// Input capture stopped for the rest of the attempt.
    pub input_lost: bool,
    /// Video stopped on at least one monitor.
    pub video_lost: bool,
}

/// The capture source of a recording attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingSource {
    Standard,
    Enhanced,
    /// The elevated source waits for certified input.
    Activating,
    /// A source failure is closing the admitted prefix.
    FailedPrefix,
    /// No selected source is reported.
    Other,
}

pub struct SessionHandle {
    result: mpsc::Receiver<Result<SessionResult, CaptureError>>,
    control: Control,
}

impl SessionHandle {
    pub(crate) fn new(result: mpsc::Receiver<Result<SessionResult, CaptureError>>, control: Control) -> Self {
        Self { result, control }
    }

    pub(crate) fn indicator_control(&self) -> Control {
        self.control.clone()
    }
    /// A short-lived status lock can be busy. Keep displaying the last snapshot.
    pub fn status(&self) -> Option<SessionStatus> {
        self.control.status()
    }

    pub fn submit(&self, command: SessionCommand) -> Result<(), CommandError> {
        self.control.submit(command)
    }

    /// Submit the fixture's independent confirmation to this recording attempt.
    /// The reply acknowledges routing, while stills.jsonl records image outcomes.
    pub fn confirm_fixture_save(
        &self, confirmation: crate::capture::save::FixtureSave,
    ) -> Result<mpsc::Receiver<crate::capture::save::FixtureSaveResult>, crate::capture::save::FixtureSaveError> {
        self.control.confirm_fixture_save(confirmation)
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
