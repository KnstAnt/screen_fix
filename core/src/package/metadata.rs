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

use super::{
    ArtifactKind, ArtifactRef,
    journal::StreamSummary,
    outcome::{DroppedTotal, LossReason, SuppressionClass},
};

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
    /// An M1-10 acceptance gate rule failed at a journal row (row 0: at the end).
    GateViolation { gate: u8, stream: StreamId, row: u64, reason: &'static str },
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
            Self::GateViolation { gate, stream, row, reason } =>
                write!(f, "Gate {gate} failed in the {stream:?} journal at row {row}: {reason}."),
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

/// Video bounds derived from one encoder format's effective frame rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCapacity {
    /// Pictures one segment can hold: (segment duration + rotation allowance) × fps + margin.
    pub segment_pictures: u64,
    /// Slot rows the video owner can stage: two segments, one awaiting
    /// verification and the open one.
    pub staged_slots: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentEncoding {
    pub admission_epoch: crate::protocol::AdmissionEpoch,
    pub first_frame: crate::protocol::FrameId,
    pub first_geometry: crate::protocol::GeometryEpoch,
    pub fps: u32,
    pub software: bool,
    /// The bitrate cap configured on the encoder, in bits per second.
    pub configured_peak_bitrate: u32,
    /// The mean bitrate configured on the output media type, in bits per second.
    pub configured_mean_bitrate: u32,
    #[serde(default)]
    pub capacity: VideoCapacity,
    /// The slot decision grace G (D-10 A): a slot is decided once a frame
    /// from a later slot arrives or G after its end.
    #[serde(default)]
    pub slot_decision_grace_us: u64,
    /// Hardware candidates rejected when this format was selected (VID-6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_encoders: Vec<RejectedEncoder>,
}

/// A hardware H.264 encoder candidate that did not qualify, with its cause.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedEncoder {
    /// The candidate's friendly name; none when discovery itself failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cause: std::borrow::Cow<'static, str>,
    pub native_code: Option<i32>,
}

/// A display property that could not be read for one monitor (VID-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayField {
    Dpi,
    Refresh,
    SdrWhiteLevel,
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
    /// The slot's frame arrived before the epoch timeline was published and
    /// could not be used when it was.
    TimelinePending,
    /// The slot's segment was finalized but its verification was rejected or
    /// did not complete in time; its `.part` is kept unpublished.
    SegmentUnverified,
}

/// Frames delivered to the video worker that no slot used. Together with the
/// capture counters, every captured frame has a counted outcome.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoCounters {
    pub indexed_encoded: u64,
    pub repeated: u64,
    pub changed_frame_arrived_late: u64,
    pub missing_by_reason: std::collections::BTreeMap<MissingVideoSlot, u64>,
    /// A newer frame of the same slot was selected.
    #[serde(default)]
    pub frames_superseded: u64,
    /// The frame's slot was already decided when it arrived.
    #[serde(default)]
    pub arrived_after_decision: u64,
    /// The frame belonged to an earlier admission epoch.
    #[serde(default)]
    pub frames_other_epoch: u64,
    /// The frame's geometry or approval no longer matched, it became
    /// available after its slot was decided, or its capture receipt preceded
    /// the previous frames row (the slot repeats instead, §6.5).
    #[serde(default)]
    pub frames_rejected: u64,
    /// Segment closures that waited for a free verifier slot.
    #[serde(default)]
    pub verifier_waits: u64,
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
    /// Present-and-admitted intervals (D-6 A) without an encoded slot. Slots
    /// of an absent monitor and approval-suppressed slots are outside them.
    #[serde(default)]
    pub intervals_without_video: u64,
    /// Of `intervals_without_video`, the intervals too short for a first
    /// frame: every slot starts within the slot decision grace G of the
    /// interval start. They are recorded but do not make the video unusable.
    #[serde(default)]
    pub short_intervals_without_video: u64,
    /// The interval being published: its admission epoch, first slot start,
    /// whether it has video and whether it has a decided slot starting after start + G.
    #[serde(skip)]
    pub(crate) open_interval: Option<(crate::protocol::AdmissionEpoch, crate::protocol::SessionTimeUs, bool, bool)>,
}

