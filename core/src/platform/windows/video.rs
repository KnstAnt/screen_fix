//! Each selected monitor owns its encoder and locally verified MP4 files.

mod frames;
mod segment;

use std::{
    collections::{BTreeMap, VecDeque},
    ops::ControlFlow,
    sync::{Arc, PoisonError, RwLock, atomic::{AtomicBool, Ordering}, mpsc::{self, Receiver, SyncSender}},
    thread,
    time::{Duration, Instant},
};

use windows::Win32::Media::MediaFoundation::*;

use crate::{
    devlog::devlog,
    capture::frame::{ApprovalGeneration, FrameMetadata},
    config::SelectedMonitor,
    package::{
        ArtifactKind,
        diagnostics::{Failure, FailureComponent, ReportOutcome},
        metadata::{AttemptLayout, CaptureCounters, Geometry, MonitorCapture, MonitorVideo, RejectedEncoder, SegmentEncoding, VideoCapacity, VideoCounters},
        outcome::HealthKind,
    },
    protocol::{AdmissionEpoch, ClockOrigin, MonitorId, SegmentId, SessionTimeUs},
    platform::windows::controller::{Admission, AwakeElapsed, PausedVideo, SuspendWatch, WorkerSession},
    session::{AdmissionBoundary, fault::FaultReport},
};

use super::{context::{ApprovalReadError, ELEVATED_FRAME_WAIT_US, FramePolicy}, ProbeError, ProbeKind, RecordingReport, capture_device, clock, display, encode, encoder, gpu, h264, io_error, monitor, mp4, slots, stage, unavailable};

pub(super) use frames::Frames;
use frames::{Lane, Mark, RowKey};
use segment::{Closed, Segment, Verifying, withdraw_revoked};

const ROTATION_US: i64 = 60_000_000;
const IDR_ALLOWANCE_US: i64 = 1_000_000;
const US_PER_SECOND: u64 = 1_000_000;
/// Picture slack beyond the segment span, kept from the previous fixed bound.
const SEGMENT_PICTURE_MARGIN: u64 = 2;
/// Staged-slot slack beyond one segment, kept from the previous fixed bound.
const STAGED_SLOT_MARGIN: u64 = 4;
/// A segment's verification result is due one segment span after its media
/// ends (§6.2 row 19), so the verifier keeps pace with recording. The rows
/// awaiting it and the open segment's rows then fit two segments of staging.
const VERIFICATION_ALLOWANCE_US: u64 = (ROTATION_US + IDR_ALLOWANCE_US).unsigned_abs();
/// Finalized parts in flight to the verifier: one running and one queued.
const VERIFIER_QUEUE: usize = 2;
const VERIFIER_POLL: Duration = Duration::from_millis(5);

/// Bounds for one encoder format. A segment spans at most the rotation
/// period plus the IDR allowance at the effective frame rate.
pub(super) fn capacity_for(fps: u32) -> VideoCapacity {
    let span_us = ROTATION_US.unsigned_abs().saturating_add(IDR_ALLOWANCE_US.unsigned_abs());
    let segment_pictures = span_us.saturating_mul(u64::from(fps)).div_ceil(US_PER_SECOND)
        .saturating_add(SEGMENT_PICTURE_MARGIN);
    // A segment's rows stay staged until its verification result arrives
    // while the next segment records, so staging covers two segments.
    VideoCapacity { segment_pictures, staged_slots: segment_pictures.saturating_add(STAGED_SLOT_MARGIN).saturating_mul(2) }
}

/// Converted frames queued from the capture owner to the video worker.
pub(super) const OBSERVATION_QUEUE: usize = 2;
/// Frames the slot selection holds: the previous slot's frame, the
/// undecided slot's candidate and a later slot's watermark frame.
pub(super) const SELECTION_FRAMES: usize = 3;
/// Frames one decision step takes: the held watermark frame and the queue.
const COLLECT_BOUND: usize = OBSERVATION_QUEUE + 1;

#[derive(Clone)]
pub(super) struct Observation {
    pub input: encode::Input,
    pub metadata: FrameMetadata,
    pub available: SessionTimeUs,
    pub change: Option<f64>,
}

/// A frames lane was closed by its merge owner (§6.2 row 1).
const FRAMES_LANE_CLOSED: &str = "The frames merge owner stopped taking rows";
/// The worker's frames staging overflowed after a one-slot wait (§6.2 row 20).
const STAGING_OVERFLOW: &str = "The video frames staging exceeded its bound";

pub(super) struct Finished {
    pub video: MonitorVideo,
    pub capture: CaptureCounters,
    pub epochs: Vec<MonitorCapture>,
    pub outcome: Result<Option<AdmissionBoundary>, ProbeError>,
    pub required_finalization_failure: Option<&'static str>,
}

pub(super) fn run(
    monitor: &SelectedMonitor, worker: &WorkerSession, layout: &AttemptLayout,
    frames: &Frames, frame_policy: &FramePolicy, topology: &display::topology::Topology, borderless: bool, ready: SyncSender<monitor::Ready>,
    reports: Option<SyncSender<RecordingReport>>,
    png: super::still_png::Inbox, still_rows: super::still_rows::Rows,
) -> Result<Finished, ProbeError> {
    let lane = frames.lane(monitor.id)?;
    let _exit = ExitMark(&lane.mark);
    let _apartment = super::com_apartment()?;
    let display = display::resolve(&monitor.key)?;
    let luid = stage(unsafe { display.adapter.GetDesc1() }, "Read the display adapter")?.AdapterLuid;
    let device = capture_device(&display.adapter)?;
    let geometry = geometry_for(&display, monitor)?;
    let source = [geometry.source_size[0] as u32, geometry.source_size[1] as u32];
    let requested_fps = worker.config().profile().capture_pace().frames_per_second();
    // The capture owner starts after selection, so the startup self-check
    // pictures are converted here, while this thread alone holds the device.
    let encoder = encoder::select(&display.adapter, luid, source, requested_fps,
        &mut |encoding| encoder::probe_input(&device, encoding))
        .map_err(|failure| {
            report_rejected(worker, monitor.id, &failure.rejected);
            failure.last
        })?;
    let geometry = video_geometry(geometry, encoder.format);
    let mut summary = MonitorVideo {
        monitor: monitor.id, fps: encoder.format.fps, degraded: encoder.software(),
        slots: 0, encoded: 0, approval_suppressed: 0, unavailable: false, counters: Default::default(), segments: Vec::new(),
        intervals_without_video: 0, short_intervals_without_video: 0, open_interval: None,
    };
    let (send, receive) = mpsc::sync_channel(OBSERVATION_QUEUE);
    let (reconfigure, changes) = mpsc::sync_channel(1);
    let (probe, probes) = mpsc::sync_channel(1);
    let pacing = Arc::new(RwLock::new(None));
    let video_unavailable = Arc::new(AtomicBool::new(false));
    // Every format this monitor can later activate, hardware or software,
    // is bounded by the highest qualifiable frame rate.
    let evidence = Arc::new(slots::Evidence::new(capacity_for(encoder::max_effective_fps(requested_fps)).staged_slots));
    let video_input = monitor::VideoTarget {
        send, device, adapter: luid.into(), source_bounds: display.bounds, encoding: encoder.encoding(),
        unavailable: Arc::clone(&video_unavailable), evidence: Arc::clone(&evidence), reconfigure, probes,
        pacing: Arc::clone(&pacing),
    };
    let (parts, segments) = mpsc::sync_channel(VERIFIER_QUEUE);
    let (verdicts, results) = mpsc::sync_channel(VERIFIER_QUEUE);
    let adapter = display.adapter.clone();
    let loss = VideoLoss { worker, id: monitor.id, unavailable: &video_unavailable, reports: reports.as_ref() };
    thread::scope(|scope| {
        let capture = thread::Builder::new().name(format!("capture-monitor-{}", monitor.id.0))
            .spawn_scoped(scope, move || monitor::run(monitor, worker, frame_policy, topology, borderless, ready, video_input, png, still_rows))
            .map_err(|error| io_error("Start the monitor capture owner", error))?;
        // The verifier ends when the recording drops its part sender; the
        // scope joins it.
        thread::Builder::new().name(format!("verify-monitor-{}", monitor.id.0))
            .spawn_scoped(scope, move || mp4::verify_segments(adapter, segments, verdicts, || worker.segment_deadline()))
            .map_err(|error| io_error("Start the segment verifier", error))?;
        let recording = record(monitor, layout, frames, lane, frame_policy, topology, receive, changes,
            &pacing, encoder, geometry, &mut summary, loss, &evidence, parts, results, probe);
        // No further row of this worker can follow (§6.5 closing).
        lane.mark.exit();
        if let Err(error) = &recording {
            worker.report_fault(FaultReport::attempt(&error.to_error(FailureComponent::Video)));
        }
        let capture = capture.join().map_err(|_| unavailable("The monitor capture owner terminated unexpectedly"))?;
        let mut required_finalization_failure = None;
        let outcome = recording.and_then(|closing| {
            required_finalization_failure = closing.required_finalization_failure;
            let cutoff = capture.outcome?;
            if closing.cutoff != cutoff {
                return Err(unavailable("Capture and video closed at different boundaries"));
            }
            Ok(cutoff)
        });
        Ok(Finished { video: summary, capture: capture.counters, epochs: capture.epochs, outcome,
            required_finalization_failure })
    })
}

