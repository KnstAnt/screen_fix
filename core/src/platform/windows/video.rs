//! Each selected monitor owns its encoder and locally verified MP4 files.

use std::{
    collections::{BTreeMap, VecDeque},
    ops::ControlFlow,
    sync::{Arc, Mutex, PoisonError, RwLock, atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering}, mpsc::{self, Receiver, SyncSender}},
    thread,
    time::{Duration, Instant},
};

use windows::Win32::Media::MediaFoundation::*;

use crate::{
    CaptureError,
    capture::frame::{FrameCoverage, FrameMetadata},
    config::SelectedMonitor,
    package::{
        ArtifactKind,
        diagnostics::{Failure, FailureComponent, ReportOutcome},
        journal::VerifiedIngress,
        metadata::{AttemptLayout, CaptureCounters, Geometry, MonitorCapture, MonitorVideo, RejectedEncoder, SegmentEncoding, SegmentSummary, UndeliveredFrames, VideoCapacity, VideoCounters},
        outcome::HealthKind,
    },
    protocol::{AdmissionEpoch, ClockOrigin, MonitorId, QpcTick, SegmentId, SessionTimeUs},
    session::{AdmissionBoundary, fault::FaultReport, windows::{Admission, AwakeElapsed, PausedVideo, WorkerSession}},
};

use super::{context::{ApprovalReadError, ELEVATED_FRAME_WAIT_US, FramePolicy}, Apartment, ProbeError, ProbeKind, RecordingReport, capture_device, clock, clock_error, display, encode, encoder, h264, io_error, monitor, mp4, package_journal_error, slots, stage, unavailable};

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

/// The row-21 bound T (§6.5) in microseconds: one segment duration, its
/// rotation allowance, the verification allowance and one slot interval at
/// the lowest rate any qualified format can have.
fn mark_stall_us(requested_fps: u32) -> u64 {
    let slot = US_PER_SECOND.div_ceil(u64::from(encoder::min_effective_fps(requested_fps)));
    ROTATION_US.unsigned_abs().saturating_add(IDR_ALLOWANCE_US.unsigned_abs())
        .saturating_add(VERIFICATION_ALLOWANCE_US).saturating_add(slot)
}

/// Rows of one monitor's frames lane: ceil(T × effective fps). Together with
/// the worker's `staged_slots` staging, a monitor's rows can wait T for
/// another monitor's mark without overflow (§6.5 capacity).
fn lane_rows(requested_fps: u32) -> usize {
    let rows = mark_stall_us(requested_fps).saturating_mul(u64::from(encoder::max_effective_fps(requested_fps)))
        .div_ceil(US_PER_SECOND);
    usize::try_from(rows).unwrap_or(usize::MAX)
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

/// The tag of a mark that no worker has published yet.
const MARK_NONE: u64 = 0;
/// The tag of a worker that has exited: it produces no further rows.
const MARK_EXITED: u64 = u64::MAX;
/// A frames lane was closed by its merge owner (§6.2 row 1).
const FRAMES_LANE_CLOSED: &str = "The frames merge owner stopped taking rows";
/// The worker's frames staging overflowed after a one-slot wait (§6.2 row 20).
const STAGING_OVERFLOW: &str = "The video frames staging exceeded its bound";
const MARK_STALLED: &str = "Advance the monitor frames journal mark";
const MERGE_POLL: Duration = Duration::from_millis(4);
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(100);

/// A video worker's progress mark (§6.5); the worker is its only writer.
/// Every row of the worker with a receipt below `receipt` is already in its
/// lane. The worker stores the other fields, then `receipt`, then `tag`.
#[derive(Default)]
struct Mark {
    /// `epoch << 1`, with bit 0 set once the epoch's rows are all in the lane
    /// and the worker waits for a new opening; `MARK_NONE` or `MARK_EXITED`.
    tag: AtomicU64,
    receipt: AtomicI64,
    /// The first slot not yet in the lane.
    slot: AtomicU64,
    /// The last enqueue found the lane full.
    full: AtomicBool,
}

impl Mark {
    fn publish(&self, epoch: AdmissionEpoch, receipt: SessionTimeUs, slot: u64, full: bool) {
        self.slot.store(slot, Ordering::Relaxed);
        self.full.store(full, Ordering::Relaxed);
        self.receipt.store(receipt.0, Ordering::Release);
        self.tag.store(epoch.0 << 1, Ordering::Release);
    }

    fn close(&self, epoch: AdmissionEpoch) {
        self.tag.store(epoch.0 << 1 | 1, Ordering::Release);
    }

    fn exit(&self) {
        self.tag.store(MARK_EXITED, Ordering::Release);
    }
}

/// One monitor's bounded single-producer lane to the frames merge owner.
struct Lane {
    monitor: MonitorId,
    rows: SyncSender<slots::Row>,
    mark: Arc<Mark>,
}

/// The merge order of a frames row: (`recv_t_us`, monitor, slot).
type RowKey = (SessionTimeUs, MonitorId, u64);

#[derive(Default)]
struct MergeSignals {
    /// Every video worker has exited.
    finish: AtomicBool,
    /// The last rows of segments whose outcome is final: once the merge has
    /// released a row, the written prefix through it is flushed. `None`, a
    /// segment without rows, asks for the prefix written so far.
    flush: Mutex<Vec<Option<RowKey>>>,
}

/// The frames journal owner (§6.5): per-monitor lanes merged in
/// (`recv_t_us`, monitor, slot) order by a thread that never waits for IO.
pub(super) struct Frames {
    lanes: Vec<Lane>,
    signals: Arc<MergeSignals>,
    owner: Option<thread::JoinHandle<Result<Vec<UndeliveredFrames>, ProbeError>>>,
}

impl Frames {
    /// Start the merge owner and register its opening endpoint with the controller.
    pub(super) fn spawn(worker: &WorkerSession, ingress: VerifiedIngress) -> Result<Self, ProbeError> {
        let requested_fps = worker.config().profile().capture_pace().frames_per_second();
        let capacity = lane_rows(requested_fps);
        let mut lanes = Vec::new();
        let mut merged = Vec::new();
        for selected in worker.config().monitors() {
            let (rows, taken) = mpsc::sync_channel(capacity);
            let mark = Arc::new(Mark::default());
            lanes.push(Lane { monitor: selected.id, rows, mark: Arc::clone(&mark) });
            merged.push(MergeLane { monitor: selected.id, rows: taken, mark, queue: VecDeque::new(), abandoned: false, watch: None });
        }
        let (opening, openings) = mpsc::sync_channel(1);
        let signals = Arc::new(MergeSignals::default());
        let merge = Merge {
            worker: worker.clone(), ingress, lanes: merged, openings, signals: Arc::clone(&signals),
            opened: None, released: None, stall: Duration::from_micros(mark_stall_us(requested_fps)),
            undelivered: Vec::new(),
        };
        let owner = thread::Builder::new().name("frames-merge".into()).spawn(move || merge.run())
            .map_err(|error| io_error("Start the frames merge owner", error))?;
        let frames = Self { lanes, signals, owner: Some(owner) };
        worker.register_frames(opening)?;
        Ok(frames)
    }

    fn lane(&self, monitor: MonitorId) -> Result<&Lane, ProbeError> {
        self.lanes.iter().find(|lane| lane.monitor == monitor)
            .ok_or_else(|| unavailable("The selected monitor has no frames lane"))
    }

    /// Flush the written frames prefix once it includes `last`, the final
    /// row of a segment whose outcome is decided, or at once without one.
    fn request_flush(&self, last: Option<RowKey>) {
        self.signals.flush.lock().unwrap_or_else(PoisonError::into_inner).push(last);
    }

    /// Called after every video worker has exited: the remaining lane rows
    /// are released and the frames ingress closes.
    pub(super) fn finish(mut self) -> Result<Vec<UndeliveredFrames>, ProbeError> {
        self.signals.finish.store(true, Ordering::Release);
        let owner = self.owner.take().ok_or_else(|| unavailable("The frames merge owner is unavailable"))?;
        owner.join().map_err(|_| unavailable("The frames merge owner terminated unexpectedly"))?
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            self.signals.finish.store(true, Ordering::Release);
            let _ = owner.join();
        }
    }
}

