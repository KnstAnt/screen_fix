//! The metadata owner is the only live publisher of an attempt manifest.

use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::{
    config::{ClipboardMode, DeviceCalibration, ResolvedCaptureConfig},
    protocol::{AttemptId, ClockOrigin, MonitorId, SegmentId, StreamId, UtcAnchor},
    session::AdmissionBoundary,
};

use super::{ArtifactKind, ArtifactRef, journal::StreamSummary};

pub const MANIFEST_VERSION: u16 = 5;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

pub type PackageResult<T> = Result<T, PackageError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageOperation {
    CreateAttempt,
    CreateLayout,
    CreateManifest,
    EncodeManifest,
    FlushManifest,
    PublishManifest,
    MarkLateManifestIncomplete,
    OpenManifest,
    ReadManifest,
    OpenArtifact,
    InspectArtifact,
    ReadArtifact,
}

impl PackageOperation {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::CreateAttempt => "Create the recording attempt directory",
            Self::CreateLayout => "Create the recording package layout",
            Self::CreateManifest => "Create a temporary manifest",
            Self::EncodeManifest => "Write the recording manifest",
            Self::FlushManifest => "Flush the recording manifest",
            Self::PublishManifest => "Publish the recording manifest",
            Self::MarkLateManifestIncomplete => "Mark a late recording manifest incomplete",
            Self::OpenManifest => "Open the recording manifest",
            Self::ReadManifest => "Read the recording manifest",
            Self::OpenArtifact => "Open a recording artifact",
            Self::InspectArtifact => "Inspect a recording artifact",
            Self::ReadArtifact => "Read a recording artifact",
        }
    }
}

#[derive(Debug)]
pub enum PackageError {
    Io { operation: PackageOperation, source: io::Error },
    InvalidManifest,
    InconsistentInventory,
    WrongAttempt,
    UnsupportedVersion(u16),
    InvalidArtifact(ArtifactKind),
    ArtifactNotInventoried(ArtifactKind),
    InvalidJournal { stream: StreamId, row: u64 },
}

impl PackageError {
    pub(super) fn io(operation: PackageOperation, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "{}: {source}.", operation.message()),
            Self::InvalidManifest => f.write_str("The recording manifest is invalid."),
            Self::InconsistentInventory => f.write_str("The closed producers do not reconcile with the recording inventory."),
            Self::WrongAttempt => f.write_str("The artifact belongs to a different recording attempt."),
            Self::UnsupportedVersion(version) => write!(f, "Manifest version {version} is unsupported."),
            Self::InvalidArtifact(kind) => write!(f, "The recording artifact {kind:?} has an invalid file type or size."),
            Self::ArtifactNotInventoried(kind) => write!(f, "The recording manifest does not list {kind:?}."),
            Self::InvalidJournal { stream, row: 0 } => write!(f, "The {stream:?} journal does not reconcile with its inventory."),
            Self::InvalidJournal { stream, row } => write!(f, "The {stream:?} journal is invalid at row {row}."),
        }
    }
}

