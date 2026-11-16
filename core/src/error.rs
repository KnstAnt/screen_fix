use std::fmt;

use crate::package::diagnostics::{DiagnosticContext, DiagnosticSnapshot, Failure, FailureComponent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureErrorKind {
    UnsupportedPlatform,
    UnsupportedConfiguration,
    BackendFailure,
    PackageFailure,
    JournalFailure,
    PostprocessFailure,
}

#[derive(Debug)]
pub struct CaptureError {
    pub kind: CaptureErrorKind,
    pub operation: &'static str,
    pub native_code: Option<i32>,
    message: String,
    failure: Failure,
    context: Option<Box<DiagnosticContext>>,
}

impl CaptureError {
    pub(crate) fn unsupported(operation: &'static str, message: &'static str) -> Self {
        Self {
            kind: CaptureErrorKind::UnsupportedPlatform,
            operation,
            native_code: None,
            message: message.into(),
            failure: Failure::new(FailureComponent::Interface, operation, None),
            context: None,
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
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn backend(operation: &'static str, native_code: Option<i32>, message: String) -> Self {
        Self::classified(
            CaptureErrorKind::BackendFailure, FailureComponent::WindowsCapture, operation, native_code, message,
        )
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn classified(
        kind: CaptureErrorKind, component: FailureComponent, operation: &'static str,
        native_code: Option<i32>, message: String,
    ) -> Self {
        Self { kind, operation, native_code, message, failure: Failure::new(component, operation, native_code), context: None }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn journal(operation: &'static str, error: crate::package::journal::JournalError) -> Self {
        let native_code = error.source.as_ref().and_then(std::io::Error::raw_os_error);
        Self {
            kind: CaptureErrorKind::JournalFailure, operation, native_code, message: error.to_string(),
            failure: Failure::journal(operation, error.stream, native_code), context: None,
        }
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