/// A worker that leaves `run` early, before or after recording, constrains
/// the frames merge no longer.
struct ExitMark<'a>(&'a Mark);

impl Drop for ExitMark<'_> {
    fn drop(&mut self) { self.0.exit(); }
}

/// Failures of the frames stream end the attempt (§6.2 rows 1 and 20);
/// every other video failure loses this monitor's video only.
fn fails_attempt(error: &ProbeError) -> bool {
    error.journal.is_some() || matches!(error.kind(), ProbeKind::FramesLaneClosed | ProbeKind::StagingOverflow)
}

fn geometry_for(display: &display::Monitor, monitor: &SelectedMonitor) -> Result<Geometry, ProbeError> {
    let (left, top, right, bottom) = display.bounds.edges();
    let size = display.size()?;
    Ok(Geometry { display_id: monitor.key.as_str().into(), desktop_rect: [left, top, right, bottom],
        source_size: [size.Width, size.Height], video_size: [0, 0], content_rect: None, video_scale: None })
}

fn video_geometry(mut geometry: Geometry, format: gpu::VideoFormat) -> Geometry {
    geometry.video_size = format.encoded;
    geometry.content_rect = Some([0, 0, format.content[0], format.content[1]]);
    geometry.video_scale = Some(f64::from(format.content[0].max(format.content[1]))
        / f64::from(format.source[0].max(format.source[1])));
    geometry
}

struct RecordedSlot {
    row: slots::Row,
    pts: i64,
    duration: i64,
    forced: bool,
}

struct Selection<'a> {
    receive: &'a Receiver<Observation>,
    /// The first received frame of a later slot: the source watermark.
    future: Option<Observation>,
    /// The newest eligible frame of the undecided slot.
    candidate: Option<Observation>,
    previous: Option<Observation>,
    missing: bool,
}

impl Selection<'_> {
    /// Take the received frames of the slot `[start, end)` until the first
    /// frame of a later slot. Frames arrive in source order, so that frame is
    /// the watermark after which the slot can receive nothing (D-10 A).
    /// Frames not selected are counted by cause.
    fn collect(
        &mut self, epoch: AdmissionEpoch, start: SessionTimeUs, end: SessionTimeUs,
        available_before: SessionTimeUs, accepts: impl Fn(&Observation) -> bool, counters: &mut VideoCounters,
    ) {
        let count = |counter: &mut u64| *counter = counter.saturating_add(1);
        for _ in 0..COLLECT_BOUND {
            let frame = match self.future.take() {
                Some(future) => future,
                None => match self.receive.try_recv() {
                    Ok(frame) => frame,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.missing = true;
                        break;
                    }
                },
            };
            let source = frame.metadata.source_t_us;
            if frame.metadata.admission != epoch {
                count(&mut counters.frames_other_epoch);
                continue;
            }
            // Conversion or queue delivery can outlive the capture slot.
            if source < start {
                count(&mut counters.arrived_after_decision);
                continue;
            }
            if source >= end {
                self.future = Some(frame);
                break;
            }
            if frame.available < available_before && accepts(&frame) {
                if self.candidate.replace(frame).is_some() {
                    count(&mut counters.frames_superseded);
                }
            } else {
                count(&mut counters.frames_rejected);
            }
        }
    }
}

/// The frames-lane publication state of one worker. It carries across
/// format changes and pauses: rows awaiting verification stay staged under
/// the bound they were staged with.
#[derive(Default)]
struct Publication {
    /// The first slot not yet in the frames lane.
    published_through: u64,
    /// Decided rows not yet in the lane: unresolved, or resolved while the
    /// lane is full.
    staged: BTreeMap<u64, slots::Row>,
    /// The staged row bound: this format's, or a larger earlier one while
    /// rows staged under it remain.
    staging_bound: u64,
    /// The frames receipt of the latest decided row (§6.5).
    last_receipt: Option<SessionTimeUs>,
    /// Paused slot ranges that the publication prefix skips.
    paused_slots: VecDeque<(u64, u64)>,
    /// Finalized segments in flight to the verifier, oldest first.
    verifying: VecDeque<Verifying>,
}

impl Publication {
    /// Begin an epoch at slot `next` under a format bound of `staged_slots`.
    /// With nothing staged, the prefix restarts there.
    fn begin_epoch(&mut self, next: u64, staged_slots: u64) {
        if self.staged.is_empty() {
            self.staging_bound = staged_slots;
            self.published_through = next;
            self.paused_slots.clear();
        } else {
            self.staging_bound = self.staging_bound.max(staged_slots);
        }
    }
}

/// The worker's slot and segment progress across epochs.
#[derive(Default)]
struct Progress {
    /// The next slot to decide.
    next: u64,
    /// The number of the last opened segment.
    segment_number: u32,
    required_finalization_failure: Option<&'static str>,
}

/// The parties told when this monitor's video is lost (L-required, §6.2 row 9).
#[derive(Clone, Copy)]
struct VideoLoss<'a> {
    worker: &'a WorkerSession,
    id: MonitorId,
    unavailable: &'a AtomicBool,
    reports: Option<&'a SyncSender<RecordingReport>>,
}

impl VideoLoss<'_> {
    /// This monitor's video can no longer be produced.
    fn lose(&self, summary: &mut MonitorVideo, error: &ProbeError) {
        summary.unavailable = true;
        self.unavailable.store(true, Ordering::Release);
        let code = error.source.as_ref().map(|source| source.code().0);
        devlog!("video", "lost m{} stage={:?} kind={:?} error={:?} hresult={:?}", self.id.0, error.stage, error.kind, error.to_string(),
            code.map(|code| format!("{:#010x}", code as u32)));
        if let Some(failure) = self.worker.video_failed(self.id, Failure::new(FailureComponent::Video, error.stage, code))
            && self.reports.is_none_or(|reports| reports.try_send(RecordingReport::Video(failure)).is_err())
        {
            self.worker.set_video_report(self.id, ReportOutcome::Unavailable { native_code: None });
        }
    }

    /// Release an encoder. A failed release loses this monitor's video only.
    fn release(&self, encoder: encoder::Encoder, summary: &mut MonitorVideo) {
        if let Err(error) = encoder.finish() {
            self.lose(summary, &error);
        }
    }
}