/// Row-21 progress of one lane: the mark's tag and slot, and the QPC and
/// suspend sequence at which they were last seen to change.
#[derive(Clone, Copy)]
struct Watch {
    progress: (u64, u64),
    since: (QpcTick, u64),
}

struct MergeLane {
    monitor: MonitorId,
    rows: Receiver<slots::Row>,
    mark: Arc<Mark>,
    /// Rows taken from the lane in slot order: the head, or after a row-21
    /// report the rows the lane held then.
    queue: VecDeque<slots::Row>,
    /// Row 21 was reported: later rows of this worker are discarded.
    abandoned: bool,
    watch: Option<Watch>,
}

impl MergeLane {
    fn head(&mut self) -> Option<&slots::Row> {
        if self.abandoned {
            while self.rows.try_recv().is_ok() {}
        } else if self.queue.is_empty() && let Ok(row) = self.rows.try_recv() {
            self.queue.push_back(row);
        }
        self.queue.front()
    }
}

/// What a monitor's future rows allow: a row ordered before the bound by
/// (`recv_t_us`, monitor) can be released.
#[derive(Clone, Copy)]
enum Constraint {
    /// The worker's published mark; its own lane rows are always below it.
    Mark(SessionTimeUs),
    /// The opening floor (R, X, 0) until the worker publishes a mark of the epoch.
    Floor(SessionTimeUs),
    Free,
}

impl Constraint {
    fn admits(self, owner: MonitorId, receipt: SessionTimeUs, monitor: MonitorId) -> bool {
        match self {
            Self::Mark(_) if monitor == owner => true,
            Self::Mark(bound) | Self::Floor(bound) => (receipt, monitor) < (bound, owner),
            Self::Free => true,
        }
    }
}

struct Merge {
    worker: WorkerSession,
    ingress: VerifiedIngress,
    lanes: Vec<MergeLane>,
    openings: Receiver<(AdmissionBoundary, SyncSender<()>)>,
    signals: Arc<MergeSignals>,
    /// The latest Opening(e, R).
    opened: Option<AdmissionBoundary>,
    /// The key of the last released row.
    released: Option<RowKey>,
    stall: Duration,
    undelivered: Vec<UndeliveredFrames>,
}

impl Merge {
    fn run(mut self) -> Result<Vec<UndeliveredFrames>, ProbeError> {
        let result = self.merge();
        if let Err(error) = &result {
            self.worker.report_fault(FaultReport::attempt(&error.to_error(FailureComponent::Journal)));
        }
        result.map(|()| self.undelivered)
    }

    fn merge(&mut self) -> Result<(), ProbeError> {
        let frequency = clock::frequency()?;
        let mut watched = Instant::now();
        loop {
            // Control precedes lane rows: an opening is acknowledged before
            // any worker can produce a row of its epoch.
            while let Ok(opening) = self.openings.try_recv() {
                self.open(opening);
            }
            let finishing = self.signals.finish.load(Ordering::Acquire);
            // Marks are read before lane heads, so every row below a mark is
            // visible in its lane.
            let constraints: Vec<_> = self.lanes.iter().map(|lane| self.constraint(lane, finishing)).collect();
            let written = self.release(&constraints)?;
            self.flush(false)?;
            if finishing && written && self.lanes.iter_mut().all(|lane| lane.head().is_none()) {
                // A request whose row was never released is not stranded.
                return self.flush(true);
            }
            if watched.elapsed() >= WATCHDOG_INTERVAL {
                watched = Instant::now();
                self.watch(frequency)?;
            }
            match self.openings.recv_timeout(MERGE_POLL) {
                Ok(opening) => self.open(opening),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => thread::park_timeout(MERGE_POLL),
            }
        }
    }

    /// Flush the written prefix once it includes a requested segment's last
    /// row. A request whose row is not yet released stays pending, and a
    /// flush the writer cannot take now is retried.
    /// `all` takes every request, as when the merge finishes.
    fn flush(&mut self, all: bool) -> Result<(), ProbeError> {
        let released = self.released;
        let lock = || self.signals.flush.lock().unwrap_or_else(PoisonError::into_inner);
        // `None`, a segment without rows, is ordered before every row.
        let due = |last: &Option<RowKey>| all || *last <= released;
        if !lock().iter().any(due) {
            return Ok(());
        }
        let Some(released) = self.released else {
            // Nothing is written: no prefix to flush.
            lock().retain(|last| !due(last));
            return Ok(());
        };
        if self.ingress.request_flush(released.0).map_err(package_journal_error)? {
            lock().retain(|last| !due(last));
        }
        Ok(())
    }

    fn open(&mut self, (opening, ack): (AdmissionBoundary, SyncSender<()>)) {
        self.opened = Some(opening);
        for lane in &mut self.lanes {
            lane.watch = None;
        }
        let _ = ack.try_send(());
    }

    fn constraint(&self, lane: &MergeLane, finishing: bool) -> Constraint {
        let tag = lane.mark.tag.load(Ordering::Acquire);
        let receipt = SessionTimeUs(lane.mark.receipt.load(Ordering::Acquire));
        // A closed epoch stops constraining until a newer opening: the
        // worker's next row then follows that opening's floor.
        let floor = |closed: Option<u64>| match self.opened {
            Some(opened) if closed.is_none_or(|epoch| opened.epoch.0 > epoch) => Constraint::Floor(opened.time),
            Some(_) => Constraint::Free,
            None => Constraint::Floor(SessionTimeUs(i64::MIN)),
        };
        match tag {
            _ if finishing || lane.abandoned || tag == MARK_EXITED => Constraint::Free,
            MARK_NONE => floor(None),
            _ if tag & 1 == 1 => floor(Some(tag >> 1)),
            _ => Constraint::Mark(receipt),
        }
    }

    /// Release lane heads in key order while every monitor's constraint
    /// admits them. Returns false when the writer had no room.
    fn release(&mut self, constraints: &[Constraint]) -> Result<bool, ProbeError> {
        loop {
            let mut next: Option<(usize, (SessionTimeUs, MonitorId, u64))> = None;
            for (index, lane) in self.lanes.iter_mut().enumerate() {
                let monitor = lane.monitor;
                if let Some(row) = lane.head() {
                    let key = (row.receipt, monitor, row.slot);
                    if next.is_none_or(|(_, best)| key < best) {
                        next = Some((index, key));
                    }
                }
            }
            let Some((index, key @ (receipt, monitor, _))) = next else { return Ok(true) };
            if !self.lanes.iter().zip(constraints).all(|(lane, bound)| bound.admits(lane.monitor, receipt, monitor)) {
                return Ok(true);
            }
            let row = self.lanes[index].queue.front().ok_or_else(|| unavailable("The frames lane head is unavailable"))?;
            let value = serde_json::to_value(row).map_err(|error| io_error("Encode the frame index row", error.into()))?;
            if !self.ingress.try_submit_verified(receipt, value).map_err(package_journal_error)? {
                return Ok(false);
            }
            self.lanes[index].queue.pop_front();
            self.released = Some(key);
        }
    }

