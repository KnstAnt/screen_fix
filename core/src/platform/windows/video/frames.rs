//! The frames journal merge owner (§6.5): per-monitor lanes merged in
//! (`recv_t_us`, monitor, slot) order.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError, atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering}, mpsc::{self, Receiver, SyncSender}},
    thread,
    time::{Duration, Instant},
};

use crate::{
    CaptureError,
    package::{diagnostics::FailureComponent, journal::VerifiedIngress, metadata::UndeliveredFrames},
    protocol::{AdmissionEpoch, MonitorId, QpcFrequency, SessionTimeUs},
    platform::windows::{
        ProbeError, clock,
        controller::{Admission, AwakeElapsed, AwakeMark, WorkerSession},
        encoder, io_error, slots, unavailable,
    },
    session::{AdmissionBoundary, fault::FaultReport},
};

use super::{IDR_ALLOWANCE_US, ROTATION_US, US_PER_SECOND, VERIFICATION_ALLOWANCE_US};

const MARK_STALLED: &str = "Advance the monitor frames journal mark";
const MERGE_POLL: Duration = Duration::from_millis(4);
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(100);

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

/// The state a mark's tag word encodes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkTag {
    /// No worker has published a mark yet.
    None,
    /// The worker's rows of this epoch below the mark's receipt are in its lane.
    Open(u64),
    /// Every row of this epoch is in the lane and the worker waits for a new opening.
    Closed(u64),
    /// The worker has exited: it produces no further rows.
    Exited,
}

impl MarkTag {
    const NONE: u64 = 0;
    const EXITED: u64 = u64::MAX;

    /// `epoch << 1`, with bit 0 set for a closed epoch.
    fn encode(self) -> u64 {
        match self {
            Self::None => Self::NONE,
            Self::Open(epoch) => epoch << 1,
            Self::Closed(epoch) => epoch << 1 | 1,
            Self::Exited => Self::EXITED,
        }
    }

    fn decode(tag: u64) -> Self {
        match tag {
            Self::NONE => Self::None,
            Self::EXITED => Self::Exited,
            _ if tag & 1 == 1 => Self::Closed(tag >> 1),
            _ => Self::Open(tag >> 1),
        }
    }
}

/// A video worker's progress mark (§6.5); the worker is its only writer.
/// Every row of the worker with a receipt below `receipt` is already in its
/// lane. The worker stores the other fields, then `receipt`, then `tag`.
#[derive(Default)]
pub(super) struct Mark {
    /// A `MarkTag`, encoded.
    tag: AtomicU64,
    receipt: AtomicI64,
    /// The first slot not yet in the lane.
    slot: AtomicU64,
    /// The last enqueue found the lane full.
    full: AtomicBool,
}

impl Mark {
    pub(super) fn publish(&self, epoch: AdmissionEpoch, receipt: SessionTimeUs, slot: u64, full: bool) {
        self.slot.store(slot, Ordering::Relaxed);
        self.full.store(full, Ordering::Relaxed);
        self.receipt.store(receipt.0, Ordering::Release);
        self.tag.store(MarkTag::Open(epoch.0).encode(), Ordering::Release);
    }

    pub(super) fn close(&self, epoch: AdmissionEpoch) {
        self.tag.store(MarkTag::Closed(epoch.0).encode(), Ordering::Release);
    }

    pub(super) fn exit(&self) {
        self.tag.store(MarkTag::Exited.encode(), Ordering::Release);
    }

    fn tag(&self) -> MarkTag {
        MarkTag::decode(self.tag.load(Ordering::Acquire))
    }
}

/// One monitor's bounded single-producer lane to the frames merge owner.
pub(super) struct Lane {
    monitor: MonitorId,
    pub(super) rows: SyncSender<slots::Row>,
    pub(super) mark: Arc<Mark>,
}

/// The merge order of a frames row: (`recv_t_us`, monitor, slot).
pub(super) type RowKey = (SessionTimeUs, MonitorId, u64);

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
pub(in crate::platform::windows) struct Frames {
    lanes: Vec<Lane>,
    signals: Arc<MergeSignals>,
    owner: Option<thread::JoinHandle<Result<Vec<UndeliveredFrames>, ProbeError>>>,
}