struct Video<'a> {
    id: MonitorId,
    worker: &'a WorkerSession,
    layout: &'a AttemptLayout,
    frames: &'a Frames,
    lane: &'a Lane,
    frame_policy: &'a FramePolicy,
    topology: &'a display::topology::Topology,
    changes: &'a Receiver<monitor::FormatRequest>,
    change: Option<monitor::FormatRequest>,
    change_ready: bool,
    evidence: &'a slots::Evidence,
    loss: VideoLoss<'a>,
    timeline: slots::Timeline,
    opening: AdmissionBoundary,
    cutoff: Option<AdmissionBoundary>,
    selection: Selection<'a>,
    outstanding: VecDeque<RecordedSlot>,
    headers: h264::Headers,
    current: Option<Segment>,
    force_pending: bool,
    summary: &'a mut MonitorVideo,
    publication: &'a mut Publication,
    progress: &'a mut Progress,
    parts: &'a SyncSender<mp4::SegmentToVerify>,
    verdicts: &'a Receiver<mp4::Verdict>,
    clock: ClockOrigin,
    preserve_rotation_part: bool,
    approval_generation: Option<ApprovalGeneration>,
    approval_break: bool,
    elevated: bool,
    conversion_wait: Option<SessionTimeUs>,
    geometry: Geometry,
    encoding: Option<gpu::Encoding>,
    /// Hardware candidates rejected when the active format was selected.
    rejected_encoders: Vec<RejectedEncoder>,
    /// Derived from the timeline's effective fps.
    capacity: VideoCapacity,
    /// The slot decision grace G, in microseconds.
    grace: i64,
    /// The suspends seen by the encoder loop.
    suspends: SuspendWatch,
}

struct Closing {
    cutoff: Option<AdmissionBoundary>,
    required_finalization_failure: Option<&'static str>,
}

/// The decision point of one slot: when it is decided, its end limited by
/// the cutoff, and the cutoff once admission closed.
#[derive(Clone, Copy)]
struct SlotDecision {
    slot: slots::Slot,
    at: SessionTimeUs,
    end: SessionTimeUs,
    cutoff: Option<SessionTimeUs>,
}

/// The admission of the worker's current epoch.
enum EpochAdmission {
    /// The epoch records; the session time now.
    Open(SessionTimeUs),
    /// The epoch closed at this boundary.
    Closed(AdmissionBoundary),
}

/// The slot decision grace G (D-10 A): one capture pacing interval plus the
/// conversion bound, capped at two slot intervals.
fn decision_grace(worker: &WorkerSession, fps: u32) -> i64 {
    let micros = |duration: Duration| i64::try_from(duration.as_micros()).unwrap_or(i64::MAX);
    let two_slots = 2 * 1_000_000 / i64::from(fps);
    micros(monitor::capture_interval(worker)).saturating_add(micros(monitor::CONVERSION_DEADLINE).min(two_slots))
}

/// One health row per hardware encoder candidate rejected by a selection (V4).
fn report_rejected(worker: &WorkerSession, id: MonitorId, rejected: &[RejectedEncoder]) {
    for rejected in rejected {
        let cause = match &rejected.cause {
            std::borrow::Cow::Borrowed(cause) => *cause,
            std::borrow::Cow::Owned(_) => "Qualify a hardware H.264 encoder",
        };
        worker.report_health(HealthKind::EncoderCandidateRejected,
            Failure::new(FailureComponent::Video, cause, rejected.native_code), Some(id), false);
    }
}

const ROTATION_IDR_MISSING: &str = "The H.264 encoder did not produce a verified rotation IDR within one second";
/// A segment interrupted during IDR rotation was not published by Stop.
const ROTATION_PART_UNPUBLISHED: &str = "Publish the segment interrupted during IDR rotation";
const ADMISSION_UNCLOSED: &str = "Video admission changed without closing its slot prefix";
/// The wait of an elevated segment for its input prefix closer, sleep excluded.
const INPUT_PREFIX_WAIT: Duration = Duration::from_secs(3);
const INPUT_PREFIX_POLL: Duration = Duration::from_millis(2);

/// The epoch opened at `opening` still records.
fn still_open(worker: &WorkerSession, opening: AdmissionBoundary) -> Result<bool, ProbeError> {
    Ok(matches!(worker.admit()?, Admission::Open(receipt) if receipt.boundary == opening))
}

/// One self-check picture converted by the capture owner (VID-12). An
/// admission change while waiting supersedes the format request.
fn capture_probe(
    probe: &SyncSender<encoder::Probe>, worker: &WorkerSession, opening: AdmissionBoundary, encoding: gpu::Encoding,
) -> Result<encode::Input, ProbeError> {
    let (reply, result) = mpsc::sync_channel(1);
    let mut request = encoder::Probe { encoding, reply };
    loop {
        match probe.try_send(request) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(returned)) if still_open(worker, opening)? => {
                request = returned;
                thread::sleep(VERIFIER_POLL);
            }
            Err(_) => return Err(encoder::interruption()),
        }
    }
    loop {
        match result.recv_timeout(VERIFIER_POLL) {
            Ok(input) => return input,
            Err(mpsc::RecvTimeoutError::Timeout) if still_open(worker, opening)? => {}
            Err(_) => return Err(encoder::interruption()),
        }
    }
}

fn record(
    monitor: &SelectedMonitor, layout: &AttemptLayout, frames: &Frames, lane: &Lane, frame_policy: &FramePolicy,
    topology: &display::topology::Topology, receive: Receiver<Observation>, changes: Receiver<monitor::FormatRequest>,
    pacing: &RwLock<Option<(AdmissionEpoch, slots::Timeline)>>, encoder: encoder::Encoder, mut geometry: Geometry,
    summary: &mut MonitorVideo, loss: VideoLoss<'_>, evidence: &slots::Evidence,
    parts: SyncSender<mp4::SegmentToVerify>, verdicts: Receiver<mp4::Verdict>, probe: SyncSender<encoder::Probe>,
) -> Result<Closing, ProbeError> {
    let worker = loss.worker;
    let Some(mut start) = worker.wait_for_start()? else {
        encoder.finish()?;
        return Ok(Closing { cutoff: None, required_finalization_failure: None });
    };
    let id = monitor.id;
    let mut fps = encoder.format.fps;
    let mut encoder = Some(encoder);
    let mut origin = start.boundary.time;
    let mut progress = Progress::default();
    let mut publication = Publication::default();
    loop {
        let capacity = capacity_for(fps);
        // The slot evidence is sized for the highest qualifiable frame rate.
        if capacity.staged_slots > evidence.slots() {
            return Err(unavailable("The video format exceeds the monitor's slot evidence bound"));
        }
        publication.begin_epoch(progress.next, capacity.staged_slots);
        let timeline = slots::Timeline::new(origin, fps)?.with_first(progress.next);
        *pacing.write().unwrap_or_else(PoisonError::into_inner) = Some((start.boundary.epoch, timeline));
        let mut state = Video {
            id, worker, layout, frames, lane, frame_policy, topology, changes: &changes, change: None, change_ready: false,
            evidence, loss, timeline,
            opening: start.boundary, cutoff: None,
            selection: Selection {
                receive: &receive, future: None, candidate: None, previous: None, missing: false,
            },
            grace: decision_grace(worker, fps),
            suspends: SuspendWatch::new(worker.suspend_marks()),
            outstanding: VecDeque::with_capacity(4), headers: h264::Headers::default(), current: None,
            force_pending: false, summary: &mut *summary, geometry: geometry.clone(),
            publication: &mut publication, progress: &mut progress, parts: &parts, verdicts: &verdicts, clock: start.clock,
            preserve_rotation_part: false,
            approval_generation: None, approval_break: false,
            elevated: worker.elevated_epoch(start.boundary.epoch), conversion_wait: None,
            encoding: encoder.as_ref().map(encoder::Encoder::encoding),
            rejected_encoders: encoder.as_ref().map(|encoder| encoder.rejected.clone()).unwrap_or_default(),
            capacity,
        };
        // The mark of this epoch lets the other monitors' rows below it pass.
        state.publish_resolved()?;
        if let Some(active) = &encoder {
            state.run_encoder(active)?;
        }
        state.close_open_segment()?;
        if state.summary.unavailable && let Some(active) = encoder.take() {
            loss.release(active, state.summary);
        }
        if encoder.is_none() && !state.change_ready {
            let reason = if state.summary.unavailable { slots::Missing::EncoderUnavailable } else { slots::Missing::MonitorMissing };
            state.complete_unavailable(reason)?;
        }
        let cutoff = state.cutoff;
        let transition = if state.change_ready {
            state.change.take()
        } else {
            if let Some(change) = state.change.take() {
                let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
            }
            None
        };
        let feedback = transition.as_ref().map(|_| state.timeline.slot(state.progress.next)).transpose()?;
        if let Some((change, boundary)) = transition.zip(feedback) {
            drop(state);
            origin = boundary.start;
            let target = FormatOwner { monitor, opening: start.boundary, probe: &probe, loss };
            if let Some(next_geometry) = target.change_format(change, &mut encoder, summary)? {
                geometry = next_geometry;
                fps = summary.fps;
            }
            continue;
        }
        let Some(closed) = cutoff else { return Err(unavailable("The video owner did not receive its closing boundary")) };
        let boundary = PausedVideo { monitor: id, first_paused_slot: state.progress.next, fps };
        // Every decided row of the closed epoch is in the lane, or the mark
        // keeps constraining at the first that is not (§6.5 closing).
        state.publish_resolved()?;
        if state.publication.staged.is_empty() {
            lane.mark.close(state.opening.epoch);
        }
        // A pause reports its decided prefix; verification can finish after it.
        worker.finish_video_pause(closed, boundary);
        let reopened = if worker.pause_pending(closed) { worker.wait_for_reopen(closed)? } else { None };
        let Some(opened) = reopened else {
            state.settle_verification()?;
            state.drain_lane()?;
            let complete = state.publication.published_through == state.progress.next && state.publication.staged.is_empty();
            state.summary.close_interval();
            drop(state);
            if !complete {
                return Err(unavailable("The video slot journal does not cover the complete admitted prefix"));
            }
            if let Some(active) = encoder.take() {
                loss.release(active, summary);
            }
            return Ok(Closing { cutoff, required_finalization_failure: progress.required_finalization_failure });
        };
        state.freeze_late();
        drop(state);
        progress.next = boundary.end_exclusive(closed.time, opened.boundary.time)
            .ok_or_else(|| unavailable("The paused video slot range overflowed"))?;
        publication.paused_slots.push_back((boundary.first_paused_slot, progress.next));
        start = opened;
        origin = start.boundary.time;
    }
}