    /// §6.2 row 21: a worker whose mark of the current epoch does not advance
    /// for T, sleep excluded, while the controller records and its rows do not
    /// wait in a full lane, is blocked in a native call. A worker after a pause
    /// boundary (Closing) has no current-epoch mark. Only this owner reports
    /// it, and it never touches the worker's staging.
    fn watch(&mut self, frequency: crate::protocol::QpcFrequency) -> Result<(), ProbeError> {
        let now = clock::qpc()?;
        let marks = self.worker.suspend_marks();
        let recording = self.opened.filter(|opened| matches!(self.worker.admit(),
            Ok(Admission::Open(receipt)) if receipt.boundary == *opened));
        for index in 0..self.lanes.len() {
            let lane = &mut self.lanes[index];
            let tag = lane.mark.tag.load(Ordering::Acquire);
            let progress = (tag, lane.mark.slot.load(Ordering::Acquire));
            // A full lane that still holds rows waits for another monitor's mark.
            let held = lane.mark.full.load(Ordering::Acquire) && !lane.queue.is_empty();
            // An older epoch's mark that was never closed (rows awaited
            // verification at the pause) still constrains, so it is watched.
            let current = recording.is_some_and(|opened| tag != MARK_NONE && tag & 1 == 0 && tag >> 1 <= opened.epoch.0)
                && !lane.abandoned && !held;
            let restart = Watch { progress, since: (now, marks.sequence) };
            let Some(watch) = lane.watch.filter(|watch| current && watch.progress == progress) else {
                lane.watch = current.then_some(restart);
                continue;
            };
            match marks.awake_elapsed(watch.since.0, now, watch.since.1, frequency) {
                AwakeElapsed::Known(elapsed) if elapsed >= self.stall => self.abandon(index),
                AwakeElapsed::Known(_) => {}
                // A suspend without its resume, or more than one suspend, is not measured.
                AwakeElapsed::Unknown => lane.watch = Some(restart),
            }
        }
        Ok(())
    }

