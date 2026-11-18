//! The serialized schema of an attempt manifest and its inventory checks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    config::{ClipboardMode, DeviceCalibration, ResolvedCaptureConfig},
    protocol::{AdmissionEpoch, AttemptId, ClockOrigin, MonitorId, SegmentId, SessionTimeUs, StreamId, UtcAnchor},
};

use super::{
    error::{PackageError, PackageResult},
    journal::StreamSummary,
    outcome::{DroppedTotal, LossReason, SuppressionClass},
    reader::STREAMS,
    source::AdmissionBoundary,
};

/// The package format version (D-1 A-v6only): the manifest and every journal
/// schema. A reader opens only this version.
pub const MANIFEST_VERSION: u16 = 6;
pub(super) const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

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
    pub common_start_us: SessionTimeUs,
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
    pub admission_epoch: AdmissionEpoch,
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
    pub missing_by_reason: BTreeMap<MissingVideoSlot, u64>,
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
    /// The present-and-admitted interval being published. Live state only.
    #[serde(skip)]
    pub(crate) open_interval: Option<OpenInterval>,
}

/// The present-and-admitted interval (D-6 A) a monitor's slot rows are being published into.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenInterval {
    epoch: AdmissionEpoch,
    /// The start of its first slot.
    start: SessionTimeUs,
    /// It has an encoded slot.
    has_video: bool,
    /// It has a decided slot starting after `start` + G, so it was long enough for a first frame.
    long: bool,
}

impl MonitorVideo {
    /// Account one published slot row to the present-and-admitted intervals.
    pub(crate) fn note_interval(
        &mut self, epoch: AdmissionEpoch, present: bool, encoded: bool,
        slot_start: SessionTimeUs, grace_us: i64,
    ) {
        if self.open_interval.is_some_and(|open| !present || open.epoch != epoch) {
            self.close_interval();
        }
        if present {
            let open = self.open_interval.get_or_insert(OpenInterval {
                epoch, start: slot_start, has_video: false, long: false,
            });
            open.has_video |= encoded;
            open.long |= slot_start.0.saturating_sub(open.start.0) > grace_us;
        }
    }

    pub(crate) fn close_interval(&mut self) {
        if let Some(OpenInterval { has_video: false, long, .. }) = self.open_interval.take() {
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
    /// Image files in `images` that no journaled still row names in `file`
    /// or `seen_file`, plus revoked images whose removal failed. The files
    /// are kept.
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unavailable: BTreeMap<DisplayField, Option<i32>>,
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
    pub start_t_us: SessionTimeUs,
    pub end_t_us: SessionTimeUs,
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
    pub first_t_us: SessionTimeUs,
    pub cutoff_t_us: SessionTimeUs,
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
    pub epoch: AdmissionEpoch,
    /// The elevated collector's source generation, without process or window identifiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_generation: Option<std::num::NonZeroU64>,
    pub start_t_us: SessionTimeUs,
    /// Exclusive upper bound on the Recorder's retained source coverage.
    pub before_t_us: SessionTimeUs,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceTail {
    pub epoch: AdmissionEpoch,
    pub tail: UncertifiedInputTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceBlindReason {
    /// A source-transition pause without another pause reason.
    ElevationTransition,
    /// A pause of an elevated epoch, labelled with its actual reason.
    Pause(super::source::PauseReason),
    ElevatedOpening(crate::protocol::ipc::elevated::runtime::CoverageReason),
    FocusSuspended,
    RecorderStaging,
    SourceFailure,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceBlindInterval {
    pub epoch: AdmissionEpoch,
    pub start_t_us: SessionTimeUs,
    pub before_t_us: SessionTimeUs,
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
    pub failure: Option<super::source::SourceFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_failure: Option<super::source::SourceFailure>,
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
    pub epoch: AdmissionEpoch,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dropped_by_reason: BTreeMap<LossReason, DroppedTotal>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub suppressed_by_class: BTreeMap<SuppressionClass, u64>,
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
    pub first_t_us: Option<SessionTimeUs>,
}

/// Frames rows absent because a monitor's video worker was blocked in a
/// native call (§6.2 row 21, an attempt failure): the slots from `first_slot`,
/// whose rows would carry receipts from `after_t_us`, up to Stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndeliveredFrames {
    pub monitor: MonitorId,
    pub first_slot: u64,
    pub after_t_us: SessionTimeUs,
}

impl ActivityLoss {
    pub(super) fn required(&self, component: super::diagnostics::FailureComponent) -> bool {
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub streams: BTreeMap<StreamId, StreamHealth>,
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

pub(super) fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl HealthSummary {
    pub(crate) fn new(
        streams: &BTreeMap<StreamId, StreamSummary>, activity_losses: Vec<ActivityLoss>, omitted_health: u64,
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
    pub admission_epoch: AdmissionEpoch,
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
    pub streams: BTreeMap<StreamId, StreamSummary>,
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
    pub disk_preflight: Option<super::source::DiskPreflight>,
}

impl AttemptManifest {
    pub(super) fn validate_recorded_inventory(&self) -> PackageResult<()> {
        let inconsistent = || PackageError::InconsistentInventory;
        let clock = self.clock.as_ref().ok_or_else(inconsistent)?;
        let input = self.input_capture.as_ref().ok_or_else(inconsistent)?;
        if clock.stop.is_none() || clock.last_admitted_boundary.is_none()
            || clock.utc_anchors.first().is_none_or(|first| first.sample.qpc != clock.origin.origin_qpc)
            || input.blind.is_some() || input.uncertified_closing_tail.is_some()
            || self.monitor_video.len() != self.selected_displays.len()
            || self.streams.len() != STREAMS.len() || self.images.is_none() || self.cursors.is_none()
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
