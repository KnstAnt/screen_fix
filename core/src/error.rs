use std::{fmt, path::PathBuf};

use crate::package::diagnostics::{DiagnosticContext, DiagnosticSnapshot, Failure, FailureComponent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureErrorKind {
    UnsupportedPlatform,
    UnsupportedConfiguration,
    BackendFailure,
    PackageFailure,
    JournalFailure,
}

#[derive(Debug)]
pub struct RetainedRecording {
    pub root: PathBuf,
    pub verified_video_frames: u64,
}

#[derive(Debug)]
pub struct CaptureError {
    pub kind: CaptureErrorKind,
    pub operation: &'static str,
    pub native_code: Option<i32>,
    message: String,
    failure: Failure,
    context: Option<Box<DiagnosticContext>>,
    retained_recording: Option<RetainedRecording>,
}

impl CaptureError {
    #[cfg(target_os = "windows")]
    pub(crate) fn unsupported(operation: &'static str, message: &'static str) -> Self {
        Self {
            kind: CaptureErrorKind::UnsupportedPlatform,
            operation,
            native_code: None,
            message: message.into(),
            failure: Failure::new(FailureComponent::Interface, operation, None),
            context: None,
            retained_recording: None,
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn unsupported_configuration(message: &'static str) -> Self {
        Self {
            kind: CaptureErrorKind::UnsupportedConfiguration,
            operation: "Record a display",
            native_code: None,
            message: message.into(),
            failure: Failure::new(FailureComponent::Interface, "Record a display", None),
            context: None,
            retained_recording: None,
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn backend(operation: &'static str, native_code: Option<i32>, message: String) -> Self {
        Self::classified(
            CaptureErrorKind::BackendFailure, FailureComponent::WindowsCapture, operation, native_code, message,
        )
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn with_raw_input_registration(
        mut self, mismatch: Option<crate::protocol::ipc::RawInputRegistrationMismatch>,
    ) -> Self {
        self.failure.raw_input_registration = mismatch;
        self
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn classified(
        kind: CaptureErrorKind, component: FailureComponent, operation: &'static str,
        native_code: Option<i32>, message: String,
    ) -> Self {
        Self { kind, operation, native_code, message, failure: Failure::new(component, operation, native_code), context: None, retained_recording: None }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn journal(operation: &'static str, error: crate::package::journal::JournalError) -> Self {
        let native_code = error.source.as_ref().and_then(std::io::Error::raw_os_error);
        Self {
            kind: CaptureErrorKind::JournalFailure, operation, native_code, message: error.to_string(),
            failure: Failure::journal(operation, error.stream, native_code), context: None,
            retained_recording: None,
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn elevated(failure: crate::session::SourceFailure) -> Self {
        use crate::session::{SourceFailureReason, SourceStage};
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
        let mut error = Self::classified(
            CaptureErrorKind::BackendFailure, component, operation,
            failure.native.and_then(|native| native.native_code),
            failure.guidance().into(),
        );
        error.failure.source = Some(failure);
        error
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn with_retained_recording(mut self, root: PathBuf, verified_video_frames: u64) -> Self {
        self.retained_recording = Some(RetainedRecording { root, verified_video_frames });
        self
    }

    pub fn retained_recording(&self) -> Option<&RetainedRecording> {
        self.retained_recording.as_ref()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn with_additional_failure(mut self, failure: Failure) -> Self {
        if let Some(context) = &mut self.context {
            context.additional_failures.push(failure);
        }
        self
    }

    pub fn source_failure(&self) -> Option<crate::session::SourceFailure> {
        self.failure.source
    }

    pub(crate) fn failure(&self) -> Failure {
        self.failure
    }

    pub(crate) fn with_context(mut self, context: DiagnosticContext) -> Self {
        if self.context.is_none() {
            self.context = Some(Box::new(context));
        }
        self
    }

    pub fn into_diagnostic(self) -> DiagnosticSnapshot {
        let snapshot = DiagnosticSnapshot::new(self.failure);
        if let Some(context) = self.context {
            snapshot.with_context(*context)
        } else {
            snapshot
        }
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CaptureError {}