    /// Close a blocked worker's lane at its last mark: the rows it holds are
    /// released in order, its later rows are discarded and the other
    /// monitors' rows no longer wait for it.
    fn abandon(&mut self, index: usize) {
        let lane = &mut self.lanes[index];
        while let Ok(row) = lane.rows.try_recv() {
            lane.queue.push_back(row);
        }
        lane.abandoned = true;
        let (first_slot, after_t_us) = match lane.queue.back() {
            Some(row) => (row.slot.saturating_add(1), row.receipt),
            None => (lane.mark.slot.load(Ordering::Acquire), SessionTimeUs(lane.mark.receipt.load(Ordering::Acquire))),
        };
        self.undelivered.push(UndeliveredFrames { monitor: lane.monitor, first_slot, after_t_us });
        let message = format!(
            "The video worker of monitor {} made no frames progress within {} s. Its frames rows from slot {} (receipt {} us) to Stop are not delivered.",
            lane.monitor.0, self.stall.as_secs(), first_slot, after_t_us.0,
        );
        self.worker.report_fault(FaultReport::attempt(&CaptureError::owned(FailureComponent::Video, MARK_STALLED, None, message)));
    }
}

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
    let _apartment = Apartment::initialize()?;
    let display = display::resolve(&monitor.key)?;
    let luid = stage(unsafe { display.adapter.GetDesc1() }, "Read the display adapter")?.AdapterLuid;
    let device = capture_device(&display.adapter)?;
    let geometry = geometry_for(&display, monitor)?;
    let source = [geometry.source_size[0] as u32, geometry.source_size[1] as u32];
    let requested_fps = worker.config().profile().capture_pace().frames_per_second();
    // The capture owner starts after selection, so the startup self-check
    // pictures are converted here, while this thread alone holds the device.
    let encoder = encoder::select(&display.adapter, luid, source, requested_fps,
        &mut |format, software| encoder::probe_input(&device, format, software))
        .map_err(|failure| {
            report_rejected(worker, monitor.id, &failure.rejected);
            failure.last
        })?;
    let format = encoder.format;
    let geometry = video_geometry(geometry, format);
    let mut summary = MonitorVideo {
        monitor: monitor.id, fps: format.fps, degraded: encoder.software(),
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
        send, device, adapter_luid: luid, source_bounds: display.bounds, format, software: encoder.software(), unavailable: Arc::clone(&video_unavailable),
        evidence: Arc::clone(&evidence), reconfigure, probes, pacing: Arc::clone(&pacing),
    };
    let (parts, segments) = mpsc::sync_channel(VERIFIER_QUEUE);
    let (verdicts, results) = mpsc::sync_channel(VERIFIER_QUEUE);
    let adapter = display.adapter.clone();
    thread::scope(|scope| {
        let capture = thread::Builder::new().name(format!("capture-monitor-{}", monitor.id.0))
            .spawn_scoped(scope, move || monitor::run(monitor, worker, frame_policy, topology, borderless, ready, video_input, png, still_rows))
            .map_err(|error| io_error("Start the monitor capture owner", error))?;
        // The verifier ends when the recording drops its part sender; the
        // scope joins it.
        thread::Builder::new().name(format!("verify-monitor-{}", monitor.id.0))
            .spawn_scoped(scope, move || mp4::verify_segments(adapter, segments, verdicts, || worker.segment_deadline()))
            .map_err(|error| io_error("Start the segment verifier", error))?;
        let recording = record(monitor, worker, layout, frames, lane, frame_policy, topology, receive, changes,
            &pacing, encoder, geometry, &mut summary, &video_unavailable, &evidence, reports.as_ref(), parts, results, probe);
        // No further row of this worker can follow (§6.5 closing).
        lane.mark.exit();
        if let Err(error) = &recording {
            worker.report_fault(FaultReport::attempt(&error.to_error(FailureComponent::Video)));
        }
        let capture = capture.join().map_err(|_| unavailable("The monitor capture owner terminated unexpectedly"))?;
        let mut required_finalization_failure = None;
        let outcome = recording.and_then(|closing| {
            required_finalization_failure = closing.required_finalization_failure;
            let monitor::Exit::Session(cutoff) = capture.outcome?;
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

fn video_geometry(mut geometry: Geometry, format: super::gpu::VideoFormat) -> Geometry {
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
    reports: Option<&'a SyncSender<RecordingReport>>,
    timeline: slots::Timeline,
    opening: AdmissionBoundary,
    cutoff: Option<AdmissionBoundary>,
    next: u64,
    selection: Selection<'a>,
    outstanding: VecDeque<RecordedSlot>,
    headers: h264::Headers,
    current: Option<Segment>,
    segment_number: u32,
    force_pending: bool,
    summary: &'a mut MonitorVideo,
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
    parts: &'a SyncSender<mp4::SegmentToVerify>,
    verdicts: &'a Receiver<mp4::Verdict>,
    /// Finalized segments in flight to the verifier, oldest first.
    verifying: VecDeque<Verifying>,
    unavailable_video: &'a AtomicBool,
    clock: ClockOrigin,
    preserve_rotation_part: bool,
    required_finalization_failure: Option<&'static str>,
    approval_generation: Option<u64>,
    approval_break: bool,
    elevated: bool,
    conversion_wait: Option<SessionTimeUs>,
    geometry: Geometry,
    format: Option<(super::gpu::VideoFormat, bool)>,
    /// Hardware candidates rejected when the active format was selected.
    rejected_encoders: Vec<RejectedEncoder>,
    /// Derived from the timeline's effective fps.
    capacity: VideoCapacity,
    /// The slot decision grace G, in microseconds.
    grace: i64,
    /// The suspend sequence last seen by the encoder loop.
    suspends: u64,
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

/// The slot decision grace G (D-10 A): one capture pacing interval plus the
/// conversion bound, capped at two slot intervals.
fn decision_grace(worker: &WorkerSession, fps: u32) -> i64 {
    let micros = |duration: Duration| i64::try_from(duration.as_micros()).unwrap_or(i64::MAX);
    let two_slots = 2 * 1_000_000 / i64::from(fps);
    micros(monitor::capture_interval(worker)).saturating_add(micros(monitor::CONVERSION_DEADLINE).min(two_slots))
}

/// This monitor's video can no longer be produced (L-required, §6.2 row 9).
fn lose_video(
    worker: &WorkerSession, id: MonitorId, summary: &mut MonitorVideo, unavailable_video: &AtomicBool,
    reports: Option<&SyncSender<RecordingReport>>, error: &ProbeError,
) {
    summary.unavailable = true;
    unavailable_video.store(true, Ordering::Release);
    let code = error.source.as_ref().map(|source| source.code().0);
    if let Some(failure) = worker.video_failed(id, Failure::new(FailureComponent::Video, error.stage, code))
        && reports.is_none_or(|reports| reports.try_send(RecordingReport::Video(failure)).is_err())
    {
        worker.set_video_report(id, ReportOutcome::Unavailable { native_code: None });
    }
}

/// Release an encoder. A failed release loses this monitor's video only
/// (§6.2 row 9).
fn release_encoder(
    encoder: encoder::Encoder, worker: &WorkerSession, id: MonitorId, summary: &mut MonitorVideo,
    unavailable_video: &AtomicBool, reports: Option<&SyncSender<RecordingReport>>,
) {
    if let Err(error) = encoder.finish() {
        lose_video(worker, id, summary, unavailable_video, reports, &error);
    }
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
/// The wait of an elevated segment for its input prefix closer, sleep excluded.
const INPUT_PREFIX_WAIT: Duration = Duration::from_secs(3);
const INPUT_PREFIX_POLL: Duration = Duration::from_millis(2);

/// One self-check picture converted by the capture owner (VID-12). An
/// admission change while waiting supersedes the format request.
fn capture_probe(
    probe: &SyncSender<encoder::Probe>, worker: &WorkerSession, opening: AdmissionBoundary,
    format: super::gpu::VideoFormat, software: bool,
) -> Result<encode::Input, ProbeError> {
    let open = || Ok::<_, ProbeError>(matches!(worker.admit()?, Admission::Open(receipt) if receipt.boundary == opening));
    let (reply, result) = mpsc::sync_channel(1);
    let mut request = encoder::Probe { format, software, reply };
    loop {
        match probe.try_send(request) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(returned)) if open()? => {
                request = returned;
                thread::sleep(VERIFIER_POLL);
            }
            Err(_) => return Err(encoder::interruption()),
        }
    }
    loop {
        match result.recv_timeout(VERIFIER_POLL) {
            Ok(input) => return input,
            Err(mpsc::RecvTimeoutError::Timeout) if open()? => {}
            Err(_) => return Err(encoder::interruption()),
        }
    }
}

fn record(
    monitor: &SelectedMonitor, worker: &WorkerSession, layout: &AttemptLayout, frames: &Frames, lane: &Lane, frame_policy: &FramePolicy,
    topology: &display::topology::Topology, receive: Receiver<Observation>, changes: Receiver<monitor::FormatRequest>,
    pacing: &RwLock<Option<(AdmissionEpoch, slots::Timeline)>>, encoder: encoder::Encoder, mut geometry: Geometry,
    summary: &mut MonitorVideo, unavailable_video: &AtomicBool, evidence: &slots::Evidence, reports: Option<&SyncSender<RecordingReport>>,
    parts: SyncSender<mp4::SegmentToVerify>, verdicts: Receiver<mp4::Verdict>, probe: SyncSender<encoder::Probe>,
) -> Result<Closing, ProbeError> {
    let Some(mut start) = worker.wait_for_start()? else {
        encoder.finish()?;
        return Ok(Closing { cutoff: None, required_finalization_failure: None });
    };
    let id = monitor.id;
    let mut fps = encoder.format.fps;
    let mut encoder = Some(encoder);
    let mut origin = start.boundary.time;
    let mut next = 0;
    let mut segment_number = 0;
    let mut required_finalization_failure = None;
    // Rows awaiting verification carry across format changes and pauses,
    // together with the bound they were staged under.
    let mut published_through = 0;
    let mut staged = BTreeMap::new();
    let mut staging_bound = 0;
    let mut last_receipt = None;
    let mut paused_slots = VecDeque::new();
    let mut verifying = VecDeque::new();
    loop {
        let capacity = capacity_for(fps);
        // The slot evidence is sized for the highest qualifiable frame rate.
        if capacity.staged_slots > evidence.slots() {
            return Err(unavailable("The video format exceeds the monitor's slot evidence bound"));
        }
        let bound = if staged.is_empty() { capacity.staged_slots } else { staging_bound.max(capacity.staged_slots) };
        if staged.is_empty() {
            published_through = next;
            paused_slots.clear();
        }
        let timeline = slots::Timeline::new(origin, fps)?.with_first(next);
        *pacing.write().unwrap_or_else(PoisonError::into_inner) = Some((start.boundary.epoch, timeline));
        let mut state = Video {
            id, worker, layout, frames, lane, frame_policy, topology, changes: &changes, change: None, change_ready: false,
            evidence, reports, timeline,
            opening: start.boundary, cutoff: None, next,
            selection: Selection {
                receive: &receive, future: None, candidate: None, previous: None, missing: false,
            },
            grace: decision_grace(worker, fps),
            suspends: worker.suspend_marks().sequence,
            outstanding: VecDeque::with_capacity(4), headers: h264::Headers::default(), current: None,
            segment_number, force_pending: false, summary: &mut *summary, geometry: geometry.clone(),
            published_through, staged: std::mem::take(&mut staged), staging_bound: bound, last_receipt,
            paused_slots: std::mem::take(&mut paused_slots), parts: &parts, verdicts: &verdicts,
            verifying: std::mem::take(&mut verifying), unavailable_video, clock: start.clock,
            preserve_rotation_part: false,
            required_finalization_failure: None,
            approval_generation: None, approval_break: false,
            elevated: worker.elevated_epoch(start.boundary.epoch), conversion_wait: None,
            format: encoder.as_ref().map(|encoder| (encoder.format, encoder.software())),
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
            state.release(active);
        }
        if encoder.is_none() && !state.change_ready {
            let reason = if state.summary.unavailable { slots::Missing::EncoderUnavailable } else { slots::Missing::MonitorMissing };
            state.complete_unavailable(reason)?;
        }
        next = state.next;
        segment_number = state.segment_number;
        required_finalization_failure = required_finalization_failure.or(state.required_finalization_failure);
        let cutoff = state.cutoff;
        let transition = if state.change_ready {
            state.change.take()
        } else {
            if let Some(change) = state.change.take() {
                let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
            }
            None
        };
        let feedback = transition.as_ref().map(|_| state.timeline.slot(next)).transpose()?;
        let mut carry = |state: &mut Video<'_>| {
            published_through = state.published_through;
            staged = std::mem::take(&mut state.staged);
            staging_bound = state.staging_bound;
            last_receipt = state.last_receipt;
            paused_slots = std::mem::take(&mut state.paused_slots);
            verifying = std::mem::take(&mut state.verifying);
        };
        if let Some((change, boundary)) = transition.zip(feedback) {
            carry(&mut state);
            drop(state);
            origin = boundary.start;
            let target = FormatOwner { monitor, worker, opening: start.boundary, unavailable_video, reports, probe: &probe };
            if let Some(next_geometry) = target.change_format(change, &mut encoder, summary)? {
                geometry = next_geometry;
                fps = summary.fps;
            }
            continue;
        }
        let Some(closed) = cutoff else { return Err(unavailable("The video owner did not receive its closing boundary")) };
        let boundary = PausedVideo { monitor: id, first_paused_slot: next, fps };
        // Every decided row of the closed epoch is in the lane, or the mark
        // keeps constraining at the first that is not (§6.5 closing).
        state.publish_resolved()?;
        if state.staged.is_empty() {
            lane.mark.close(state.opening.epoch);
        }
        // A pause reports its decided prefix; verification can finish after it.
        worker.finish_video_pause(closed, boundary);
        let reopened = if worker.pause_pending(closed) { worker.wait_for_reopen(closed)? } else { None };
        let Some(opened) = reopened else {
            state.settle_verification()?;
            state.drain_lane()?;
            required_finalization_failure = required_finalization_failure.or(state.required_finalization_failure);
            let complete = state.published_through == state.next && state.staged.is_empty();
            state.summary.close_interval();
            drop(state);
            if !complete {
                return Err(unavailable("The video slot journal does not cover the complete admitted prefix"));
            }
            if let Some(active) = encoder.take() {
                release_encoder(active, worker, id, summary, unavailable_video, reports);
            }
            return Ok(Closing { cutoff, required_finalization_failure });
        };
        state.freeze_late();
        carry(&mut state);
        drop(state);
        next = boundary.end_exclusive(closed.time, opened.boundary.time)
            .ok_or_else(|| unavailable("The paused video slot range overflowed"))?;
        paused_slots.push_back((boundary.first_paused_slot, next));
        start = opened;
        origin = start.boundary.time;
    }
}

/// The parties of a format change (VID-9): the video owner of one monitor
/// and the capture owner that requested it.
struct FormatOwner<'a> {
    monitor: &'a SelectedMonitor,
    worker: &'a WorkerSession,
    opening: AdmissionBoundary,
    unavailable_video: &'a AtomicBool,
    reports: Option<&'a SyncSender<RecordingReport>>,
    probe: &'a SyncSender<encoder::Probe>,
}

impl FormatOwner<'_> {
    /// Apply a format change at its boundary slot: release the old encoder,
    /// select one for the new output and reply to the capture owner. Returns
    /// the new video geometry once an encoder is active; `summary.fps` then
    /// holds its frame rate.
    fn change_format(
        &self, change: monitor::FormatRequest, encoder: &mut Option<encoder::Encoder>, summary: &mut MonitorVideo,
    ) -> Result<Option<Geometry>, ProbeError> {
        let (worker, id) = (self.worker, self.monitor.id);
        if !matches!(worker.admit()?, Admission::Open(receipt) if receipt.boundary == self.opening) {
            let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
            return Ok(None);
        }
        if let Some(old) = encoder.take() {
            release_encoder(old, worker, id, summary, self.unavailable_video, self.reports);
        }
        let (Some(output), Some(device)) = (change.output, change.device) else {
            let _ = change.reply.try_send(monitor::FormatReply::Missing);
            return Ok(None);
        };
        // A failed display read proves nothing: the capture owner retries
        // on its next topology tick (VID-9).
        let actual = display::resolve(&self.monitor.key).ok()
            .filter(|actual| display::topology::Output::from_native(actual).is_ok_and(|actual| actual == output));
        let Some((actual, luid)) = actual.and_then(|actual| {
            let luid = unsafe { actual.adapter.GetDesc1() }.ok()?.AdapterLuid;
            Some((actual, luid))
        }) else {
            let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
            return Ok(None);
        };
        if self.unavailable_video.load(Ordering::Acquire) {
            // A known encoder failure cannot be retried by topology.
            // The WGC owner can still rebuild its native-only resources.
            let _ = change.reply.try_send(monitor::FormatReply::Ready { device, encoding: None, output });
            return Ok(None);
        }
        let next_geometry = geometry_for(&actual, self.monitor)?;
        let source = [next_geometry.source_size[0] as u32, next_geometry.source_size[1] as u32];
        let requested_fps = worker.config().profile().capture_pace().frames_per_second();
        let selected = encoder::select(&actual.adapter, luid, source, requested_fps,
            &mut |format, software| capture_probe(self.probe, worker, self.opening, format, software));
        let activated = match selected {
            Ok(activated) => activated,
            Err(failure) if failure.last.kind() == ProbeKind::ProbeInterrupted => {
                let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
                return Ok(None);
            }
            Err(failure) => {
                // No encoder qualifies for the new format: this monitor's
                // video is lost (§6.2 row 9); capture continues.
                report_rejected(worker, id, &failure.rejected);
                lose_video(worker, id, summary, self.unavailable_video, self.reports, &failure.last);
                let _ = change.reply.try_send(monitor::FormatReply::Ready { device, encoding: None, output });
                return Ok(None);
            }
        };
        report_rejected(worker, id, &activated.rejected);
        let (format, software) = (activated.format, activated.software());
        summary.fps = format.fps;
        summary.degraded |= software;
        monitor::report_video_ready(worker, id, format, software);
        *encoder = Some(activated);
        let _ = change.reply.try_send(monitor::FormatReply::Ready { device, encoding: Some((format, software)), output });
        Ok(Some(video_geometry(next_geometry, format)))
    }
}

impl Video<'_> {
    /// Encode this epoch's slots until it closes or the format changes. A
    /// failure of the frames stream ends the attempt; any other loses this
    /// monitor's video.
    fn run_encoder(&mut self, active: &encoder::Encoder) -> Result<(), ProbeError> {
        let encoded = encode::record(&active.transform, active.device.as_ref(), self,
            Video::observe_admission, Video::next_frame, Video::finish_approval_prefix, Video::output);
        match encoded {
            Err(error) if fails_attempt(&error) => return Err(error),
            Err(error) => self.flag_failure(&error),
            Ok(_) if self.preserve_rotation_part => {
                self.flag_failure(&unavailable("The H.264 encoder drained without a verified rotation IDR"));
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
                if self.elevated {
                    segment.mux.discard()?;
                }
                if self.stop_expired() {
                    self.required_finalization_failure.get_or_insert(ROTATION_PART_UNPUBLISHED);
                }
            }
        } else if let Err(error) = self.finish_current() {
            if fails_attempt(&error) {
                return Err(error);
            }
            self.flag_failure(&error);
        }
        Ok(())
    }

    fn release(&mut self, encoder: encoder::Encoder) {
        release_encoder(encoder, self.worker, self.id, self.summary, self.unavailable_video, self.reports);
    }

    fn observe_admission(&mut self) -> Result<bool, ProbeError> {
        if let Some(lost) = self.poll_verification(Duration::ZERO)? {
            return Err(lost);
        }
        // A pause boundary can span sleep (D-5 A); the encoder watchdog restarts.
        let suspends = self.worker.suspend_marks().sequence;
        let slept = std::mem::replace(&mut self.suspends, suspends) != suspends;
        if self.change.is_none() {
            self.change = self.changes.try_recv().ok();
        }
        let at = match self.worker.admit()? {
            Admission::Open(receipt) if receipt.boundary == self.opening => {
                receipt.clock.time(receipt.qpc).map_err(clock_error)?
            }
            Admission::Closed(Some(boundary)) if boundary.epoch == self.opening.epoch => {
                self.cutoff = Some(boundary);
                if self.stop_expired() {
                    return Err(unavailable("The video segment drain deadline expired"));
                }
                boundary.time
            }
            _ => return Err(unavailable("Video admission changed without closing its slot prefix")),
        };
        if !self.approval_break && self.preserve_rotation_part && self.rotation_overdue(at) {
            return Err(unavailable(ROTATION_IDR_MISSING));
        }
        Ok(slept)
    }

    /// The open segment's span from its start to `at`.
    fn segment_age(&self, at: SessionTimeUs) -> Option<i64> {
        self.current.as_ref().map(|segment| at.0.saturating_sub(segment.start.0))
    }

    fn rotation_overdue(&self, at: SessionTimeUs) -> bool {
        self.segment_age(at).is_some_and(|age| age > ROTATION_US + IDR_ALLOWANCE_US)
    }

    fn next_frame(&mut self) -> Result<encode::Next, ProbeError> {
        let slot = self.timeline.slot(self.next)?;
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
        if fresh.as_ref().is_some_and(|frame| self.last_receipt.is_some_and(|last| frame.metadata.recv_t_us < last)) {
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
        self.next = self.next.checked_add(1).ok_or_else(|| unavailable("The video slot count overflowed"))?;
        self.classify(&mut row, decision, approval.is_some(), geometry.is_some(), decision_due, expiry, repeated);
        if row.reason.is_some() {
            self.stage_slot(row)?;
            return Ok(encode::Next::Wait);
        }
        self.submit_slot(row, slot, cutoff)
    }

    /// The decision point of `slot`, or the encoder step while it cannot be
    /// decided: an open epoch decides a slot once it ended, a closed one at
    /// its cutoff.
    fn admit_slot(&mut self, slot: slots::Slot) -> Result<ControlFlow<encode::Next, SlotDecision>, ProbeError> {
        let decision = match self.worker.admit()? {
            Admission::Open(receipt) if receipt.boundary == self.opening => {
                let now = receipt.clock.time(receipt.qpc).map_err(clock_error)?;
                if now < slot.end {
                    return Ok(ControlFlow::Break(encode::Next::Wait));
                }
                SlotDecision { slot, at: now, end: slot.end, cutoff: None }
            }
            Admission::Closed(Some(boundary)) if boundary.epoch == self.opening.epoch => {
                self.cutoff = Some(boundary);
                if slot.start >= boundary.time {
                    return Ok(ControlFlow::Break(encode::Next::End));
                }
                SlotDecision { slot, at: boundary.time, end: slot.end.min(boundary.time), cutoff: Some(boundary.time) }
            }
            _ => return Err(unavailable("Video admission changed without closing its slot prefix")),
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
        Ok(encode::Next::Frame { input, pts, duration, force_idr })
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
                self.segment_number = self.segment_number.checked_add(1)
                    .ok_or_else(|| unavailable("The MP4 segment index overflowed"))?;
                let id = SegmentId(self.segment_number);
                let path = self.layout.path(ArtifactKind::VideoSegment { monitor: self.id, segment: id });
                stage(unsafe { media.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &sequence) },
                    "Attach verified SPS and PPS to the new MP4 stream")?;
                let (format, software) = self.format.ok_or_else(|| unavailable("The segment encoding mode is unavailable"))?;
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
                self.current = Some(Segment {
                    id, start: pending.row.pts_us, offset: pts, encoding, mux: mp4::Recording::open(&media, &path)?,
                    index: Vec::new(), pending: None, coverage: pending.row.coverage.clone(),
                });
            } else if self.current.is_none() {
                return Err(unavailable("The first encoded frame is not an IDR"));
            }
        }
        self.current.as_mut().ok_or_else(|| unavailable("The MP4 segment is unavailable"))?
            .push(pending, sample, picture.idr)?;
        Ok(retired)
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
            let mut seen = self.worker.suspend_marks().sequence;
            while !self.worker.input_prefix_closed(closed) {
                // Time asleep does not count against the prefix wait (D-5 A).
                let suspends = self.worker.suspend_marks().sequence;
                if std::mem::replace(&mut seen, suspends) != suspends {
                    deadline = Instant::now() + INPUT_PREFIX_WAIT;
                }
                if Instant::now() >= self.worker.segment_deadline().map_or(deadline, |limit| limit.min(deadline)) {
                    if let Some(segment) = self.current.take() { segment.mux.discard()?; }
                    let stage = "The elevated video prefix did not close before publication";
                    self.required_finalization_failure.get_or_insert(stage);
                    return Err(unavailable(stage));
                }
                thread::park_timeout(INPUT_PREFIX_POLL);
            }
        }
        if let Some(segment) = self.current.take() {
            let closed = match segment.close(self.id, self.geometry.clone(), self.worker, self.clock) {
                Ok(closed) => closed,
                Err(error) => {
                    // Only the Stop deadline makes an unclosed segment a
                    // required finalization failure; any other close
                    // failure loses this monitor's video (§6.2 rows 9 and 2).
                    if error.kind() == ProbeKind::PublicationDeadline || self.stop_expired() {
                        self.required_finalization_failure.get_or_insert(error.stage);
                    }
                    return Err(error);
                }
            };
            match closed {
                Closed::Resolved(rows) => {
                    let last = self.last_row(&rows);
                    self.resolve_all(rows)?;
                    self.frames.request_flush(last);
                    self.worker.request_journal_flush();
                }
                Closed::Part { pending, part, suppressed } => {
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
        let rows = self.staged.values_mut().chain(self.verifying.iter_mut().flat_map(|entry| entry.index.iter_mut()));
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
        if self.verifying.len() >= VERIFIER_QUEUE {
            self.summary.counters.verifier_waits += 1;
        }
        while self.verifying.len() >= VERIFIER_QUEUE && !self.verifying.iter().any(|entry| entry.abandoned) && !self.stop_expired() {
            lost = lost.or(self.poll_verification(VERIFIER_POLL)?);
        }
        if self.verifying.len() < VERIFIER_QUEUE && !self.stop_expired() && self.parts.try_send(part).is_ok() {
            self.verifying.push_back(pending);
            return lost.map_or(Ok(()), Err);
        }
        // An overdue part still holds the verifier, the Stop deadline expired,
        // or the verifier exited: this part stays `.part`.
        if self.stop_expired() {
            self.required_finalization_failure.get_or_insert(mp4::DEADLINE);
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
        if self.verifying.is_empty() {
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
                    for entry in std::mem::take(&mut self.verifying) {
                        self.unverified(entry.index)?;
                    }
                    return Ok(lost.or_else(|| Some(unavailable("The segment verifier exited"))));
                }
            }
            received = self.verdicts.try_recv().map_err(|error| error == mpsc::TryRecvError::Disconnected);
        }
        let now = clock::qpc()?;
        let marks = self.worker.suspend_marks();
        for index in 0..self.verifying.len() {
            let entry = &mut self.verifying[index];
            // Sleep does not count; an unmeasurable interval is not evaluated.
            let overdue = matches!(
                marks.awake_elapsed(entry.handoff.0, now, entry.handoff.1, self.clock.qpc_frequency),
                AwakeElapsed::Known(elapsed) if elapsed >= entry.allowance);
            let Some(rows) = overdue.then(|| entry.abandon()).flatten() else { continue };
            self.unverified(rows)?;
            lost = lost.or_else(|| Some(unavailable("The segment verification result is overdue")));
        }
        Ok(lost)
    }

    fn apply_verdict(&mut self, verdict: mp4::Verdict) -> Result<Option<ProbeError>, ProbeError> {
        let Some(position) = self.verifying.iter().position(|entry| entry.summary.segment == verdict.segment) else { return Ok(None) };
        let Some(mut entry) = self.verifying.remove(position) else { return Ok(None) };
        if entry.abandoned {
            // A late verdict of an overdue part is discarded.
            return Ok(None);
        }
        let last = self.last_row(&entry.index);
        let mut lost = None;
        match verdict.outcome {
            Ok(Some(published)) => {
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
                let before = entry.coverage.as_ref().and_then(|coverage| coverage.before())
                    .ok_or_else(|| unavailable("Revoked video media has no coverage cutoff"))?;
                withdraw_revoked(&mut entry.index, before);
                self.resolve_all(entry.index)?;
            }
            Err(error) => {
                if error.kind() == ProbeKind::PublicationDeadline {
                    self.required_finalization_failure.get_or_insert(mp4::DEADLINE);
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
        while self.verifying.iter().any(|entry| !entry.abandoned) {
            if self.stop_expired() {
                let rows: Vec<_> = self.verifying.iter_mut().filter_map(Verifying::abandon).flatten().collect();
                if !rows.is_empty() {
                    self.required_finalization_failure.get_or_insert(mp4::DEADLINE);
                    self.unverified(rows)?;
                }
            }
            if let Some(lost) = self.poll_verification(VERIFIER_POLL)? {
                self.flag_failure(&lost);
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
            None => self.last_receipt.map_or(end, |last| last.max(end)),
        };
        self.last_receipt = Some(receipt);
        receipt
    }

    fn stage_slot(&mut self, row: slots::Row) -> Result<(), ProbeError> {
        if row.slot < self.published_through || self.staged.contains_key(&row.slot) {
            return Err(unavailable("The staged video slot prefix contained a duplicate"));
        }
        if self.staged.len() as u64 >= self.staging_bound {
            // The bound holds while the merge takes rows (§6.2 row 20): wait
            // at most one slot interval for lane space.
            let slot = self.timeline.slot(row.slot)?;
            let until = Instant::now() + Duration::from_micros(slot.end.0.saturating_sub(slot.start.0).unsigned_abs());
            loop {
                self.publish_resolved()?;
                if (self.staged.len() as u64) < self.staging_bound { break; }
                if Instant::now() >= until { return Err(unavailable(STAGING_OVERFLOW).with_kind(ProbeKind::StagingOverflow)); }
                thread::park_timeout(Duration::from_millis(1));
            }
        }
        self.staged.insert(row.slot, row);
        self.publish_resolved()
    }

    fn resolve(&mut self, row: slots::Row) -> Result<(), ProbeError> {
        let prior = self.staged.get_mut(&row.slot)
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
        let mut full = false;
        loop {
            if let Some(&(first, end)) = self.paused_slots.front() && self.published_through >= first {
                self.published_through = self.published_through.max(end);
                self.paused_slots.pop_front();
                continue;
            }
            if !self.staged.get(&self.published_through).is_some_and(|row| row.encoded || row.reason.is_some()) {
                break;
            }
            let mut row = self.staged.remove(&self.published_through)
                .ok_or_else(|| unavailable("The resolved video slot is unavailable"))?;
            if !row.encoded && row.revoked() {
                row.withdraw(slots::Missing::ApprovalSuppressed);
            }
            // A closed epoch's flags were fixed at its pause (`freeze_late`).
            if row.repeated == Some(true) && row.admission_epoch == self.opening.epoch {
                row.late = Some(self.evidence.late(row.slot));
            }
            row.published_t_us = Some(self.clock.time(clock::qpc()?).map_err(clock_error)?);
            let (epoch, encoded, reason, pts) = (row.admission_epoch, row.encoded, row.reason, row.pts_us);
            let (repeated, late) = (row.repeated == Some(true), row.late == Some(true));
            match self.lane.rows.try_send(row) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(row)) => {
                    self.staged.insert(row.slot, row);
                    full = true;
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(unavailable(FRAMES_LANE_CLOSED).with_kind(ProbeKind::FramesLaneClosed)),
            }
            self.published_through += 1;
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
        if self.staged.is_empty() {
            self.staging_bound = self.capacity.staged_slots;
        }
        // Rows are nondecreasing in slot order, so the first staged row bounds
        // every row not yet in the lane; with none staged, the next row
        // follows the previous one and this epoch's opening.
        let floor = self.last_receipt.map_or(self.opening.time, |last| last.max(self.opening.time));
        let receipt = self.staged.values().next().map_or(floor, |row| row.receipt);
        self.lane.mark.publish(self.opening.epoch, receipt, self.published_through, full);
        Ok(())
    }

    /// Stop: every resolved row enters the lane before the Stop deadline.
    fn drain_lane(&mut self) -> Result<(), ProbeError> {
        loop {
            self.publish_resolved()?;
            if self.staged.is_empty() || self.stop_expired() {
                return Ok(());
            }
            thread::park_timeout(VERIFIER_POLL);
        }
    }

    fn flag_failure(&mut self, error: &ProbeError) {
        lose_video(self.worker, self.id, self.summary, self.unavailable_video, self.reports, error);
    }

    fn change_at_slot(&mut self, slot: slots::Slot) -> bool {
        let ready = self.change.as_ref().is_some_and(|change| slot.start >= change.at)
            && self.cutoff.is_none_or(|cutoff| slot.start < cutoff.time);
        self.change_ready |= ready;
        ready
    }

    fn complete_unavailable(&mut self, reason: slots::Missing) -> Result<(), ProbeError> {
        // Rows of parts in flight wait for their verdicts.
        let verifying: Vec<_> = self.verifying.iter()
            .filter_map(|entry| Some(entry.index.first()?.slot..=entry.index.last()?.slot)).collect();
        for row in self.staged.values_mut() {
            if !row.encoded && row.reason.is_none() && !verifying.iter().any(|range| range.contains(&row.slot)) {
                row.miss(if self.preserve_rotation_part { slots::Missing::SegmentUnpublished } else { reason });
            }
        }
        self.publish_resolved()?;
        for index in self.published_through.max(self.timeline.first())..self.next {
            if index >= self.published_through && !self.staged.contains_key(&index) {
                let slot = self.timeline.slot(index)?;
                self.publish_unavailable(slot, reason)?;
            }
        }
        loop {
            if let Some(lost) = self.poll_verification(Duration::ZERO)? {
                self.flag_failure(&lost);
            }
            if self.change.is_none() { self.change = self.changes.try_recv().ok(); }
            let slot = self.timeline.slot(self.next)?;
            match self.worker.admit()? {
                Admission::Closed(Some(boundary)) if boundary.epoch == self.opening.epoch => {
                    self.cutoff = Some(boundary);
                    if self.next >= self.timeline.closed_prefix(boundary.time)? {
                        return Ok(());
                    }
                }
                Admission::Open(receipt) if receipt.boundary == self.opening => {
                    if self.change_at_slot(slot) { return Ok(()); }
                    if receipt.clock.time(receipt.qpc).map_err(clock_error)? < slot.end {
                        let _ = self.worker.wait_for_close(Duration::from_millis(5))?;
                        continue;
                    }
                }
                _ => return Err(unavailable("Unavailable video did not receive its admission cutoff")),
            }
            self.publish_unavailable(slot, reason)?;
            self.next += 1;
        }
    }

    fn publish_unavailable(&mut self, slot: slots::Slot, reason: slots::Missing) -> Result<(), ProbeError> {
        let decided = self.clock.time(clock::qpc()?).map_err(clock_error)?;
        let end = self.cutoff.map_or(slot.end, |cutoff| slot.end.min(cutoff.time));
        let receipt = self.receipt(end, None);
        let mut row = slots::Row::pending(self.id, self.opening.epoch, slot, decided, receipt, None, None);
        row.miss(reason);
        self.stage_slot(row)
    }

}

/// A segment handed to the verifier. Its rows stay staged until the verdict.
struct Verifying {
    summary: SegmentSummary,
    index: Vec<slots::Row>,
    coverage: Option<Arc<FrameCoverage>>,
    claim: Arc<std::sync::atomic::AtomicU8>,
    /// The QPC at which the part was closed, and the suspend sequence read after it.
    handoff: (QpcTick, u64),
    /// The verification allowance left at that QPC, sleep excluded.
    allowance: Duration,
    /// Overdue: its rows are resolved and a late verdict is discarded.
    abandoned: bool,
}

impl Verifying {
    /// Abandon the part and take its rows, unless already abandoned or its
    /// publication began (its verdict then follows).
    fn abandon(&mut self) -> Option<Vec<slots::Row>> {
        if self.abandoned || self.claim.compare_exchange(mp4::CLAIM_OPEN, mp4::CLAIM_ABANDONED, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        self.abandoned = true;
        Some(std::mem::take(&mut self.index))
    }
}

enum Closed {
    /// No part remains: an empty or revoked segment with its resolved rows.
    Resolved(Vec<slots::Row>),
    /// A finalized part for the verifier, and a final row withdrawn now.
    Part { pending: Verifying, part: mp4::SegmentToVerify, suppressed: Option<slots::Row> },
}

struct Segment {
    id: SegmentId,
    start: SessionTimeUs,
    offset: i64,
    encoding: SegmentEncoding,
    mux: mp4::Recording,
    index: Vec<slots::Row>,
    pending: Option<(RecordedSlot, IMFSample, bool)>,
    coverage: Option<Arc<FrameCoverage>>,
}

impl Segment {
    fn push(&mut self, slot: RecordedSlot, sample: &IMFSample, idr: bool) -> Result<(), ProbeError> {
        if self.coverage.as_ref().map(|coverage| coverage.visit) != slot.row.coverage.as_ref().map(|coverage| coverage.visit) {
            return Err(unavailable("An encoded slot changed the segment's elevated visit"));
        }
        if let Some((prior, picture, prior_idr)) = self.pending.take() {
            let duration = slot.pts.checked_sub(prior.pts)
                .filter(|&duration| duration > 0).ok_or_else(|| unavailable("The MP4 timestamps are not increasing"))?;
            self.write(prior, picture, duration, prior_idr)?;
        }
        self.pending = Some((slot, sample.clone(), idr));
        Ok(())
    }

    fn write(&mut self, mut slot: RecordedSlot, sample: IMFSample, duration: i64, idr: bool) -> Result<(), ProbeError> {
        let pts = slot.pts.checked_sub(self.offset)
            .filter(|&pts| pts >= 0).ok_or_else(|| unavailable("The segment-relative PTS is invalid"))?;
        stage(unsafe { sample.SetSampleTime(pts) }, "Set the segment-local PTS")?;
        stage(unsafe { sample.SetSampleDuration(duration) }, "Set the segment-local duration")?;
        stage(unsafe {
            sample.SetUINT64(&MFSampleExtension_DecodeTimestamp, pts as u64)
        }, "Set the segment-local DTS")?;
        stage(unsafe {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, u32::from(idr))
        }, "Set verified H.264 sample attributes")?;
        if self.index.len() as u64 >= self.encoding.capacity.segment_pictures {
            return Err(unavailable("The open MP4 segment exceeded its frame index bound"));
        }
        self.mux.write(&sample)?;
        slot.row.verified(self.id, self.index.len() as u64, pts, duration);
        self.index.push(slot.row);
        Ok(())
    }

    /// Write the held picture and finalize, release and flush the part on
    /// the video worker. Verification and publication follow on the verifier.
    fn close(
        mut self, monitor: MonitorId, geometry: Geometry, worker: &WorkerSession, origin: ClockOrigin,
    ) -> Result<Closed, ProbeError> {
        let coverage = self.coverage.take();
        let cutoff = || coverage.as_ref().and_then(|coverage| coverage.before());
        let mut suppressed = None;
        if let Some((slot, picture, idr)) = self.pending.take() {
            if cutoff().is_some_and(|before| !within_media_prefix(&slot.row, slot.duration, before)) {
                let mut row = slot.row;
                row.withdraw(slots::Missing::ApprovalSuppressed);
                suppressed = Some(row);
            } else {
                let duration = slot.duration;
                self.write(slot, picture, duration, idr)?;
            }
        }
        if self.index.is_empty() {
            self.mux.discard()?;
            return Ok(Closed::Resolved(suppressed.into_iter().collect()));
        }
        // A muxed sample can display through a later slot. The on-disk part
        // is indivisible once its sink has accepted an unprovable sample.
        let media_before = self.index.iter().map(|row| {
            let end = row.pts_us.0.saturating_add(ceil_us(row.duration_100ns.unwrap_or(0)));
            SessionTimeUs(end.max(row.source_t_us.map_or(i64::MIN, |source| source.0.saturating_add(1))))
        }).max().expect("the segment has encoded rows");
        let start = self.start;
        let authorization = coverage.clone();
        let authorized = move |decoded_end: Option<i64>| {
            let decoded_before = decoded_end.map_or(media_before, |end| {
                SessionTimeUs(start.0.saturating_add(ceil_us(end)))
            });
            authorization.as_ref().and_then(|coverage| coverage.before())
                .is_none_or(|before| media_before.max(decoded_before) <= before)
        };
        let expected = self.index.iter().map(|row| row.pts_100ns.zip(row.duration_100ns)
            .ok_or_else(|| unavailable("The segment frame has no planned timing"))).collect::<Result<Vec<_>, _>>()?;
        let Some(part) = self.mux.close(self.id, expected, geometry.video_size, || worker.segment_deadline(), Box::new(authorized))? else {
            let before = cutoff().expect("revoked media has a cutoff");
            withdraw_revoked(&mut self.index, before);
            self.index.extend(suppressed);
            return Ok(Closed::Resolved(self.index));
        };
        // The allowance runs from the end of the segment's media and includes
        // any wait for a verifier slot.
        let closed_qpc = clock::qpc()?;
        let suspends = worker.suspend_marks().sequence;
        let lag = origin.time(closed_qpc).map_err(clock_error)?.0.saturating_sub(media_before.0);
        let allowance = Duration::from_micros(VERIFICATION_ALLOWANCE_US.saturating_sub(u64::try_from(lag).unwrap_or(0)));
        let pending = Verifying {
            summary: SegmentSummary {
                monitor, segment: self.id, common_start_us: self.start, frame_count: self.index.len() as u64,
                local_end_100ns: 0, published_bytes: None, geometry, encoding: Some(self.encoding),
            },
            index: self.index, coverage, claim: Arc::clone(&part.claim), handoff: (closed_qpc, suspends), allowance, abandoned: false,
        };
        Ok(Closed::Part { pending, part, suppressed })
    }

}

fn withdraw_revoked(rows: &mut [slots::Row], before: SessionTimeUs) {
    for row in rows {
        let reason = if within_media_prefix(row, row.duration_100ns.unwrap_or(0), before) {
            slots::Missing::SegmentUnpublished
        } else {
            slots::Missing::ApprovalSuppressed
        };
        row.withdraw(reason);
    }
}

fn within_media_prefix(row: &slots::Row, duration_100ns: i64, before: SessionTimeUs) -> bool {
    row.pts_us.0.saturating_add(ceil_us(duration_100ns)) <= before.0
        && row.source_t_us.is_some_and(|source| source < before)
}

/// 100 ns units rounded up to microseconds.
fn ceil_us(hundred_ns: i64) -> i64 {
    hundred_ns.saturating_add(9).div_euclid(10)
}