/// The parties of a format change (VID-9): the video owner of one monitor
/// and the capture owner that requested it.
struct FormatOwner<'a> {
    monitor: &'a SelectedMonitor,
    opening: AdmissionBoundary,
    probe: &'a SyncSender<encoder::Probe>,
    loss: VideoLoss<'a>,
}

impl FormatOwner<'_> {
    /// Apply a format change at its boundary slot: release the old encoder,
    /// select one for the new output and reply to the capture owner. Returns
    /// the new video geometry once an encoder is active; `summary.fps` then
    /// holds its frame rate.
    fn change_format(
        &self, change: monitor::FormatRequest, encoder: &mut Option<encoder::Encoder>, summary: &mut MonitorVideo,
    ) -> Result<Option<Geometry>, ProbeError> {
        let monitor::FormatRequest { output, device, reply, .. } = change;
        let (decided, geometry) = self.decide(output, device, encoder, summary)?;
        let _ = reply.try_send(decided);
        Ok(geometry)
    }

    /// The reply to a format request, and the new video geometry once an
    /// encoder is active.
    fn decide(
        &self, output: Option<display::topology::Output>, device: Option<windows::Win32::Graphics::Direct3D11::ID3D11Device>,
        encoder: &mut Option<encoder::Encoder>, summary: &mut MonitorVideo,
    ) -> Result<(monitor::FormatReply, Option<Geometry>), ProbeError> {
        use monitor::FormatReply;
        let (worker, id) = (self.loss.worker, self.monitor.id);
        if !still_open(worker, self.opening)? {
            return Ok((FormatReply::ChangedAgain, None));
        }
        if let Some(old) = encoder.take() {
            self.loss.release(old, summary);
        }
        let (Some(output), Some(device)) = (output, device) else {
            return Ok((FormatReply::Missing, None));
        };
        // A failed display read proves nothing: the capture owner retries
        // on its next topology tick (VID-9).
        let Some(actual) = display::resolve_matching(&self.monitor.key, &output) else {
            return Ok((FormatReply::ChangedAgain, None));
        };
        if self.loss.unavailable.load(Ordering::Acquire) {
            // A known encoder failure cannot be retried by topology.
            // The WGC owner can still rebuild its native-only resources.
            return Ok((FormatReply::Ready { device, encoding: None, output }, None));
        }
        let next_geometry = geometry_for(&actual, self.monitor)?;
        let source = [next_geometry.source_size[0] as u32, next_geometry.source_size[1] as u32];
        let requested_fps = worker.config().profile().capture_pace().frames_per_second();
        // The matching output carries the adapter LUID read from `actual`.
        let selected = encoder::select(&actual.adapter, output.adapter.into(), source, requested_fps,
            &mut |encoding| capture_probe(self.probe, worker, self.opening, encoding));
        let activated = match selected {
            Ok(activated) => activated,
            Err(failure) if failure.last.kind() == ProbeKind::ProbeInterrupted => {
                return Ok((FormatReply::ChangedAgain, None));
            }
            Err(failure) => {
                // No encoder qualifies for the new format: this monitor's
                // video is lost (§6.2 row 9); capture continues.
                report_rejected(worker, id, &failure.rejected);
                self.loss.lose(summary, &failure.last);
                return Ok((FormatReply::Ready { device, encoding: None, output }, None));
            }
        };
        report_rejected(worker, id, &activated.rejected);
        let encoding = activated.encoding();
        summary.fps = encoding.format.fps;
        summary.degraded |= encoding.software;
        monitor::report_video_ready(worker, id, encoding);
        *encoder = Some(activated);
        Ok((FormatReply::Ready { device, encoding: Some(encoding), output }, Some(video_geometry(next_geometry, encoding.format))))
    }
}

