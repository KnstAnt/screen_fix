use std::{fmt, path::PathBuf, sync::Arc};

use crate::package::diagnostics::{DiagnosticContext, DiagnosticSnapshot, Failure, FailureComponent};

#[derive(Debug)]
/// An admitted attempt that failed: its package root, and whether its
/// failed manifest publication was confirmed.
pub struct RetainedRecording {
    pub root: PathBuf,
    pub verified_video_frames: u64,
    pub manifest_confirmed: bool,
}

/// What failed, how it reads to the user and how it can be resolved. The
/// controller's fault log keeps it to rebuild the error later.
#[derive(Debug, Clone)]
pub(crate) struct CaptureCause {
    pub(crate) failure: Failure,
    pub(crate) message: Arc<str>,
    pub(crate) remedy: Option<Remedy>,
}

#[derive(Debug)]
pub struct CaptureError {
    cause: CaptureCause,
    context: Option<Box<DiagnosticContext>>,
    /// Secondary causes recorded before a diagnostic context is attached.
    additional_failures: Vec<Failure>,
    retained_recording: Option<RetainedRecording>,
}

/// How far a failed attempt got, which decides how the Recorder reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStage {
    /// Capture had started, standard or elevated (M1-01).
    Admitted,
    /// The foreground process failed token verification; recording did not start.
    TargetRejected,
    /// The elevated source failed before admission; recording did not start.
    ElevatedUnavailable,
    /// Any other failure before admission.
    BeforeAdmission,
}

/// An action the user can take to resolve a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// Install the Media Feature Pack in Windows Optional features.
    InstallMediaFeaturePack,
}

impl CaptureError {
    fn new(failure: Failure, message: impl Into<Arc<str>>) -> Self {
        Self::from(CaptureCause { failure, message: message.into(), remedy: None })
    }

    pub(crate) fn unsupported(operation: &'static str, message: &'static str) -> Self {
        Self::new(Failure::new(FailureComponent::Interface, operation, None), message)
    }

    pub(crate) fn unsupported_configuration(message: &'static str) -> Self {
        Self::new(Failure::new(FailureComponent::Interface, "Record a display", None), message)
    }

    /// A backend failure attributed to the owner that reported it.
    pub(crate) fn owned(
        component: FailureComponent, operation: &'static str, native_code: Option<i32>, message: String,
    ) -> Self {
        Self::new(Failure::new(component, operation, native_code), message)
    }

    pub(crate) fn with_raw_input_registration(
        mut self, mismatch: Option<crate::protocol::ipc::RawInputRegistrationMismatch>,
    ) -> Self {
        self.cause.failure.raw_input_registration = mismatch;
        self
    }

    pub(crate) fn journal(operation: &'static str, error: &crate::package::journal::JournalError) -> Self {
        let native_code = error.source.as_ref().and_then(std::io::Error::raw_os_error);
        Self::new(Failure::journal(operation, error.stream, native_code), error.to_string())
    }

    pub(crate) fn elevated(failure: crate::session::SourceFailure) -> Self {
        use crate::{protocol::SourceFailureReason, session::SourceStage};
        let (component, operation) = if failure.reason == SourceFailureReason::StandardHelperUnavailable {
            (FailureComponent::InputHelper, "Resume the standard input helper")
        } else {
            let operation = match failure.stage {
                SourceStage::Foreground => "Verify foreground process access",
                SourceStage::Consent => "Request Windows elevation consent",
                SourceStage::Readiness => "Prepare the elevated capture source",
                SourceStage::Activation => "Activate the elevated capture source",
                SourceStage::Capture => "Capture the approved elevated application",
                SourceStage::Cleanup => "Stop the elevated capture source",
            };
            (FailureComponent::ElevatedSource, operation)
        };
        let mut error = Self::owned(
            component, operation, failure.native.and_then(|native| native.native_code), crate::session::source_failure_guidance(failure).into(),
        );
        error.cause.failure.source = Some(failure);
        error
    }

    pub(crate) fn with_retained_recording(mut self, root: PathBuf, verified_video_frames: u64, manifest_confirmed: bool) -> Self {
        self.retained_recording = Some(RetainedRecording { root, verified_video_frames, manifest_confirmed });
        self
    }

    pub fn retained_recording(&self) -> Option<&RetainedRecording> {
        self.retained_recording.as_ref()
    }

    pub(crate) fn with_remedy(mut self, remedy: Option<Remedy>) -> Self {
        self.cause.remedy = remedy;
        self
    }

    /// The action that can resolve this failure, if one is known.
    pub fn remedy(&self) -> Option<Remedy> {
        self.cause.remedy
    }

    pub(crate) fn with_additional_failure(mut self, failure: Failure) -> Self {
        match &mut self.context {
            Some(context) => context.additional_failures.push(failure),
            None => self.additional_failures.push(failure),
        }
        self
    }

    pub fn source_failure(&self) -> Option<crate::session::SourceFailure> {
        self.cause.failure.source
    }

    /// How far the attempt got before this failure.
    pub fn attempt_stage(&self) -> AttemptStage {
        use crate::{protocol::SourceFailureReason, session::SourceStage};
        let source = self.source_failure();
        if self.retained_recording.is_some() || source.is_some_and(|failure| failure.admitted) {
            return AttemptStage::Admitted;
        }
        match source {
            Some(failure) if failure.reason == SourceFailureReason::TokenIdentity
                && failure.stage == SourceStage::Foreground => AttemptStage::TargetRejected,
            Some(_) => AttemptStage::ElevatedUnavailable,
            None => AttemptStage::BeforeAdmission,
        }
    }

    pub(crate) fn cause(&self) -> &CaptureCause {
        &self.cause
    }

    pub(crate) fn failure(&self) -> Failure {
        self.cause.failure
    }

    pub(crate) fn operation(&self) -> &'static str {
        self.cause.failure.operation()
    }

    pub(crate) fn with_context(mut self, mut context: DiagnosticContext) -> Self {
        if self.context.is_none() {
            context.additional_failures.append(&mut self.additional_failures);
            self.context = Some(Box::new(context));
        }
        self
    }

    pub fn into_diagnostic(self) -> DiagnosticSnapshot {
        let snapshot = DiagnosticSnapshot::new(self.cause.failure);
        if let Some(context) = self.context {
            snapshot.with_context(*context)
        } else {
            snapshot
        }
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.cause.message)
    }
}

impl std::error::Error for CaptureError {}

/// An error with no diagnostic context, secondary causes or retained recording.
impl From<CaptureCause> for CaptureError {
    fn from(cause: CaptureCause) -> Self {
        Self { cause, context: None, additional_failures: Vec::new(), retained_recording: None }
    }
}