impl std::error::Error for PackageError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageState {
    Incomplete,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureDisposition {
    Pending,
    Recorded,
    StoppedBeforeFirstFrame,
    ApprovalSuppressed,
    VideoUnavailable,
    InputUnavailable,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub qpc_at_start: i64,
    pub qpc_frequency: i64,
    pub qpc_uncertainty_100ns: i64,
    pub video_origin_100ns: i64,
    pub stop_100ns: i64,
    #[serde(default)]
    pub common_clock_schema: Option<u32>,
    #[serde(default)]
    pub utc_anchors: Vec<UtcAnchor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Geometry {
    pub display_id: String,
    pub desktop_rect: [i32; 4],
    pub source_size: [i32; 2],
    pub video_size: [u32; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_rect: Option<[u32; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_scale: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VideoSummary {
    pub monitor: MonitorId,
    pub segment: SegmentId,
    pub frame_count: usize,
    pub timeline: Timeline,
    pub geometry: Geometry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentSummary {
    pub monitor: MonitorId,
    pub segment: SegmentId,
    pub frame_count: u64,
    pub common_start_us: crate::protocol::SessionTimeUs,
    pub local_end_100ns: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<SegmentEncoding>,
    pub geometry: Geometry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentEncoding {
    pub admission_epoch: crate::protocol::AdmissionEpoch,
    pub first_frame: crate::protocol::FrameId,
    pub first_geometry: crate::protocol::GeometryEpoch,
    pub fps: u32,
    pub software: bool,
    pub encoder_target_bitrate_bps: u32,
    pub media_mean_bitrate_bps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingVideoSlot {
    BeforeFirstFrame,
    ApprovalSuppressed,
    CaptureUnavailable,
    MonitorMissing,
    ConversionPressure,
    QueuePressure,
    EncoderUnavailable,
    SegmentUnpublished,
    EncoderSkipped,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoCounters {
    pub indexed_encoded: u64,
    pub repeated: u64,
    pub changed_frame_arrived_late: u64,
    pub missing_by_reason: std::collections::BTreeMap<MissingVideoSlot, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorVideo {
    pub monitor: MonitorId,
    pub fps: u32,
    pub degraded: bool,
    pub slots: u64,
    pub encoded: u64,
    #[serde(default)]
    pub approval_suppressed: u64,
    pub unavailable: bool,
    #[serde(default)]
    pub counters: VideoCounters,
    pub segments: Vec<SegmentSummary>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PngCounters {
    pub jobs_offered: u64,
    pub jobs_rejected: u64,
    pub jobs_evicted: u64,
    pub queue_peak: usize,
    pub new_images: u64,
    pub deduplicated_jobs: u64,
    pub dropped_at_processing: u64,
    pub failed_jobs: u64,
    pub queue_wait_max_us: u64,
    pub processing_max_us: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureCounters {
    pub captured: u64,
    #[serde(default)]
    pub frames_dequeued: u64,
    #[serde(default)]
    pub rejected_time_or_epoch: u64,
    #[serde(default)]
    pub rejected_approval: u64,
    #[serde(default)]
    pub native_texture_bytes_peak: u64,
    pub mismatched_size: u64,
    pub native_pressure: u64,
    pub thumbnail_pressure: u64,
    #[serde(default, alias = "legacy_video_pressure")]
    pub video_pressure: u64,
}

/// Actual mode reported by one selected monitor's capture owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorCapture {
    pub device_path: String,
    pub geometry: crate::capture::frame::MonitorGeometry,
    pub capture_epoch: crate::protocol::CaptureEpoch,
    pub refresh_numerator: u32,
    pub refresh_denominator: std::num::NonZeroU32,
    pub rotation: i32,
    pub primary: bool,
    pub hdr: bool,
    pub sdr_white_level: Option<std::num::NonZeroU32>,
    pub conversion: crate::capture::frame::CaptureColor,
    pub tone_map: Option<String>,
    pub fp16_fallback_hresult: Option<i32>,
    pub border_visible: bool,
    pub cursor_in_pixels: bool,
    pub video_connected: bool,
    #[serde(default)]
    pub counters: CaptureCounters,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputBlindReason {
    HelperExited,
    RawInputRegistrationLost,
    HookUnavailable,
    HelperCommunicationFailed,
    HelperCleanupFailed,
    ProducerProgressStalled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputBlind {
    pub start_t_us: crate::protocol::SessionTimeUs,
    pub end_t_us: crate::protocol::SessionTimeUs,
    pub reason: InputBlindReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputPublication {
    PendingApprovalPath,
    ApprovedReceiptGate,
}

/// Receipt progress bounds are exclusive. This interval ends at the
/// inclusive admission cutoff and has no inferred loss count.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UncertifiedInputTail {
    pub first_t_us: crate::protocol::SessionTimeUs,
    pub cutoff_t_us: crate::protocol::SessionTimeUs,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PenPressure {
    Captured,
    Disabled,
    UnavailableNoHidCollection,
    UnavailableVmOrRemote,
    UnavailableUnknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabletDriverMode {
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabletInputPath {
    NativeDigitizerHid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PenCapture {
    #[serde(rename = "pen_pressure")]
    pub pressure: PenPressure,
    pub observed_path: Option<TabletInputPath>,
    pub driver_mode: TabletDriverMode,
    pub app_tablet_settings: crate::protocol::Evidence<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCapture {
    pub registration: crate::protocol::ipc::InputRegistration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pen: Option<PenCapture>,
    pub publication: InputPublication,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<crate::capture::context::InputSettings>,
    pub transport_overflow: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices: Option<crate::protocol::BoundedList<crate::protocol::ipc::DeviceDescriptor, { crate::protocol::ipc::MAX_INPUT_DEVICES }>>,
    #[serde(default)]
    pub device_inventory_truncated: bool,
    #[serde(default)]
    pub keyboard_hook_loss_suspected: bool,
    #[serde(default)]
    pub pointer_hook_loss_suspected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertified_closing_tail: Option<UncertifiedInputTail>,
    pub blind: Option<InputBlind>,
}

/// Common time for every stream, independent of whether any segment was saved.
#[derive(Debug, Serialize, Deserialize)]
pub struct AttemptClock {
    pub origin: ClockOrigin,
    pub utc_anchors: Vec<ClockAnchor>,
    pub stop: Option<AdmissionBoundary>,
    pub last_admitted_boundary: Option<AdmissionBoundary>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClockAnchor {
    pub admission_epoch: crate::protocol::AdmissionEpoch,
    pub sample: UtcAnchor,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileInventory {
    pub m1_stub_revision: std::num::NonZeroU16,
    pub approved_executable_count: usize,
    pub requested_capture_fps: u32,
    pub clipboard_opt_in: bool,
    pub pen_hid_requested: bool,
    pub device_calibrations: Vec<DeviceCalibration>,
    pub unspecified_device_calibration: crate::protocol::UnavailableReason,
}

impl From<&ResolvedCaptureConfig> for ProfileInventory {
    fn from(config: &ResolvedCaptureConfig) -> Self {
        let profile = config.profile();
        Self {
            m1_stub_revision: profile.revision(),
            approved_executable_count: profile.approved_identities().len(),
            requested_capture_fps: profile.capture_pace().frames_per_second(),
            clipboard_opt_in: profile.clipboard() == ClipboardMode::OptIn,
            pen_hid_requested: profile.pen_hid(),
            device_calibrations: config.calibrations().to_vec(),
            unspecified_device_calibration: crate::protocol::UnavailableReason::MissingCalibration,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AttemptManifest {
    pub version: u16,
    pub attempt_id: AttemptId,
    pub state: PackageState,
    pub capture: CaptureDisposition,
    pub selected_displays: Vec<MonitorId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileInventory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<AttemptClock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<crate::platform::WindowsEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitor_capture: Vec<MonitorCapture>,
    /// Mode at every successfully activated geometry epoch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitor_epochs: Vec<MonitorCapture>,
    pub failed_step: Option<String>,
    pub streams: std::collections::BTreeMap<StreamId, StreamSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transitional_video: Option<VideoSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitor_video: Vec<MonitorVideo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_capture: Option<InputCapture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_registration: Option<crate::protocol::ipc::InputRegistration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png: Option<PngCounters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_preflight: Option<crate::session::DiskPreflight>,
}

impl AttemptManifest {
    fn validate_recorded_inventory(&self) -> PackageResult<()> {
        let inconsistent = || PackageError::InconsistentInventory;
        let clock = self.clock.as_ref().ok_or_else(inconsistent)?;
        let input = self.input_capture.as_ref().ok_or_else(inconsistent)?;
        if clock.stop.is_none() || clock.last_admitted_boundary.is_none()
            || clock.utc_anchors.first().is_none_or(|first| first.sample.qpc != clock.origin.origin_qpc)
            || input.blind.is_some() || input.uncertified_closing_tail.is_some()
            || self.monitor_video.len() != self.selected_displays.len()
            || self.streams.len() != 4
        {
            return Err(inconsistent());
        }
        let geometry_epochs: std::collections::BTreeSet<_> = self.monitor_epochs.iter().map(|mode| {
            (mode.geometry.id.monitor, mode.geometry.id.epoch, mode.capture_epoch)
        }).collect();
        let mut slots = 0_u64;
        for &id in &self.selected_displays {
            let mut candidates = self.monitor_video.iter().filter(|video| video.monitor == id);
            let video = candidates.next().ok_or_else(inconsistent)?;
            if candidates.next().is_some() || video.unavailable || video.encoded == 0
                || video.slots < video.encoded || video.approval_suppressed > video.slots - video.encoded
                || !self.monitor_capture.iter().any(|capture| capture.geometry.id.monitor == id)
            {
                return Err(inconsistent());
            }
            let mut previous_id = 0;
            let mut frames = 0_u64;
            for segment in &video.segments {
                let encoding = segment.encoding.as_ref().ok_or_else(inconsistent)?;
                if segment.monitor != id || segment.segment.0 <= previous_id
                    || encoding.first_frame.monitor != id || segment.frame_count == 0
                    || segment.local_end_100ns <= 0 || segment.published_bytes.is_none_or(|bytes| bytes == 0)
                    || segment.geometry.source_size.iter().any(|&dimension| dimension <= 0)
                    || segment.geometry.video_size.contains(&0)
                    || !geometry_epochs.contains(&(id, encoding.first_geometry, encoding.first_frame.epoch))
                {
                    return Err(inconsistent());
                }
                previous_id = segment.segment.0;
                frames = frames.checked_add(segment.frame_count).ok_or_else(inconsistent)?;
            }
            let missing = video.counters.missing_by_reason.values().try_fold(0_u64, |total, count| total.checked_add(*count))
                .ok_or_else(inconsistent)?;
            if video.encoded != frames || video.encoded != video.counters.indexed_encoded
                || missing != video.slots - video.encoded
            {
                return Err(inconsistent());
            }
            slots = slots.checked_add(video.slots).ok_or_else(inconsistent)?;
        }
        let frame_journal = self.streams.get(&StreamId::Frames).ok_or_else(inconsistent)?;
        if frame_journal.data_rows != slots || frame_journal.dropped != 0 {
            return Err(inconsistent());
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AttemptLayout {
    id: AttemptId,
    root: PathBuf,
}

impl AttemptLayout {
    pub(crate) fn id(&self) -> &AttemptId {
        &self.id
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, kind: ArtifactKind) -> PathBuf {
        self.root.join(ArtifactRef { attempt: self.id.clone(), kind }.relative_uri())
    }
}

pub(crate) struct MetadataOwner {
    layout: AttemptLayout,
    manifest: AttemptManifest,
}

impl MetadataOwner {
    pub(crate) fn create(
        config: &ResolvedCaptureConfig, disk: crate::session::DiskPreflight, os: crate::platform::WindowsEvidence,
    ) -> PackageResult<Self> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let attempts = config.output_root().join("attempts");
        fs::create_dir_all(&attempts).map_err(|e| PackageError::io(PackageOperation::CreateAttempt, e))?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let mut layout = None;
        for suffix in 0..32 {
            let candidate = format!("{stamp:x}-{:x}-{suffix:x}", std::process::id());
            let id = AttemptId::new(&candidate).map_err(|_| PackageError::InvalidManifest)?;
            let root = attempts.join(format!("attempt_{}", id.as_str()));
            match fs::create_dir(&root) {
                Ok(()) => {
                    layout = Some(AttemptLayout { id, root });
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(PackageError::io(PackageOperation::CreateAttempt, e)),
            }
        }
        let layout = layout.ok_or_else(|| PackageError::io(
            PackageOperation::CreateAttempt, io::Error::new(io::ErrorKind::AlreadyExists, "Attempt IDs are already in use"),
        ))?;
        let owner = Self {
            manifest: AttemptManifest {
                version: MANIFEST_VERSION, attempt_id: layout.id.clone(),
                state: PackageState::Incomplete, capture: CaptureDisposition::Pending,
                selected_displays: config.monitors().iter().map(|selected| selected.id).collect(),
                profile: Some(config.into()), clock: None, os: Some(os), monitor_capture: Vec::new(), monitor_epochs: Vec::new(), failed_step: None,
                streams: Default::default(), transitional_video: None, monitor_video: Vec::new(), input_capture: None, input_registration: None, png: None,
                disk_preflight: Some(disk),
            },
            layout,
        };
        owner.publish(|| Ok(()))?;
        if let Err(error) = owner.create_layout() {
            let _ = owner.finish(Some(PackageOperation::CreateLayout.message()), Default::default(), Vec::new());
            return Err(error);
        }
        Ok(owner)
    }

    fn create_layout(&self) -> PackageResult<()> {
        for directory in ["raw/video", "raw/images", "raw/cursors"] {
            fs::create_dir_all(self.layout.root.join(directory))
                .map_err(|e| PackageError::io(PackageOperation::CreateLayout, e))?;
        }
        for id in &self.manifest.selected_displays {
            fs::create_dir(self.layout.root.join("raw/video").join(format!("m{}", id.0)))
                .map_err(|e| PackageError::io(PackageOperation::CreateLayout, e))?;
        }
        Ok(())
    }

    pub(crate) fn layout(&self) -> &AttemptLayout {
        &self.layout
    }

    pub(crate) fn set_clock(&mut self, clock: Option<AttemptClock>) {
        self.manifest.clock = clock;
    }

    pub(crate) fn set_monitor_capture(&mut self, monitors: Vec<MonitorCapture>, epochs: Vec<MonitorCapture>) {
        self.manifest.monitor_capture = monitors;
        self.manifest.monitor_epochs = epochs;
    }

    pub(crate) fn set_png(&mut self, png: Option<PngCounters>) {
        self.manifest.png = png;
    }

    pub(crate) fn set_input_capture(
        &mut self, input: Option<InputCapture>, registration: Option<crate::protocol::ipc::InputRegistration>,
    ) {
        self.manifest.input_capture = input;
        self.manifest.input_registration = registration;
    }

    pub(crate) fn finish(
        self,
        failed_step: Option<&'static str>,
        streams: std::collections::BTreeMap<StreamId, StreamSummary>,
        video: Vec<MonitorVideo>,
    ) -> PackageResult<()> {
        self.finish_recording(failed_step, streams, video, || None, || {
            Err(io::Error::other("Complete publication requires authorization"))
        }).map(|_| ())
    }

    pub(crate) fn finish_recording(
        mut self,
        failed_step: Option<&'static str>,
        streams: std::collections::BTreeMap<StreamId, StreamSummary>,
        video: Vec<MonitorVideo>,
        deadline: impl Fn() -> Option<Instant>,
        authorize_complete: impl FnOnce() -> io::Result<()>,
    ) -> PackageResult<CaptureDisposition> {
        self.manifest.streams = streams;
        let any_video = video.iter().any(|monitor| monitor.encoded != 0);
        let video_unavailable = video.iter().any(|monitor| monitor.unavailable)
            || (any_video && self.manifest.selected_displays.iter().any(|id| {
                !video.iter().any(|monitor| monitor.monitor == *id && monitor.encoded != 0)
            }));
        let input_unavailable = self.manifest.clock.is_some() && self.manifest.input_capture.as_ref()
            .is_none_or(|input| input.blind.is_some() || input.uncertified_closing_tail.is_some());
        let approval_suppressed = video.iter().any(|monitor| monitor.slots != 0)
            && video.iter().all(|monitor| monitor.approval_suppressed == monitor.slots);
        self.manifest.monitor_video = video;
        self.manifest.capture = if failed_step.is_some() {
            CaptureDisposition::Failed
        } else if video_unavailable {
            CaptureDisposition::VideoUnavailable
        } else if input_unavailable {
            CaptureDisposition::InputUnavailable
        } else if any_video {
            CaptureDisposition::Recorded
        } else if approval_suppressed {
            CaptureDisposition::ApprovalSuppressed
        } else {
            CaptureDisposition::StoppedBeforeFirstFrame
        };
        let validation = if self.manifest.capture == CaptureDisposition::Recorded {
            self.manifest.validate_recorded_inventory()
        } else {
            Ok(())
        };
        self.manifest.failed_step = failed_step.map(str::to_owned);
        if validation.is_err() {
            self.manifest.capture = CaptureDisposition::Failed;
            self.manifest.failed_step = Some("Reconcile the closed recording inventory".into());
        }
        if self.manifest.capture == CaptureDisposition::Recorded {
            self.manifest.state = PackageState::Complete;
        }
        if self.manifest.state == PackageState::Complete {
            self.publish(authorize_complete)?;
        } else {
            let check_deadline = || {
                if deadline().is_some_and(|limit| Instant::now() >= limit) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "The recording deadline expired"));
                }
                Ok(())
            };
            check_deadline().map_err(|error| PackageError::io(PackageOperation::PublishManifest, error))?;
            self.publish(&check_deadline)?;
            check_deadline().map_err(|error| PackageError::io(PackageOperation::PublishManifest, error))?;
        }
        validation?;
        Ok(self.manifest.capture)
    }

    fn publish(&self, before_replace: impl FnOnce() -> io::Result<()>) -> PackageResult<()> {
        let pending = self.layout.root.join("manifest.pending.json");
        self.write_manifest(&pending)?;
        let manifest = self.layout.path(ArtifactKind::Manifest);
        replace_manifest(&pending, &manifest, before_replace)
            .map_err(|error| PackageError::io(PackageOperation::PublishManifest, error))
    }

    fn write_manifest(&self, path: &Path) -> PackageResult<()> {
        let mut file = File::create_new(path)
            .map_err(|e| PackageError::io(PackageOperation::CreateManifest, e))?;
        let encoded = serde_json::to_writer_pretty(&mut file, &self.manifest);
        encoded.map_err(|e| PackageError::io(PackageOperation::EncodeManifest, e.into()))?;
        file.write_all(b"\n").map_err(|e| PackageError::io(PackageOperation::EncodeManifest, e))?;
        if file.metadata().map_err(|e| PackageError::io(PackageOperation::EncodeManifest, e))?.len() > MAX_MANIFEST_BYTES {
            return Err(PackageError::io(PackageOperation::EncodeManifest,
                io::Error::new(io::ErrorKind::InvalidData, "The recording inventory exceeds the manifest size limit")));
        }
        file.sync_all().map_err(|e| PackageError::io(PackageOperation::FlushManifest, e))
    }

}

#[cfg(target_os = "windows")]
fn replace_manifest(
    pending: &Path, manifest: &Path, before_replace: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    crate::platform::windows::replace_manifest(pending, manifest, before_replace)
}

#[cfg(not(target_os = "windows"))]
fn replace_manifest(
    pending: &Path, manifest: &Path, before_replace: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    before_replace()?;
    fs::rename(pending, manifest)
}

/// Read-only view of an M1 attempt. Legacy exports retain their own versioned reader.
pub struct PackageReader {
    layout: AttemptLayout,
    manifest: AttemptManifest,
}

impl PackageReader {
    pub fn open(root: &Path) -> PackageResult<Self> {
        let file = File::open(root.join("manifest.json"))
            .map_err(|e| PackageError::io(PackageOperation::OpenManifest, e))?;
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)
            .map_err(|e| PackageError::io(PackageOperation::ReadManifest, e))?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(PackageError::InvalidManifest);
        }
        #[derive(Deserialize)]
        struct Version { version: u16 }
        let decode_error = |error: serde_json::Error| PackageError::io(PackageOperation::ReadManifest, error.into());
        let version = serde_json::from_slice::<Version>(&bytes).map_err(decode_error)?.version;
        if version != 4 && version != MANIFEST_VERSION {
            return Err(PackageError::UnsupportedVersion(version));
        }
        let manifest: AttemptManifest = serde_json::from_slice(&bytes).map_err(decode_error)?;
        if manifest.selected_displays.is_empty()
            || manifest.selected_displays.len() > crate::config::MAX_SELECTED_MONITORS
            || manifest.selected_displays.iter().enumerate().any(|(index, id)| manifest.selected_displays[..index].contains(id))
            || (manifest.state == PackageState::Complete) != (manifest.capture == CaptureDisposition::Recorded)
            || (manifest.capture == CaptureDisposition::Failed) != manifest.failed_step.is_some()
        {
            return Err(PackageError::InvalidManifest);
        }
        if manifest.version == MANIFEST_VERSION && manifest.state == PackageState::Complete && manifest.profile.is_some() {
            manifest.validate_recorded_inventory()?;
        }
        let layout = AttemptLayout { id: manifest.attempt_id.clone(), root: root.to_owned() };
        Ok(Self { layout, manifest })
    }

    pub fn manifest(&self) -> &AttemptManifest {
        &self.manifest
    }

    pub fn artifact_for_uri(&self, uri: &str) -> Option<ArtifactRef> {
        ArtifactRef::from_relative_uri(self.layout.id.clone(), uri)
    }

    pub fn open_artifact(&self, artifact: &ArtifactRef) -> PackageResult<BufReader<File>> {
        open_artifact(artifact.kind, self.artifact_path(artifact)?)
    }

    pub(crate) fn artifact_path(&self, artifact: &ArtifactRef) -> PackageResult<PathBuf> {
        if artifact.attempt != self.layout.id {
            return Err(PackageError::WrongAttempt);
        }
        if self.manifest.version == MANIFEST_VERSION {
            let listed = match artifact.kind {
                ArtifactKind::Journal(stream) if self.manifest.state == PackageState::Complete => {
                    self.manifest.streams.contains_key(&stream)
                }
                ArtifactKind::VideoSegment { monitor, segment } => self.manifest.monitor_video.iter()
                    .any(|video| video.monitor == monitor && video.segments.iter().any(|entry| entry.segment == segment)),
                _ => true,
            };
            if !listed {
                return Err(PackageError::ArtifactNotInventoried(artifact.kind));
            }
        }
        Ok(self.layout.path(artifact.kind))
    }
}

fn open_artifact(kind: ArtifactKind, path: PathBuf) -> PackageResult<BufReader<File>> {
    // Empty journals are legitimate. Media and the physical manifest must be nonempty.
    let meta = fs::symlink_metadata(&path).map_err(|e| PackageError::io(PackageOperation::InspectArtifact, e))?;
    if !meta.file_type().is_file() || (meta.len() == 0 && !matches!(kind, ArtifactKind::Journal(_))) {
        return Err(PackageError::InvalidArtifact(kind));
    }
    File::open(path).map(BufReader::new)
        .map_err(|e| PackageError::io(PackageOperation::OpenArtifact, e))
}