impl MonitorVideo {
    /// Account one published slot row to the present-and-admitted intervals.
    pub(crate) fn note_interval(
        &mut self, epoch: crate::protocol::AdmissionEpoch, present: bool, encoded: bool,
        slot_start: crate::protocol::SessionTimeUs, grace_us: i64,
    ) {
        if self.open_interval.is_some_and(|(open, ..)| !present || open != epoch) {
            self.close_interval();
        }
        if present {
            let (_, start, video, long) = self.open_interval.get_or_insert((epoch, slot_start, false, false));
            *video |= encoded;
            *long |= slot_start.0.saturating_sub(start.0) > grace_us;
        }
    }

    pub(crate) fn close_interval(&mut self) {
        if let Some((_, _, false, long)) = self.open_interval.take() {
            self.intervals_without_video = self.intervals_without_video.saturating_add(1);
            if !long {
                self.short_intervals_without_video = self.short_intervals_without_video.saturating_add(1);
            }
        }
    }

    /// The D-6 A rule: usable video in every present-and-admitted interval
    /// long enough for a first frame.
    pub(crate) fn usable(&self) -> bool {
        !self.unavailable && self.intervals_without_video == self.short_intervals_without_video
    }
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
    /// Images published after their still slot was resolved without them at
    /// the drain deadline. The files are kept and no row references them.
    #[serde(default)]
    pub unreferenced_images: u64,
}

/// One image file published by its owner. The canonical file name is its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageFile {
    /// The attempt-relative URI.
    pub file: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
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
    #[serde(default)]
    pub video_pressure: u64,
    /// Frames held for the epoch timeline and not used when it was published.
    #[serde(default)]
    pub timeline_pending: u64,
    /// Frames whose video conversion was discarded by a pause, a topology
    /// change, a rejected approval or an unavailable video worker.
    #[serde(default)]
    pub video_discarded: u64,
    /// Frames not converted for the software encoder because their video
    /// slot already had a converted frame: conversion runs at encoding pace.
    #[serde(default)]
    pub video_software_paced: u64,
}