impl encode::EncodeOwner for Video<'_> {
    fn observe_admission(&mut self) -> Result<bool, ProbeError> {
        if let Some(lost) = self.poll_verification(Duration::ZERO)? {
            return Err(lost);
        }
        // A pause boundary can span sleep (D-5 A); the encoder watchdog restarts.
        let slept = self.suspends.slept(self.worker.suspend_marks());
        if self.change.is_none() {
            self.change = self.changes.try_recv().ok();
        }
        let at = match self.admission(ADMISSION_UNCLOSED)? {
            EpochAdmission::Open(now) => now,
            EpochAdmission::Closed(boundary) => {
                if self.stop_expired() {
                    return Err(unavailable("The video segment drain deadline expired"));
                }
                boundary.time
            }
        };
        if !self.approval_break && self.preserve_rotation_part && self.rotation_overdue(at) {
            return Err(unavailable(ROTATION_IDR_MISSING));
        }
        Ok(slept)
    }

    fn next_frame(&mut self) -> Result<encode::Next, ProbeError> {
        let slot = self.timeline.slot(self.progress.next)?;
        if self.change_at_slot(slot) {
            return Ok(encode::Next::End);
        }
        let decision = match self.admit_slot(slot)? {
            ControlFlow::Continue(decision) => decision,
            ControlFlow::Break(next) => return Ok(next),
        };
        let SlotDecision { at, end, cutoff, .. } = decision;
        let approval = match self.frame_policy.try_current(self.opening.epoch, at) {
            Ok(approval) => approval,
            Err(ApprovalReadError::NewerThanReceipt) if cutoff.is_some() => None,
            Err(_) => return Ok(encode::Next::Wait),
        };
        let expiry = SessionTimeUs(end.0.saturating_add(ELEVATED_FRAME_WAIT_US));
        let may_wait = self.elevated && cutoff.is_none() && at < expiry;
        if may_wait && approval.as_ref().is_some_and(|approval| approval.awaits_slot(slot.start, end)) {
            return Ok(encode::Next::Wait);
        }
        let approval = approval.filter(|approval| approval.contains_slot(slot.start, end));
        if self.approval_generation != approval.as_ref().map(|approval| approval.generation) {
            self.approval_break |= self.approval_generation.is_some();
            self.approval_generation = approval.as_ref().map(|approval| approval.generation);
        }
        if self.approval_break {
            if !self.outstanding.is_empty() {
                return Ok(encode::Next::DrainPrefix);
            }
            self.finish_approval_prefix()?;
        }
        if self.rotation_overdue(slot.start) {
            self.preserve_rotation_part = true;
            return Err(unavailable(ROTATION_IDR_MISSING));
        }
        let Ok(current) = self.topology.capture_geometry(self.id) else { return Ok(encode::Next::Wait) };
        let geometry = current.map(|current| current.id);
        let approved = |frame: &Observation| approval.as_ref().is_some_and(|approval| approval.matches(&frame.metadata));
        let accepts = |frame: &Observation| geometry == Some(frame.metadata.geometry.id) && approved(frame);
        // Within one video format, a geometry epoch change keeps the pixel size
        // and format (DPI or position, ALG §9.3), so the repeat source survives
        // it and keeps its own geometry version. Size, rotation and format
        // changes reconfigure the encoder with a new selection.
        let repeatable = |frame: &Observation| approved(frame) && current.is_some_and(|current| {
            current.id.monitor == frame.metadata.geometry.id.monitor
                && current.bounds.size() == frame.metadata.geometry.bounds.size()
        });
        if self.selection.previous.as_ref().is_some_and(|frame| !repeatable(frame)) {
            self.selection.previous = None;
        }
        // A frame is eligible when it became available before the decision.
        let available_before = if self.elevated { expiry } else { at };
        self.selection.collect(self.opening.epoch, slot.start, end, available_before, accepts, &mut self.summary.counters);
        // Decide when a later slot's frame arrived or the grace elapsed (D-10 A).
        let decision_due = SessionTimeUs(end.0.saturating_add(self.grace));
        if cutoff.is_none() && self.selection.future.is_none() && !self.selection.missing && at < decision_due {
            return Ok(encode::Next::Wait);
        }
        let mut fresh = self.selection.candidate.take();
        // A frame received before the previous row's receipt would break the
        // frames order (§6.5). It is counted as rejected and the slot repeats.
        if fresh.as_ref().is_some_and(|frame| self.publication.last_receipt.is_some_and(|last| frame.metadata.recv_t_us < last)) {
            fresh = None;
            self.summary.counters.frames_rejected = self.summary.counters.frames_rejected.saturating_add(1);
        }
        let repeated = fresh.is_none();
        let captured = fresh.as_ref().map(|frame| frame.metadata.recv_t_us);
        if let Some(frame) = fresh {
            self.selection.previous = Some(frame);
        } else if may_wait && approval.is_some() && self.selection.future.is_none() && !self.selection.missing
            && self.evidence.pressure(slot.index).is_none()
        {
            // Give the monitor one slot period to finish the conversion that
            // became eligible with this certificate. The slot and PTS stay fixed.
            let period = slot.end.0 - slot.start.0;
            let until = *self.conversion_wait.get_or_insert(SessionTimeUs(at.0.saturating_add(period)).min(expiry));
            if at < until { return Ok(encode::Next::Wait); }
        }
        self.conversion_wait = None;
        let receipt = self.receipt(end, captured);
        let source = self.selection.previous.as_ref().map(|frame| slots::Source {
            metadata: &frame.metadata, change: frame.change, repeated,
        });
        let mut row = slots::Row::pending(self.id, self.opening.epoch, slot, at, receipt, source,
            (repeated && source.is_some()).then(|| self.evidence.late(slot.index)));
        row.limit_coverage(end);
        self.progress.next = self.progress.next.checked_add(1).ok_or_else(|| unavailable("The video slot count overflowed"))?;
        self.classify(&mut row, decision, approval.is_some(), geometry.is_some(), decision_due, expiry, repeated);
        if row.reason.is_some() {
            self.stage_slot(row)?;
            return Ok(encode::Next::Wait);
        }
        self.submit_slot(row, slot, cutoff)
    }

    fn prefix_drained(&mut self) -> Result<(), ProbeError> {
        self.finish_approval_prefix()
    }

    fn output(&mut self, transform: &IMFTransform, sample: &IMFSample) -> Result<usize, ProbeError> {
        let pts = stage(unsafe { sample.GetSampleTime() }, "Read the encoded picture timestamp")?;
        let mut retired = 1;
        while self.outstanding.front().is_some_and(|pending| pending.pts < pts) {
            let Some(mut skipped) = self.outstanding.pop_front() else { break };
            self.force_pending &= !skipped.forced;
            skipped.row.miss(slots::Missing::EncoderSkipped);
            self.resolve(skipped.row)?;
            retired += 1;
        }
        let mut pending = self.outstanding.pop_front().filter(|pending| pending.pts == pts)
            .ok_or_else(|| unavailable("The encoder produced a frame outside the submitted slot prefix"))?;
        self.force_pending &= !pending.forced;
        if pending.row.revoked() {
            pending.row.withdraw(slots::Missing::ApprovalSuppressed);
            self.resolve(pending.row)?;
            return Ok(retired);
        }
        let boundary_due = self.segment_age(pending.row.pts_us).is_none_or(|age| age >= ROTATION_US);
        if boundary_due && self.current.is_some() {
            self.preserve_rotation_part = true;
        }
        let media = if boundary_due {
            let media = stage(unsafe { transform.GetOutputCurrentType(0) }, "Read the H.264 media type")?;
            self.headers.read_media_type(&media)?;
            Some(media)
        } else {
            None
        };
        let picture = self.headers.inspect(sample)?;
        if picture.b_slice {
            return Err(unavailable("The H.264 encoder returned a B-frame"));
        }
        if let Some(media) = media {
            if picture.idr {
                let sequence = self.headers.segment_header(&picture)?;
                // Keep ownership of the boundary sample and its actual parameter
                // sets while the prior muxer finishes synchronously on this worker.
                // Capture keeps only its bounded queue until we resume consumption.
                self.preserve_rotation_part = false;
                self.finish_current()?;
                self.progress.segment_number = self.progress.segment_number.checked_add(1)
                    .ok_or_else(|| unavailable("The MP4 segment index overflowed"))?;
                let id = SegmentId(self.progress.segment_number);
                let path = self.layout.path(ArtifactKind::VideoSegment { monitor: self.id, segment: id });
                stage(unsafe { media.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &sequence) },
                    "Attach verified SPS and PPS to the new MP4 stream")?;
                let gpu::Encoding { format, software } = self.encoding.ok_or_else(|| unavailable("The segment encoding mode is unavailable"))?;
                let encoding = SegmentEncoding {
                    admission_epoch: pending.row.admission_epoch,
                    first_frame: pending.row.frame_id.ok_or_else(|| unavailable("The segment first frame has no capture identity"))?,
                    first_geometry: pending.row.geometry_version.ok_or_else(|| unavailable("The segment first frame has no geometry identity"))?,
                    fps: format.fps, software,
                    configured_peak_bitrate: format.bitrate(),
                    configured_mean_bitrate: format.mean_bitrate(),
                    capacity: capacity_for(format.fps),
                    slot_decision_grace_us: self.grace.unsigned_abs(),
                    rejected_encoders: self.rejected_encoders.clone(),
                };
                self.current = Some(Segment::open(id, pending.row.pts_us, pts, encoding, mp4::Recording::open(&media, &path)?,
                    pending.row.coverage.clone()));
                devlog!("video", "segment opened m{} s{} start_us={} epoch={} software={software}",
                    self.id.0, id.0, pending.row.pts_us.0, pending.row.admission_epoch.0);
            } else if self.current.is_none() {
                return Err(unavailable("The first encoded frame is not an IDR"));
            }
        }
        self.current.as_mut().ok_or_else(|| unavailable("The MP4 segment is unavailable"))?
            .push(pending, sample, picture.idr)?;
        Ok(retired)
    }
}