impl Frames {
    /// Start the merge owner and register its opening endpoint with the controller.
    pub(in crate::platform::windows) fn spawn(worker: &WorkerSession, ingress: VerifiedIngress) -> Result<Self, ProbeError> {
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

    pub(super) fn lane(&self, monitor: MonitorId) -> Result<&Lane, ProbeError> {
        self.lanes.iter().find(|lane| lane.monitor == monitor)
            .ok_or_else(|| unavailable("The selected monitor has no frames lane"))
    }

    /// Flush the written frames prefix once it includes `last`, the final
    /// row of a segment whose outcome is decided, or at once without one.
    pub(super) fn request_flush(&self, last: Option<RowKey>) {
        self.signals.flush.lock().unwrap_or_else(PoisonError::into_inner).push(last);
    }

    /// Called after every video worker has exited: the remaining lane rows
    /// are released and the frames ingress closes.
    pub(in crate::platform::windows) fn finish(mut self) -> Result<Vec<UndeliveredFrames>, ProbeError> {
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

/// Row-21 progress of one lane: the mark's tag and slot, and the awake mark
/// at which they were last seen to change.
#[derive(Clone, Copy)]
struct Watch {
    progress: (MarkTag, u64),
    since: AwakeMark,
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
        if self.ingress.request_flush(released.0)? {
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
        let tag = lane.mark.tag();
        let receipt = SessionTimeUs(lane.mark.receipt.load(Ordering::Acquire));
        // A closed epoch stops constraining until a newer opening: the
        // worker's next row then follows that opening's floor.
        let floor = |closed: Option<u64>| match self.opened {
            Some(opened) if closed.is_none_or(|epoch| opened.epoch.0 > epoch) => Constraint::Floor(opened.time),
            Some(_) => Constraint::Free,
            None => Constraint::Floor(SessionTimeUs(i64::MIN)),
        };
        match tag {
            _ if finishing || lane.abandoned => Constraint::Free,
            MarkTag::Exited => Constraint::Free,
            MarkTag::None => floor(None),
            MarkTag::Closed(epoch) => floor(Some(epoch)),
            MarkTag::Open(_) => Constraint::Mark(receipt),
        }
    }

    /// Release lane heads in key order while every monitor's constraint
    /// admits them. Returns false when the writer had no room.
    fn release(&mut self, constraints: &[Constraint]) -> Result<bool, ProbeError> {
        loop {
            let mut next: Option<(usize, RowKey)> = None;
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
            if !self.ingress.try_submit_verified(receipt, value)? {
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
    fn watch(&mut self, frequency: QpcFrequency) -> Result<(), ProbeError> {
        let now = clock::qpc()?;
        let marks = self.worker.suspend_marks();
        let recording = self.opened.filter(|opened| matches!(self.worker.admit(),
            Ok(Admission::Open(receipt)) if receipt.boundary == *opened));
        for index in 0..self.lanes.len() {
            let lane = &mut self.lanes[index];
            let tag = lane.mark.tag();
            let progress = (tag, lane.mark.slot.load(Ordering::Acquire));
            // A full lane that still holds rows waits for another monitor's mark.
            let held = lane.mark.full.load(Ordering::Acquire) && !lane.queue.is_empty();
            // An older epoch's mark that was never closed (rows awaited
            // verification at the pause) still constrains, so it is watched.
            let current = recording.is_some_and(|opened| matches!(tag, MarkTag::Open(epoch) if epoch <= opened.epoch.0))
                && !lane.abandoned && !held;
            let restart = Watch { progress, since: AwakeMark { qpc: now, suspends: marks.sequence } };
            let Some(watch) = lane.watch.filter(|watch| current && watch.progress == progress) else {
                lane.watch = current.then_some(restart);
                continue;
            };
            match marks.since(watch.since, now, frequency) {
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