/// Actual mode reported by one selected monitor's capture owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorCapture {
    pub device_path: String,
    pub geometry: crate::capture::frame::MonitorGeometry,
    pub capture_epoch: crate::protocol::CaptureEpoch,
    pub refresh_numerator: Option<u32>,
    pub refresh_denominator: Option<std::num::NonZeroU32>,
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
    /// Display properties left null, each with the native code of its failed read.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub unavailable: std::collections::BTreeMap<DisplayField, Option<i32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputBlindReason {
    HelperExited,
    /// A required Raw Input registration was taken or lost (INP-1, D-2 B).
    RegistrationLost,
    /// A required hook was confirmed or suspected lost (INP-12, D-2 B).
    HookLost,
    HelperCommunicationFailed,
    HelperCleanupFailed,
    /// A producer's progress stopped advancing for the stall bound.
    ProducerStall,
    /// The opening or post-gap held-state resync could not be performed.
    ResyncUnavailable,
    /// The receipt-time foreground was a higher-integrity process.
    ElevatedWindow,
    /// The receipt-time foreground was otherwise not accessible.
    ForegroundInaccessible,
    /// A device was removed; its held state is unknown until the next resync.
    DeviceRemoved,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<InputTailReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTailReason {
    ProducerProgress,
    ElevatedCoverage,
    ActivationPending,
    RequiredHook,
    SourceFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectedSourceKind {
    Ordinary,
    Elevated,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SelectedSourceInterval {
    pub source: SelectedSourceKind,
    pub epoch: crate::protocol::AdmissionEpoch,
    /// The elevated collector's source generation, without process or window identifiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_generation: Option<std::num::NonZeroU64>,
    pub start_t_us: crate::protocol::SessionTimeUs,
    /// Exclusive upper bound on the Recorder's retained source coverage.
    pub before_t_us: crate::protocol::SessionTimeUs,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceTail {
    pub epoch: crate::protocol::AdmissionEpoch,
    pub tail: UncertifiedInputTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceBlindReason {
    /// A source-transition pause without another pause reason.
    ElevationTransition,
    /// A pause of an elevated epoch, labelled with its actual reason.
    Pause(crate::session::PauseReason),
    ElevatedOpening(crate::protocol::ipc::elevated::runtime::CoverageReason),
    FocusSuspended,
    RecorderStaging,
    SourceFailure,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceBlindInterval {
    pub epoch: crate::protocol::AdmissionEpoch,
    pub start_t_us: crate::protocol::SessionTimeUs,
    pub before_t_us: crate::protocol::SessionTimeUs,
    pub reason: SourceBlindReason,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceEvidence {
    /// The ordinary selected source uses the inherited shared-memory channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<StandardSourceEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevated: Option<ElevatedSourceEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::session::SourceFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_failure: Option<crate::session::SourceFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intervals: Vec<SelectedSourceInterval>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncertified_tails: Vec<SourceTail>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blind: Vec<SourceBlindInterval>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StandardSourceEvidence {
    pub launched: bool,
    pub registration_reported: bool,
    pub required_registration_ready: bool,
    pub stopped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blind_reason: Option<InputBlindReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inactive_transport_failure: Option<InputBlindReason>,
}

/// Recorder-side milestones from protected launch, lane authentication and native registration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElevatedSourceEvidence {
    pub requested: bool,
    /// The UAC request reached Windows, and the shell call returned (E4).
    #[serde(default)]
    pub consent_dispatched: bool,
    #[serde(default)]
    pub consent_returned: bool,
    /// Launch requests withdrawn before dispatch (E4).
    #[serde(default)]
    pub withdrawn_launches: u32,
    pub authenticated: bool,
    pub configured: bool,
    pub stopped: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registrations: Vec<SourceRegistration>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceRegistration {
    pub epoch: crate::protocol::AdmissionEpoch,
    pub observed_qpc: crate::protocol::QpcTick,
    pub registration: crate::protocol::ipc::InputRegistration,
    /// The Armed held-state readiness, without bits (E4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<crate::protocol::ipc::elevated::runtime::HeldReadiness>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PenPressure {
    Captured,
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
    /// Extension: the pen HID opt-in; when false the pressure is `unavailable_unknown`.
    pub pen_hid_collection_enabled: bool,
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
    /// Optional collections that the periodic registration check found missing
    /// during the attempt. Startup availability is in `registration`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_unavailable: Vec<crate::protocol::ipc::OptionalCollection>,
}

/// Loss and suppression totals of one stream in the manifest health summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHealth {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub dropped_by_reason: std::collections::BTreeMap<LossReason, DroppedTotal>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub suppressed_by_class: std::collections::BTreeMap<SuppressionClass, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LostActivity {
    /// Input, or the video of one monitor (§6.2 L-required).
    Required,
    /// Stills, UIA, clipboard, cursor assets or identity resolution (§6.2 L-auxiliary).
    Auxiliary,
}

/// One activity that stopped for the rest of the attempt (§6.2 class L).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityLoss {
    pub component: super::diagnostics::FailureComponent,
    pub activity: LostActivity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorId>,
    pub operation: String,
    pub native_code: Option<i32>,
    /// First affected time, or the detection time when the first affected time is unknown.
    pub first_t_us: Option<crate::protocol::SessionTimeUs>,
}

/// Frames rows absent because a monitor's video worker was blocked in a
/// native call (§6.2 row 21, an attempt failure): the slots from `first_slot`,
/// whose rows would carry receipts from `after_t_us`, up to Stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndeliveredFrames {
    pub monitor: MonitorId,
    pub first_slot: u64,
    pub after_t_us: crate::protocol::SessionTimeUs,
}

impl ActivityLoss {
    fn required(&self, component: super::diagnostics::FailureComponent) -> bool {
        self.activity == LostActivity::Required && self.component == component
    }
}

/// Attempt health. The counts derive from the closed stream summaries and are
/// counts of persisted rows, so the reader can reconcile them with the journals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSummary {
    pub pauses: u64,
    /// Gap rows across all streams.
    pub gaps: u64,
    pub blind_intervals: u64,
    /// Unapproved-foreground suppression intervals and their total duration (C5, D-3 A).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub suppressed_intervals: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub suppressed_interval_us: u64,
    pub dropped_stills: u64,
    /// M1 never recovers a prefix (REL-3 is deferred).
    pub recovered_prefix: bool,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub streams: std::collections::BTreeMap<StreamId, StreamHealth>,
    /// Activity losses reported to the controller, in report order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_losses: Vec<ActivityLoss>,
    /// Capture and video health conditions beyond the runtime report bound,
    /// counted here instead of written as rows.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_health: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undelivered_frames: Vec<UndeliveredFrames>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl HealthSummary {
    pub(crate) fn new(
        streams: &std::collections::BTreeMap<StreamId, StreamSummary>, activity_losses: Vec<ActivityLoss>, omitted_health: u64,
        undelivered_frames: Vec<UndeliveredFrames>,
    ) -> Self {
        let mut health = Self { activity_losses, omitted_health, undelivered_frames, ..Self::default() };
        for (stream, summary) in streams {
            health.pauses = health.pauses.saturating_add(summary.pause_rows);
            health.gaps = health.gaps.saturating_add(summary.gap_rows);
            health.blind_intervals = health.blind_intervals.saturating_add(summary.blind_rows);
            health.suppressed_intervals = health.suppressed_intervals.saturating_add(summary.suppressed_interval_rows);
            health.suppressed_interval_us = health.suppressed_interval_us.saturating_add(summary.suppressed_interval_us);
            if *stream == StreamId::Stills {
                health.dropped_stills = summary.unavailable_by_reason.values()
                    .fold(health.dropped_stills, |total, count| total.saturating_add(*count));
            }
            if !summary.dropped_by_reason.is_empty() || !summary.suppressed_by_class.is_empty() {
                health.streams.insert(*stream, StreamHealth {
                    dropped_by_reason: summary.dropped_by_reason.clone(),
                    suppressed_by_class: summary.suppressed_by_class.clone(),
                });
            }
        }
        health
    }
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
    pub health: Option<HealthSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitor_video: Vec<MonitorVideo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_capture: Option<InputCapture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_registration: Option<crate::protocol::ipc::InputRegistration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence: Option<SourceEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png: Option<PngCounters>,
    /// Published still images (M1-09 output inventory), sorted by file. Absent
    /// when the owner could not report it; its images are then unchecked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageFile>>,
    /// Published cursor assets, as images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursors: Option<Vec<ImageFile>>,
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
            || self.streams.len() != 4 || self.images.is_none() || self.cursors.is_none()
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
            if candidates.next().is_some() || !video.usable()
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

/// The controller's fault outcome for one attempt.
pub(crate) struct AttemptFaults {
    /// The operation of the first attempt failure.
    pub(crate) failed_step: Option<&'static str>,
    pub(crate) activity_losses: Vec<ActivityLoss>,
    pub(crate) omitted_health: u64,
    pub(crate) undelivered_frames: Vec<UndeliveredFrames>,
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
                streams: Default::default(), health: None, monitor_video: Vec::new(), input_capture: None, input_registration: None, source_evidence: None, png: None, images: None, cursors: None,
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

    /// The files reported by the PNG and cursor asset owners.
    pub(crate) fn set_image_inventory(&mut self, images: Option<Vec<ImageFile>>, cursors: Option<Vec<ImageFile>>) {
        let sorted = |mut files: Vec<ImageFile>| {
            files.sort_by(|left, right| left.file.cmp(&right.file));
            files.dedup_by(|left, right| left.file == right.file);
            files
        };
        self.manifest.images = images.map(sorted);
        self.manifest.cursors = cursors.map(sorted);
    }

    pub(crate) fn set_source_evidence(&mut self, evidence: Option<SourceEvidence>) {
        self.manifest.source_evidence = evidence.map(|mut evidence| {
            evidence.failure = evidence.failure.map(crate::session::SourceFailure::without_report_detail);
            evidence.cleanup_failure = evidence.cleanup_failure.map(crate::session::SourceFailure::without_report_detail);
            evidence
        });
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
        let faults = AttemptFaults { failed_step, activity_losses: Vec::new(), omitted_health: 0, undelivered_frames: Vec::new() };
        self.finish_recording(faults, streams, video, || None, || {
            Err(io::Error::other("Complete publication requires authorization"))
        }).map(|_| ())
    }

    /// The disposition follows §6.2 rule 4: any attempt failure, then a
    /// required video loss or the selected-monitor rule, then a required input
    /// loss or an input-certification shortfall. Auxiliary activity losses and
    /// recorded losses never change it; they appear only in `health`.
    pub(crate) fn finish_recording(
        mut self,
        faults: AttemptFaults,
        streams: std::collections::BTreeMap<StreamId, StreamSummary>,
        video: Vec<MonitorVideo>,
        deadline: impl Fn() -> Option<Instant>,
        authorize_complete: impl FnOnce() -> io::Result<()>,
    ) -> PackageResult<CaptureDisposition> {
        use super::diagnostics::FailureComponent;

        let AttemptFaults { failed_step, activity_losses, omitted_health, undelivered_frames } = faults;
        let video_loss = activity_losses.iter().any(|loss| loss.required(FailureComponent::Video));
        let input_loss = activity_losses.iter().any(|loss| loss.required(FailureComponent::InputHelper));
        self.manifest.health = Some(HealthSummary::new(&streams, activity_losses, omitted_health, undelivered_frames));
        self.manifest.streams = streams;
        let any_video = video.iter().any(|monitor| monitor.encoded != 0);
        // D-6 A: a selected monitor needs video in each interval in which it
        // was present and admitted; its absence does not affect Complete.
        let video_unavailable = video_loss
            || (any_video && self.manifest.selected_displays.iter().any(|id| {
                !video.iter().any(|monitor| monitor.monitor == *id && monitor.usable())
            }));
        let input_shortfall = self.manifest.clock.is_some() && self.manifest.input_capture.as_ref()
            .is_none_or(|input| input.uncertified_closing_tail.is_some());
        let input_unavailable = input_loss || input_shortfall;
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
            // A successful replacement is final (C6): no deadline is checked after it.
            self.publish(&check_deadline)?;
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

/// Read-only view of a v5 attempt.
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
        if version != MANIFEST_VERSION {
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
        if manifest.health.as_ref().is_some_and(|health| {
            *health != HealthSummary::new(&manifest.streams, health.activity_losses.clone(), health.omitted_health,
                health.undelivered_frames.clone())
                || (manifest.state == PackageState::Complete
                    && (!health.undelivered_frames.is_empty()
                        || health.activity_losses.iter().any(|loss| loss.activity == LostActivity::Required)))
        }) {
            return Err(PackageError::InvalidManifest);
        }
        if let Some(source) = &manifest.source_evidence {
            source.validate(manifest.clock.as_ref().and_then(|clock| clock.stop.map(|stop| stop.time)))?;
        }
        if manifest.state == PackageState::Complete && manifest.profile.is_some() {
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
        let listed = match artifact.kind {
            ArtifactKind::Journal(stream) if self.manifest.state == PackageState::Complete => {
                self.manifest.streams.contains_key(&stream)
            }
            ArtifactKind::StillImage(_) | ArtifactKind::CursorImage(_) => self.image_entry(artifact).is_some(),
            ArtifactKind::VideoSegment { monitor, segment } => self.manifest.monitor_video.iter()
                .any(|video| video.monitor == monitor && video.segments.iter().any(|entry| entry.segment == segment)),
            _ => true,
        };
        if !listed {
            return Err(PackageError::ArtifactNotInventoried(artifact.kind));
        }
        Ok(self.layout.path(artifact.kind))
    }

    /// The output inventory entry of a still or cursor image: `None` when the
    /// image is not listed, `Some(None)` when its owner reported no inventory.
    pub(crate) fn image_entry(&self, artifact: &ArtifactRef) -> Option<Option<&ImageFile>> {
        let files = match artifact.kind {
            ArtifactKind::StillImage(_) => &self.manifest.images,
            ArtifactKind::CursorImage(_) => &self.manifest.cursors,
            _ => return None,
        };
        let Some(files) = files else { return Some(None) };
        let uri = artifact.relative_uri();
        files.binary_search_by(|entry| entry.file.as_str().cmp(uri.as_str())).ok().map(|index| Some(&files[index]))
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