impl Video<'_> {
    /// Encode this epoch's slots until it closes or the format changes. A
    /// failure of the frames stream ends the attempt; any other loses this
    /// monitor's video.
    fn run_encoder(&mut self, active: &encoder::Encoder) -> Result<(), ProbeError> {
        let encoded = encode::record(&active.transform, active.device.as_ref(), self);
        match encoded {
            Err(error) if fails_attempt(&error) => return Err(error),
            Err(error) => self.loss.lose(self.summary, &error),
            Ok(_) if self.preserve_rotation_part => {
                self.loss.lose(self.summary, &unavailable("The H.264 encoder drained without a verified rotation IDR"));
            }
            Ok(_) => {}
        }
        Ok(())
    }

    /// Resolve the inputs the encoder did not return and close the open
    /// segment. A segment interrupted during IDR rotation is not published:
    /// its `.part` stays and the lost video resolves its rows. It is a
    /// required finalization failure only once the Stop deadline expired
    /// (§6.2 rows 9 and 2).
    fn close_open_segment(&mut self) -> Result<(), ProbeError> {
        self.skip_outstanding(if self.summary.unavailable { slots::Missing::EncoderUnavailable } else { slots::Missing::EncoderSkipped })?;
        if self.preserve_rotation_part {
            if let Some(segment) = self.current.take() {
                devlog!("video", "segment interrupted during IDR rotation m{} s{} left unpublished", self.id.0, segment.id.0);
                if self.elevated {
                    segment.discard()?;
                }
                if self.stop_expired() {
                    self.progress.required_finalization_failure.get_or_insert(ROTATION_PART_UNPUBLISHED);
                }
            }
        } else if let Err(error) = self.finish_current() {
            if fails_attempt(&error) {
                return Err(error);
            }
            self.loss.lose(self.summary, &error);
        }
        Ok(())
    }

    /// This epoch's admission. A closing of this epoch records the cutoff;
    /// any other admission fails with `otherwise`.
    fn admission(&mut self, otherwise: &'static str) -> Result<EpochAdmission, ProbeError> {
        match self.worker.admit()? {
            Admission::Open(receipt) if receipt.boundary == self.opening => {
                Ok(EpochAdmission::Open(receipt.clock.time(receipt.qpc)?))
            }
            Admission::Closed(Some(boundary)) if boundary.epoch == self.opening.epoch => {
                self.cutoff = Some(boundary);
                Ok(EpochAdmission::Closed(boundary))
            }
            _ => Err(unavailable(otherwise)),
        }
    }

    /// The open segment's span from its start to `at`.
    fn segment_age(&self, at: SessionTimeUs) -> Option<i64> {
        self.current.as_ref().map(|segment| at.0.saturating_sub(segment.start.0))
    }

    fn rotation_overdue(&self, at: SessionTimeUs) -> bool {
        self.segment_age(at).is_some_and(|age| age > ROTATION_US + IDR_ALLOWANCE_US)
    }

    /// The decision point of `slot`, or the encoder step while it cannot be
    /// decided: an open epoch decides a slot once it ended, a closed one at
    /// its cutoff.
    fn admit_slot(&mut self, slot: slots::Slot) -> Result<ControlFlow<encode::Next, SlotDecision>, ProbeError> {
        let decision = match self.admission(ADMISSION_UNCLOSED)? {
            EpochAdmission::Open(now) => {
                if now < slot.end {
                    return Ok(ControlFlow::Break(encode::Next::Wait));
                }
                SlotDecision { slot, at: now, end: slot.end, cutoff: None }
            }
            EpochAdmission::Closed(boundary) => {
                if slot.start >= boundary.time {
                    return Ok(ControlFlow::Break(encode::Next::End));
                }
                SlotDecision { slot, at: boundary.time, end: slot.end.min(boundary.time), cutoff: Some(boundary.time) }
            }
        };
        Ok(ControlFlow::Continue(decision))
    }

    /// Record why a decided slot has no encoded picture, if it has none.
    fn classify(
        &mut self, row: &mut slots::Row, decision: SlotDecision, approved: bool, geometry_known: bool,
        decision_due: SessionTimeUs, expiry: SessionTimeUs, repeated: bool,
    ) {
        let SlotDecision { slot, at, cutoff, .. } = decision;
        if !approved || row.revoked() {
            self.summary.approval_suppressed += 1;
            row.withdraw(slots::Missing::ApprovalSuppressed);
        } else if !geometry_known || (self.selection.missing && cutoff.is_none()) {
            row.miss(slots::Missing::CaptureUnavailable);
        } else if !self.elevated && decision_due.0 < at.0.saturating_sub(slot.end.0 - slot.start.0)
            || self.elevated && at > expiry
        {
            row.miss(slots::Missing::QueuePressure);
        } else if repeated && let Some(pressure) = self.evidence.pressure(slot.index) {
            row.miss(pressure);
        } else if self.selection.previous.is_none() {
            row.miss(slots::Missing::BeforeFirstFrame);
        }
    }

    /// Stage a slot with a picture and submit that picture to the encoder,
    /// forcing an IDR when the open segment is due to rotate.
    fn submit_slot(&mut self, row: slots::Row, slot: slots::Slot, cutoff: Option<SessionTimeUs>) -> Result<encode::Next, ProbeError> {
        let input = self.selection.previous.as_ref()
            .ok_or_else(|| unavailable("The selected video slot has no input frame"))?.input.clone();
        let pts = slot.pts()?;
        let duration = slot.duration(cutoff)?;
        let due = self.segment_age(slot.start).is_none_or(|age| age >= ROTATION_US);
        let force_idr = due && !self.force_pending;
        self.force_pending |= force_idr;
        if self.current.is_some() && force_idr {
            self.preserve_rotation_part = true;
        }
        self.stage_slot(row.clone())?;
        self.outstanding.push_back(RecordedSlot { row, pts, duration, forced: force_idr });
        Ok(encode::Next::Frame(encode::Picture { input, pts, duration, force_idr }))
    }

    /// Inputs the encoder did not produce become explicit missing slots.
    fn skip_outstanding(&mut self, reason: slots::Missing) -> Result<(), ProbeError> {
        while let Some(mut skipped) = self.outstanding.pop_front() {
            skipped.row.miss(reason);
            self.resolve(skipped.row)?;
        }
        Ok(())
    }

    fn finish_approval_prefix(&mut self) -> Result<(), ProbeError> {
        // The MFT either had no pending input or completed its drain.
        self.skip_outstanding(slots::Missing::EncoderSkipped)?;
        self.finish_current()?;
        self.approval_break = false;
        self.force_pending = false;
        self.preserve_rotation_part = false;
        Ok(())
    }

    fn finish_current(&mut self) -> Result<(), ProbeError> {
        if self.elevated && self.current.is_some()
            && let Admission::Closed(Some(closed)) = self.worker.admit()?
            && closed.epoch == self.opening.epoch
        {
            // Stop can deliver the last native batch to the existing input
            // prefix closer. Do not publish an open part ahead of that result.
            let mut deadline = Instant::now() + INPUT_PREFIX_WAIT;
            // A local sequence: `self.suspends` stays for the encoder watchdog.
            let mut seen = SuspendWatch::new(self.worker.suspend_marks());
            while !self.worker.input_prefix_closed(closed) {
                // Time asleep does not count against the prefix wait (D-5 A).
                if seen.slept(self.worker.suspend_marks()) {
                    deadline = Instant::now() + INPUT_PREFIX_WAIT;
                }
                if Instant::now() >= self.worker.segment_deadline().map_or(deadline, |limit| limit.min(deadline)) {
                    if let Some(segment) = self.current.take() { segment.discard()?; }
                    let stage = "The elevated video prefix did not close before publication";
                    self.progress.required_finalization_failure.get_or_insert(stage);
                    return Err(unavailable(stage));
                }
                thread::park_timeout(INPUT_PREFIX_POLL);
            }
        }
        if let Some(segment) = self.current.take() {
            let segment_id = segment.id.0;
            let closed = match segment.close(self.id, self.geometry.clone(), self.worker, self.clock) {
                Ok(closed) => closed,
                Err(error) => {
                    devlog!("video", "segment close failed m{} s{segment_id} stage={:?} kind={:?} error={:?}", self.id.0, error.stage, error.kind,
                        error.to_string());
                    // Only the Stop deadline makes an unclosed segment a
                    // required finalization failure; any other close
                    // failure loses this monitor's video (§6.2 rows 9 and 2).
                    if error.kind() == ProbeKind::PublicationDeadline || self.stop_expired() {
                        self.progress.required_finalization_failure.get_or_insert(error.stage);
                    }
                    return Err(error);
                }
            };
            match closed {
                Closed::Resolved(rows) => {
                    devlog!("video", "segment closed m{} s{segment_id} resolved rows={}", self.id.0, rows.len());
                    let last = self.last_row(&rows);
                    self.resolve_all(rows)?;
                    self.frames.request_flush(last);
                    self.worker.request_journal_flush();
                }
                Closed::Part { pending, part, suppressed } => {
                    devlog!("video", "segment closed m{} s{segment_id} part handed to verifier", self.id.0);
                    self.resolve_all(suppressed)?;
                    self.hand_off(pending, part)?;
                }
            }
        }
        Ok(())
    }

    /// No frame of a closed epoch arrives after its pause, so its late flags
    /// are final. Fix them before carrying the rows: across the paused range
    /// their evidence cells can be reused by later slots.
    fn freeze_late(&mut self) {
        let publication = &mut *self.publication;
        let rows = publication.staged.values_mut()
            .chain(publication.verifying.iter_mut().flat_map(|entry| entry.index.iter_mut()));
        for row in rows.filter(|row| row.repeated == Some(true)) {
            row.late = Some(self.evidence.late(row.slot));
        }
    }

    fn stop_expired(&self) -> bool {
        self.worker.segment_deadline().is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Hand a finalized part to the verifier. With two parts in flight the
    /// worker waits for a verifier slot, bounded by the oldest part's
    /// allowance; capture continues meanwhile and its slots record queue
    /// pressure (ALG §9.2). Encoding resumes once the part is handed off.
    fn hand_off(&mut self, pending: Verifying, part: mp4::SegmentToVerify) -> Result<(), ProbeError> {
        let mut lost = None;
        if self.publication.verifying.len() >= VERIFIER_QUEUE {
            self.summary.counters.verifier_waits += 1;
        }
        while self.publication.verifying.len() >= VERIFIER_QUEUE
            && !self.publication.verifying.iter().any(Verifying::abandoned) && !self.stop_expired()
        {
            lost = lost.or(self.poll_verification(VERIFIER_POLL)?);
        }
        if self.publication.verifying.len() < VERIFIER_QUEUE && !self.stop_expired() && self.parts.try_send(part).is_ok() {
            self.publication.verifying.push_back(pending);
            return lost.map_or(Ok(()), Err);
        }
        // An overdue part still holds the verifier, the Stop deadline expired,
        // or the verifier exited: this part stays `.part`.
        if self.stop_expired() {
            self.progress.required_finalization_failure.get_or_insert(mp4::DEADLINE);
        }
        self.unverified(pending.index)?;
        Err(lost.unwrap_or_else(|| unavailable("The segment verifier could not take a finalized segment")))
    }

    /// Rows of a segment that will not be verified: the part stays unpublished
    /// (§6.2 rows 2 and 19).
    fn unverified(&mut self, rows: Vec<slots::Row>) -> Result<(), ProbeError> {
        self.resolve_all(rows.into_iter().map(|mut row| {
            row.withdraw(slots::Missing::SegmentUnverified);
            row
        }))
    }

    /// Apply arrived verdicts, waiting up to `wait` for the first, then
    /// resolve overdue segments as unverified (§6.2 row 19). Returns the cause
    /// of a newly lost video activity.
    fn poll_verification(&mut self, wait: Duration) -> Result<Option<ProbeError>, ProbeError> {
        if self.publication.verifying.is_empty() {
            return Ok(None);
        }
        let mut lost = None;
        let mut received = if wait.is_zero() {
            self.verdicts.try_recv().map_err(|error| error == mpsc::TryRecvError::Disconnected)
        } else {
            self.verdicts.recv_timeout(wait).map_err(|error| error == mpsc::RecvTimeoutError::Disconnected)
        };
        loop {
            match received {
                Ok(verdict) => lost = lost.or(self.apply_verdict(verdict)?),
                Err(false) => break,
                Err(true) => {
                    // The verifier exited: no part in flight can be published.
                    for entry in std::mem::take(&mut self.publication.verifying) {
                        self.unverified(entry.index)?;
                    }
                    return Ok(lost.or_else(|| Some(unavailable("The segment verifier exited"))));
                }
            }
            received = self.verdicts.try_recv().map_err(|error| error == mpsc::TryRecvError::Disconnected);
        }
        let now = clock::qpc()?;
        let marks = self.worker.suspend_marks();
        for index in 0..self.publication.verifying.len() {
            let entry = &mut self.publication.verifying[index];
            // Sleep does not count; an unmeasurable interval is not evaluated.
            let overdue = matches!(
                marks.since(entry.handoff, now, self.clock.qpc_frequency),
                AwakeElapsed::Known(elapsed) if elapsed >= entry.allowance);
            let Some(rows) = overdue.then(|| entry.abandon()).flatten() else { continue };
            self.unverified(rows)?;
            lost = lost.or_else(|| Some(unavailable("The segment verification result is overdue")));
        }
        Ok(lost)
    }

    fn apply_verdict(&mut self, verdict: mp4::Verdict) -> Result<Option<ProbeError>, ProbeError> {
        let verifying = &mut self.publication.verifying;
        let Some(position) = verifying.iter().position(|entry| entry.summary.segment == verdict.segment) else { return Ok(None) };
        let Some(mut entry) = verifying.remove(position) else { return Ok(None) };
        if entry.abandoned() {
            // A late verdict of an overdue part is discarded.
            return Ok(None);
        }
        let last = self.last_row(&entry.index);
        let mut lost = None;
        match verdict.outcome {
            Ok(Some(published)) => {
                devlog!("video", "segment published m{} s{} frames={} bytes={}", self.id.0, verdict.segment.0,
                    entry.summary.frame_count, published.bytes);
                for (row, (pts, duration)) in entry.index.iter_mut().zip(published.timing) {
                    row.pts_100ns = Some(pts);
                    row.duration_100ns = Some(duration);
                }
                entry.summary.local_end_100ns = published.local_end_100ns;
                entry.summary.published_bytes = Some(published.bytes);
                self.summary.encoded += entry.summary.frame_count;
                self.summary.segments.push(entry.summary);
                self.resolve_all(entry.index)?;
            }
            Ok(None) => {
                devlog!("video", "segment revoked m{} s{}", self.id.0, verdict.segment.0);
                let before = entry.coverage.as_ref().and_then(|coverage| coverage.before())
                    .ok_or_else(|| unavailable("Revoked video media has no coverage cutoff"))?;
                withdraw_revoked(&mut entry.index, before);
                self.resolve_all(entry.index)?;
            }
            Err(error) => {
                devlog!("video", "segment verification failed m{} s{} stage={:?} kind={:?} error={:?}", self.id.0, verdict.segment.0,
                    error.stage, error.kind, error.to_string());
                if error.kind() == ProbeKind::PublicationDeadline {
                    self.progress.required_finalization_failure.get_or_insert(mp4::DEADLINE);
                }
                self.unverified(entry.index)?;
                lost = Some(error);
            }
        }
        self.frames.request_flush(last);
        self.worker.request_journal_flush();
        Ok(lost)
    }

    /// The merge key of a segment's last row, which its flush must include.
    fn last_row(&self, rows: &[slots::Row]) -> Option<RowKey> {
        rows.iter().map(|row| (row.receipt, self.id, row.slot)).max()
    }

    /// Stop: wait for every part in flight within the Stop deadline. A part
    /// still unverified at the deadline stays `.part` and is a required
    /// finalization failure (§6.2 row 2).
    fn settle_verification(&mut self) -> Result<(), ProbeError> {
        while self.publication.verifying.iter().any(|entry| !entry.abandoned()) {
            if self.stop_expired() {
                let rows: Vec<_> = self.publication.verifying.iter_mut().filter_map(Verifying::abandon).flatten().collect();
                if !rows.is_empty() {
                    self.progress.required_finalization_failure.get_or_insert(mp4::DEADLINE);
                    self.unverified(rows)?;
                }
            }
            if let Some(lost) = self.poll_verification(VERIFIER_POLL)? {
                self.loss.lose(self.summary, &lost);
            }
        }
        Ok(())
    }

    /// The frames receipt of a decided row (§6.5). A row that newly selects a
    /// frame carries that frame's capture receipt. Every other row carries
    /// the ordering receipt max(previous row, e_k), where e_k is the slot end
    /// limited by the cutoff: every later slot's frame has a source time, and
    /// so a capture receipt, at or after e_k, delivered to this worker or not.
    /// A captured receipt below the previous row's is filtered out by the caller.
    fn receipt(&mut self, end: SessionTimeUs, captured: Option<SessionTimeUs>) -> SessionTimeUs {
        let receipt = match captured {
            Some(captured) => captured,
            None => self.publication.last_receipt.map_or(end, |last| last.max(end)),
        };
        self.publication.last_receipt = Some(receipt);
        receipt
    }

    fn stage_slot(&mut self, row: slots::Row) -> Result<(), ProbeError> {
        if row.slot < self.publication.published_through || self.publication.staged.contains_key(&row.slot) {
            return Err(unavailable("The staged video slot prefix contained a duplicate"));
        }
        if self.publication.staged.len() as u64 >= self.publication.staging_bound {
            // The bound holds while the merge takes rows (§6.2 row 20): wait
            // at most one slot interval for lane space.
            let slot = self.timeline.slot(row.slot)?;
            let until = Instant::now() + Duration::from_micros(slot.end.0.saturating_sub(slot.start.0).unsigned_abs());
            loop {
                self.publish_resolved()?;
                if (self.publication.staged.len() as u64) < self.publication.staging_bound { break; }
                if Instant::now() >= until { return Err(unavailable(STAGING_OVERFLOW).with_kind(ProbeKind::StagingOverflow)); }
                thread::park_timeout(Duration::from_millis(1));
            }
        }
        self.publication.staged.insert(row.slot, row);
        self.publish_resolved()
    }

    fn resolve(&mut self, row: slots::Row) -> Result<(), ProbeError> {
        let prior = self.publication.staged.get_mut(&row.slot)
            .filter(|prior| !prior.encoded && prior.reason.is_none())
            .ok_or_else(|| unavailable("The H.264 output does not match an unresolved slot"))?;
        *prior = row;
        self.publish_resolved()
    }

    fn resolve_all(&mut self, rows: impl IntoIterator<Item = slots::Row>) -> Result<(), ProbeError> {
        rows.into_iter().try_for_each(|row| self.resolve(row))
    }

    /// Move the resolved prefix into the frames lane without waiting; a full
    /// lane leaves the rest staged. Then publish the mark.
    fn publish_resolved(&mut self) -> Result<(), ProbeError> {
        let publication = &mut *self.publication;
        let mut full = false;
        loop {
            if let Some(&(first, end)) = publication.paused_slots.front() && publication.published_through >= first {
                publication.published_through = publication.published_through.max(end);
                publication.paused_slots.pop_front();
                continue;
            }
            if !publication.staged.get(&publication.published_through).is_some_and(|row| row.encoded || row.reason.is_some()) {
                break;
            }
            let mut row = publication.staged.remove(&publication.published_through)
                .ok_or_else(|| unavailable("The resolved video slot is unavailable"))?;
            if !row.encoded && row.revoked() {
                row.withdraw(slots::Missing::ApprovalSuppressed);
            }
            // A closed epoch's flags were fixed at its pause (`freeze_late`).
            if row.repeated == Some(true) && row.admission_epoch == self.opening.epoch {
                row.late = Some(self.evidence.late(row.slot));
            }
            row.published_t_us = Some(self.clock.time(clock::qpc()?)?);
            let (epoch, encoded, reason, pts) = (row.admission_epoch, row.encoded, row.reason, row.pts_us);
            let (repeated, late) = (row.repeated == Some(true), row.late == Some(true));
            match self.lane.rows.try_send(row) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(row)) => {
                    publication.staged.insert(row.slot, row);
                    full = true;
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(unavailable(FRAMES_LANE_CLOSED).with_kind(ProbeKind::FramesLaneClosed)),
            }
            publication.published_through += 1;
            self.summary.slots += 1;
            let present = reason.is_none_or(|reason| !matches!(reason, slots::Missing::MonitorMissing | slots::Missing::ApprovalSuppressed));
            self.summary.note_interval(epoch, present, encoded, pts, self.grace);
            let counts = &mut self.summary.counters;
            if let Some(reason) = reason {
                *counts.missing_by_reason.entry(reason).or_default() += 1;
            } else if encoded {
                counts.indexed_encoded += 1;
                counts.repeated += u64::from(repeated);
                counts.changed_frame_arrived_late += u64::from(late);
            }
        }
        if publication.staged.is_empty() {
            publication.staging_bound = self.capacity.staged_slots;
        }
        // Rows are nondecreasing in slot order, so the first staged row bounds
        // every row not yet in the lane; with none staged, the next row
        // follows the previous one and this epoch's opening.
        let floor = publication.last_receipt.map_or(self.opening.time, |last| last.max(self.opening.time));
        let receipt = publication.staged.values().next().map_or(floor, |row| row.receipt);
        self.lane.mark.publish(self.opening.epoch, receipt, publication.published_through, full);
        Ok(())
    }

    /// Stop: every resolved row enters the lane before the Stop deadline.
    fn drain_lane(&mut self) -> Result<(), ProbeError> {
        loop {
            self.publish_resolved()?;
            if self.publication.staged.is_empty() || self.stop_expired() {
                return Ok(());
            }
            thread::park_timeout(VERIFIER_POLL);
        }
    }

    fn change_at_slot(&mut self, slot: slots::Slot) -> bool {
        let ready = self.change.as_ref().is_some_and(|change| slot.start >= change.at)
            && self.cutoff.is_none_or(|cutoff| slot.start < cutoff.time);
        self.change_ready |= ready;
        ready
    }

    fn complete_unavailable(&mut self, reason: slots::Missing) -> Result<(), ProbeError> {
        // Rows of parts in flight wait for their verdicts.
        let verifying: Vec<_> = self.publication.verifying.iter()
            .filter_map(|entry| Some(entry.index.first()?.slot..=entry.index.last()?.slot)).collect();
        for row in self.publication.staged.values_mut() {
            if !row.encoded && row.reason.is_none() && !verifying.iter().any(|range| range.contains(&row.slot)) {
                row.miss(if self.preserve_rotation_part { slots::Missing::SegmentUnpublished } else { reason });
            }
        }
        self.publish_resolved()?;
        for index in self.publication.published_through.max(self.timeline.first())..self.progress.next {
            if index >= self.publication.published_through && !self.publication.staged.contains_key(&index) {
                let slot = self.timeline.slot(index)?;
                self.publish_unavailable(slot, reason)?;
            }
        }
        loop {
            if let Some(lost) = self.poll_verification(Duration::ZERO)? {
                self.loss.lose(self.summary, &lost);
            }
            if self.change.is_none() { self.change = self.changes.try_recv().ok(); }
            let slot = self.timeline.slot(self.progress.next)?;
            match self.admission("Unavailable video did not receive its admission cutoff")? {
                EpochAdmission::Closed(boundary) => {
                    if self.progress.next >= self.timeline.closed_prefix(boundary.time)? {
                        return Ok(());
                    }
                }
                EpochAdmission::Open(now) => {
                    if self.change_at_slot(slot) { return Ok(()); }
                    if now < slot.end {
                        let _ = self.worker.wait_for_close(Duration::from_millis(5))?;
                        continue;
                    }
                }
            }
            self.publish_unavailable(slot, reason)?;
            self.progress.next += 1;
        }
    }

    fn publish_unavailable(&mut self, slot: slots::Slot, reason: slots::Missing) -> Result<(), ProbeError> {
        let decided = self.clock.time(clock::qpc()?)?;
        let end = self.cutoff.map_or(slot.end, |cutoff| slot.end.min(cutoff.time));
        let receipt = self.receipt(end, None);
        let mut row = slots::Row::pending(self.id, self.opening.epoch, slot, decided, receipt, None, None);
        row.miss(reason);
        self.stage_slot(row)
    }
}
