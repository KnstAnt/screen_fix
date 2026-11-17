//! Local reports contain fixed technical facts, never captured record payloads.

use std::{
    fs::File,
    io::{self, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::{CapturePace, ClipboardMode, ResolvedCaptureConfig},
    protocol::{AttemptId, MonitorId, SessionTimeUs, StreamId},
};

use super::{StreamSummary, metadata::{CaptureCounters, PngCounters, VideoCounters}};

static REPORT_SERIAL: AtomicU64 = AtomicU64::new(0);

/// The owner that reported a failure. It is always the reporting owner,
/// never a fixed default (§6.2 rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureComponent {
    Interface,
    Controller,
    /// Windows capability, display and per-monitor capture owners.
    Capture,
    Video,
    InputHelper,
    /// The Recorder's input receiver and context loop.
    Receiver,
    /// Approved-application and foreground evidence.
    Context,
    Stills,
    ElevatedSource,
    PackageMetadata,
    Journal,
}

impl FailureComponent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Controller => "session controller",
            Self::Capture => "screen capture",
            Self::Video => "video",
            Self::InputHelper => "input helper",
            Self::Receiver => "input receiver",
            Self::Context => "context",
            Self::Stills => "stills",
            Self::ElevatedSource => "elevated collector",
            Self::PackageMetadata => "package metadata",
            Self::Journal => "journal",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Failure {
    component: FailureComponent,
    operation: &'static str,
    stream: Option<StreamId>,
    native_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<crate::session::SourceFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) raw_input_registration: Option<crate::protocol::ipc::RawInputRegistrationMismatch>,
}

impl Failure {
    pub(crate) fn without_report_detail(mut self) -> Self {
        self.raw_input_registration = None;
        self.source = self.source.map(crate::session::SourceFailure::without_report_detail);
        self
    }

    pub fn new(component: FailureComponent, operation: &'static str, native_code: Option<i32>) -> Self {
        Self { component, operation, stream: None, native_code, source: None, raw_input_registration: None }
    }

    pub(crate) fn component(&self) -> FailureComponent {
        self.component
    }

    /// The same owner reported the same operation and native result.
    pub(crate) fn same_cause(&self, other: &Self) -> bool {
        self.component == other.component && self.operation == other.operation && self.stream == other.stream
            && self.native_code == other.native_code && self.source == other.source
    }

    pub(crate) fn operation(&self) -> &'static str {
        self.operation
    }

    pub(crate) fn native_code(&self) -> Option<i32> {
        self.native_code
    }

    pub(crate) fn journal(operation: &'static str, stream: StreamId, native_code: Option<i32>) -> Self {
        Self { component: FailureComponent::Journal, operation, stream: Some(stream), native_code, source: None, raw_input_registration: None }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct DiagnosticContext {
    #[serde(skip)]
    output_root: PathBuf,
    pub(crate) attempt: Option<AttemptId>,
    pub(crate) session_time_us: Option<SessionTimeUs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed_monitor: Option<MonitorId>,
    selected_displays: Vec<MonitorId>,
    profile_revision: u16,
    capture_fps: u8,
    clipboard_opt_in: bool,
    pub(crate) streams: std::collections::BTreeMap<StreamId, StreamSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) capture: Vec<(MonitorId, CaptureCounters)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) video: Vec<(MonitorId, VideoCounters)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) png: Option<PngCounters>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) input_registration: Option<crate::protocol::ipc::InputRegistration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_evidence: Option<super::metadata::SourceEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed_manifest_published: Option<bool>,
    /// Causes recorded after the primary cause, in report order (§6.2 rule 2).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) additional_failures: Vec<Failure>,
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) omitted_fault_reports: u64,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

impl DiagnosticContext {
    pub(crate) fn for_config(config: &ResolvedCaptureConfig) -> Self {
        let profile = config.profile();
        Self {
            output_root: config.output_root().to_owned(),
            attempt: None,
            session_time_us: None,
            failed_monitor: None,
            selected_displays: config.monitors().iter().map(|monitor| monitor.id).collect(),
            profile_revision: profile.revision().get(),
            capture_fps: match profile.capture_pace() {
                CapturePace::Fps30 => 30,
                CapturePace::Fps60 => 60,
            },
            clipboard_opt_in: profile.clipboard() == ClipboardMode::OptIn,
            streams: Default::default(),
            capture: Vec::new(),
            video: Vec::new(),
            png: None,
            input_registration: None,
            source_evidence: None,
            failed_manifest_published: None,
            additional_failures: Vec::new(),
            omitted_fault_reports: 0,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DiagnosticSnapshot {
    #[serde(skip)]
    output_root: Option<PathBuf>,
    recorder_version: &'static str,
    platform: &'static str,
    utc_ns: Option<u64>,
    failure: Failure,
    context: Option<DiagnosticContext>,
}

impl DiagnosticSnapshot {
    pub fn new(failure: Failure) -> Self {
        Self {
            output_root: None,
            recorder_version: env!("CARGO_PKG_VERSION"),
            platform: std::env::consts::OS,
            utc_ns: utc_ns(),
            failure,
            context: None,
        }
    }

    pub(crate) fn with_context(mut self, context: DiagnosticContext) -> Self {
        self.output_root = Some(context.output_root.clone());
        self.context = Some(context);
        self
    }

    /// Select the recording output directory for errors before a session exists.
    pub fn with_output_root(mut self, output_root: PathBuf) -> Self {
        if self.output_root.is_none() && !output_root.as_os_str().is_empty() {
            self.output_root = Some(output_root);
        }
        self
    }

    /// Run on a report worker or a command-line caller.
    pub fn write(self) -> ReportOutcome {
        match self.write_once() {
            Ok(path) => ReportOutcome::Saved(path),
            Err(error) => ReportOutcome::Unavailable { native_code: error.raw_os_error() },
        }
    }

    fn write_once(&self) -> io::Result<PathBuf> {
        let serial = REPORT_SERIAL.fetch_add(1, Ordering::Relaxed);
        // Before an output directory is selected, startup errors still have a
        // local destination. A selected but inaccessible directory never falls
        // back silently to a different location.
        let directory = self.output_root.clone().unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!(
            "screen-fix-report-{}-{}-{serial}.json",
            std::process::id(),
            self.utc_ns.unwrap_or_default(),
        ));
        let mut file = File::create_new(&path)?;
        let result = (|| {
            serde_json::to_writer_pretty(&mut file, self).map_err(io::Error::from)?;
            file.write_all(b"\n")?;
            file.sync_all()
        })();
        drop(file);
        if let Err(error) = result {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        Ok(path)
    }
}

fn utc_ns() -> Option<u64> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
}

#[derive(Debug, Clone)]
pub enum ReportOutcome {
    Saved(PathBuf),
    Unavailable { native_code: Option<i32> },
}

impl std::fmt::Display for ReportOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saved(path) => write!(f, "Diagnostic report saved at {}.", path.display()),
            Self::Unavailable { native_code: Some(code) } =>
                write!(f, "Diagnostic report unavailable (system code {code})."),
            Self::Unavailable { native_code: None } =>
                f.write_str("Diagnostic report unavailable."),
        }
    }
}
