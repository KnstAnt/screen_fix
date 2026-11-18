//! Receipt-ordered staging for the attempt's still journal. Producers hand off
//! reservations and progress on the same local timeline; job results fill slots.

use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::{
    capture::frame::FrameCoverage,
    package::{
        diagnostics::{Failure, FailureComponent},
        journal::{HealthRecord, JournalIngress, Submit},
        outcome::{HealthKind, LossEvidence, LossReason},
    },
    protocol::{QpcFrequency, QpcTick, SessionTimeUs},
    platform::windows::controller::{AwakeElapsed, SuspendMarks},
};

use super::{ProbeError, stills::StillRow, unavailable};

const SLOTS: usize = 1024;
const WAIT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Budget {
    slots: AtomicUsize,
    /// At most one pending omission per loss reason.
    omitted: Mutex<BTreeMap<LossReason, Omitted>>,
}

#[derive(Clone, Copy)]
struct Omitted {
    count: std::num::NonZeroU64,
    first: SessionTimeUs,
    last: SessionTimeUs,
}

impl Budget {
    fn omit(&self, at: SessionTimeUs, reason: LossReason) {
        let mut pending = self.omitted.lock().unwrap_or_else(PoisonError::into_inner);
        pending.entry(reason).and_modify(|omitted| {
            omitted.count = omitted.count.saturating_add(1);
            omitted.first = omitted.first.min(at);
            omitted.last = omitted.last.max(at);
        }).or_insert(Omitted { count: std::num::NonZeroU64::MIN, first: at, last: at });
    }

    /// Omissions that every earlier still row has already passed, in ascending last receipt.
    fn take_gaps(&self, before: SessionTimeUs, first_row: SessionTimeUs) -> Vec<(LossReason, Omitted)> {
        let mut pending = self.omitted.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ready: Vec<_> = pending.iter().filter(|(_, gap)| gap.last < before && gap.last <= first_row)
            .map(|(reason, gap)| (*reason, *gap)).collect();
        for (reason, _) in &ready {
            pending.remove(reason);
        }
        ready.sort_by_key(|(_, gap)| (gap.last, gap.first));
        ready
    }
}

struct Charge(Arc<Budget>);

impl Drop for Charge {
    fn drop(&mut self) {
        self.0.slots.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct SharedSlot {
    fallback: Option<StillRow>,
    expired: bool,
}

#[derive(Clone)]
pub(super) struct StillCoverage {
    pub visit: Arc<FrameCoverage>,
    pub last: SessionTimeUs,
}

impl StillCoverage {
    pub fn allows(&self) -> bool { self.visit.before().is_none_or(|before| self.last < before) }
}

struct Completed {
    row: StillRow,
    coverage: Option<StillCoverage>,
}

impl Completed {
    /// The row, withdrawn when its still is past its elevated visit's certified bound.
    fn publishable(mut self) -> StillRow {
        if self.coverage.as_ref().is_some_and(|coverage| !coverage.allows()) {
            self.row.withdraw_uncertified();
        }
        self.row
    }
}

/// A handle that finishes one half of its slot: the only half, or the
/// before (0) or seen (1) half of a paired row.
pub(super) struct Reservation {
    result: SyncSender<(u8, Option<Completed>)>,
    shared: Arc<Mutex<SharedSlot>>,
    half: u8,
}

impl Reservation {
    /// Transition a private slot after the causal input has a durable EventRef.
    /// The producer owns the terminal row even if a monitor has stopped.
    pub fn approve(&self, fallback: StillRow) -> bool {
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.expired { return false; }
        shared.fallback = Some(fallback);
        true
    }

    pub fn suppress(self) {
        let _ = self.result.try_send((self.half, None));
    }

    pub fn finish(self, row: StillRow) {
        self.finish_frame(row, None);
    }

    pub fn finish_frame(self, row: StillRow, coverage: Option<StillCoverage>) {
        let _ = self.result.try_send((self.half, Some(Completed { row, coverage })));
    }
}

struct Slot {
    at: SessionTimeUs,
    deadline: Instant,
    result: Receiver<(u8, Option<Completed>)>,
    shared: Arc<Mutex<SharedSlot>>,
    /// The halves still owed, and the ones received by index.
    owed: u8,
    halves: [Option<Completed>; 2],
    _charge: Charge,
}

impl Slot {
    /// The slot's row once every half is in; none when it was suppressed.
    fn row(self) -> Option<StillRow> {
        let [row, seen] = self.halves;
        let row = row?.publishable();
        Some(match seen {
            Some(seen) => row.join_seen(seen.publishable()),
            None => row,
        })
    }
}

#[derive(Default)]
struct Progress {
    before: AtomicI64,
    closed: AtomicBool,
    excluded: AtomicBool,
}

struct Producer {
    progress: Arc<Progress>,
    send: SyncSender<Slot>,
    budget: Arc<Budget>,
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.progress.closed.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(super) struct Rows(Arc<Producer>);

impl Rows {
    /// A failed reservation reports why, so a public caller can account it with `note_loss`.
    pub fn reserve(&self, at: SessionTimeUs, fallback: Option<StillRow>) -> Result<Reservation, LossReason> {
        self.reserve_halves(at, fallback, 1)
    }

    /// Reserve a public row; a failure is accounted as a loss.
    pub fn reserve_public(&self, at: SessionTimeUs, fallback: StillRow) -> Option<Reservation> {
        self.reserve(at, Some(fallback)).inspect_err(|reason| self.note_loss(at, *reason)).ok()
    }

    /// One row of a pre-motion or pre-action trigger, finished by its before
    /// and seen halves (IMG-2). Its fallback is set at approval.
    pub fn reserve_pair(&self, at: SessionTimeUs) -> Result<[Reservation; 2], LossReason> {
        let before = self.reserve_halves(at, None, 2)?;
        let seen = Reservation { result: before.result.clone(), shared: Arc::clone(&before.shared), half: 1 };
        Ok([before, seen])
    }

    fn reserve_halves(&self, at: SessionTimeUs, fallback: Option<StillRow>, halves: u8) -> Result<Reservation, LossReason> {
        let producer = &self.0;
        if producer.progress.excluded.load(Ordering::Acquire) {
            return Err(LossReason::ProducerStall);
        }
        if at.0 < producer.progress.before.load(Ordering::Relaxed) {
            return Err(LossReason::LateRow);
        }
        producer.budget.slots.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < SLOTS).then_some(count + 1)
        }).map_err(|_| LossReason::StagingPressure)?;
        let charge = Charge(Arc::clone(&producer.budget));
        let (finish, result) = sync_channel(usize::from(halves));
        let shared = Arc::new(Mutex::new(SharedSlot { fallback, expired: false }));
        producer.send.try_send(Slot { at, deadline: Instant::now() + WAIT, result,
            shared: Arc::clone(&shared), owed: halves, halves: [None, None], _charge: charge })
            .map_err(|_| LossReason::StagingPressure)?;
        Ok(Reservation { result: finish, shared, half: 0 })
    }

    /// Called only after offering every earlier logical row or accounting for
    /// an explicitly lost, approved receipt. Private losses are never counted here.
    pub fn advance(&self, before: SessionTimeUs) {
        self.0.progress.before.fetch_max(before.0, Ordering::Release);
    }

    pub fn note_loss(&self, at: SessionTimeUs, reason: LossReason) {
        self.0.budget.omit(at, reason);
    }
}

struct Lane {
    progress: Arc<Progress>,
    before: SessionTimeUs,
    /// The QPC time and suspend sequence at which `before` last changed.
    changed: Option<(QpcTick, u64)>,
    excluded: bool,
}

/// QPC time for the lane-stall watch, which excludes sleep (S0).
pub(super) struct Awake {
    pub now: QpcTick,
    pub marks: SuspendMarks,
    pub frequency: QpcFrequency,
}

impl Awake {
    fn mark(&self) -> (QpcTick, u64) {
        (self.now, self.marks.sequence)
    }
}

pub(super) struct Merge {
    receive: Receiver<Slot>,
    lanes: Vec<Lane>,
    budget: Arc<Budget>,
    ordered: BTreeMap<(SessionTimeUs, u64), Slot>,
    ordinal: u64,
    last: SessionTimeUs,
    certified_before: SessionTimeUs,
    /// Every image `file` and `seen_file` of a row queued to the journal.
    referenced: HashSet<String>,
}

impl Merge {
    pub fn new(count: usize) -> (Vec<Rows>, Self) {
        let (send, receive) = sync_channel(SLOTS);
        let budget = Arc::new(Budget::default());
        let mut rows = Vec::with_capacity(count);
        let mut lanes = Vec::with_capacity(count);
        for _ in 0..count {
            let progress = Arc::new(Progress::default());
            rows.push(Rows(Arc::new(Producer { progress: Arc::clone(&progress), send: send.clone(), budget: Arc::clone(&budget) })));
            lanes.push(Lane { progress, before: SessionTimeUs(0), changed: None, excluded: false });
        }
        (rows, Self {
            receive, lanes, budget, ordered: BTreeMap::new(), ordinal: 0, last: SessionTimeUs(0),
            certified_before: SessionTimeUs(0), referenced: HashSet::new(),
        })
    }

    pub fn finish(&mut self, journal: &mut JournalIngress) -> Result<(), ProbeError> {
        if self.lanes.iter().any(|lane| !lane.progress.closed.load(Ordering::Acquire)) {
            return Err(unavailable("A still producer did not close before journal finalization"));
        }
        // A closed lane no longer holds a cutoff-equal receipt. A disconnected
        // reservation resolves to its approved fallback or is silently retired.
        self.publish_prefix(journal, Instant::now(), None)?;
        if !self.ordered.is_empty() {
            return Err(unavailable("The closed still journal prefix did not drain"));
        }
        Ok(())
    }

    pub fn publish(&mut self, journal: &mut JournalIngress, now: Instant, awake: &Awake) -> Result<(), ProbeError> {
        self.publish_prefix(journal, now, Some(awake))
    }

    pub fn latest_receipt(&self) -> SessionTimeUs { self.last }

    /// Whether a row queued to the journal names this image (`file` relative to `raw/`).
    pub fn references(&self, file: &str) -> bool {
        self.referenced.contains(file)
    }

    pub fn includes(&self, cutoff: SessionTimeUs) -> bool {
        self.certified_before > cutoff && self.ordered.first_key_value().is_none_or(|(key, _)| key.0 > cutoff)
    }

    /// Publish the certified prefix. Without `awake` the merge is finishing:
    /// every lane is closed, and every slot resolves now.
    fn publish_prefix(&mut self, journal: &mut JournalIngress, now: Instant, awake: Option<&Awake>)
        -> Result<(), ProbeError>
    {
        let finishing = awake.is_none();
        let (before, stalled) = self.watch_lanes(awake);
        self.certified_before = before;
        while let Ok(slot) = self.receive.try_recv() {
            self.ordinal = self.ordinal.wrapping_add(1);
            self.ordered.insert((slot.at, self.ordinal), slot);
        }

        loop {
            let first_row = self.ordered.first_key_value().map_or(SessionTimeUs(i64::MAX), |(key, _)| key.0);
            for (reason, gap) in self.budget.take_gaps(before, first_row) {
                let at = self.last.max(gap.last);
                // A coalesced public loss occupies its terminal receipt.
                // Earlier valid rows keep their own receipt order.
                let loss = LossEvidence::ordinary(reason, None, gap.count, gap.first, gap.last);
                journal.record_loss(loss, at)?;
                self.last = at;
            }
            let Some(mut entry) = self.ordered.first_entry() else { break; };
            let key = *entry.key();
            if !finishing && key.0 >= before { break; }
            let slot = entry.get_mut();
            let mut failed = None;
            while slot.owed != 0 {
                match slot.result.try_recv() {
                    Ok((half, completed)) => {
                        slot.owed -= 1;
                        slot.halves[usize::from(half)] = completed;
                    }
                    Err(TryRecvError::Empty) if !finishing && now < slot.deadline => break,
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            if slot.owed != 0 && failed.is_none() { break; }
            let slot = entry.remove();
            // A slot whose halves did not all finish is its approved fallback.
            // An image that a finished half saved is then named by no row; the
            // PNG worker counts it from `references`.
            let row = match failed {
                None => slot.row(),
                Some(error) => {
                    let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                    shared.expired = true;
                    let mut fallback = shared.fallback.take();
                    if error == TryRecvError::Empty && let Some(row) = fallback.as_mut() {
                        row.deadline(finishing);
                    }
                    fallback
                }
            };
            if let Some(row) = row {
                if key.0 < self.last {
                    self.budget.omit(key.0, LossReason::LateRow);
                } else {
                    let files: Vec<String> = row.images().map(str::to_owned).collect();
                    let row = row.value().map_err(|_| unavailable("Encode a still row"))?;
                    if journal.try_submit(key.0, row)? == Submit::Queued {
                        self.referenced.extend(files);
                    }
                    self.last = key.0;
                }
            }
        }
        if stalled != 0 {
            journal.submit_health(HealthRecord {
                kind: HealthKind::StillProgressStall,
                failure: Failure::new(FailureComponent::Stills, "Advance the still producer progress", None),
                detected: self.last, first_affected: None, ongoing: true, monitor: None, count: Some(stalled),
            })?;
        }
        journal.offer_gap(self.last)?;
        Ok(())
    }

    /// The certified receipt bound over the open lanes, and the number of
    /// lanes newly excluded because their progress stalled for `WAIT` of
    /// awake time. A sleep does not count; an interval that cannot be
    /// measured restarts the watch.
    fn watch_lanes(&mut self, awake: Option<&Awake>) -> (SessionTimeUs, u64) {
        // An Acquire snapshot taken before draining the shared reservation
        // channel includes the producer's sends preceding each Release marker.
        let mut before = SessionTimeUs(i64::MAX);
        let mut stalled = 0;
        let idle = self.ordered.is_empty();
        for lane in &mut self.lanes {
            if lane.excluded || lane.progress.closed.load(Ordering::Acquire) { continue; }
            let stamp = SessionTimeUs(lane.progress.before.load(Ordering::Acquire));
            if stamp != lane.before || idle || lane.changed.is_none() {
                lane.before = stamp;
                lane.changed = awake.map(Awake::mark);
            } else if let (Some(awake), Some((since, sequence))) = (awake, lane.changed) {
                match awake.marks.awake_elapsed(since, awake.now, sequence, awake.frequency) {
                    AwakeElapsed::Known(elapsed) if elapsed >= WAIT => {
                        lane.excluded = true;
                        lane.progress.excluded.store(true, Ordering::Release);
                        stalled += 1;
                        continue;
                    }
                    AwakeElapsed::Known(_) => {}
                    AwakeElapsed::Unknown => lane.changed = Some(awake.mark()),
                }
            }
            before = before.min(stamp);
        }
        (before, stalled)
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;
    use crate::{platform::windows::stills::tests::saved_row, protocol::{
        AdmissionEpoch, ClockOrigin, FocusGeneration, WindowId,
        ipc::{ProcessInstance, elevated::{VisitBinding, WindowInstance}},
    }};

    fn coverage(before: QpcTick, last: SessionTimeUs) -> StillCoverage {
        let visit = VisitBinding {
            epoch: AdmissionEpoch(1), source_generation: NonZeroU64::MIN, grant: NonZeroU64::MIN, witness_revision: 0,
            foreground: WindowInstance {
                window: WindowId(1), process: ProcessInstance { pid: NonZeroU32::MIN, creation_filetime: 0 }, thread: NonZeroU32::MIN,
            },
            focus_generation: FocusGeneration(0), focus_revision: 0, resync_revision: NonZeroU64::MIN,
        };
        let clock = ClockOrigin { origin_qpc: QpcTick(0), qpc_frequency: QpcFrequency(NonZeroU64::MIN.saturating_add(999_999)) };
        let visit = FrameCoverage::new(visit, clock);
        visit.restrict(before);
        StillCoverage { visit: Arc::new(visit), last }
    }

    /// A still past its elevated visit's certified bound becomes a dropped row.
    #[test]
    fn publishable_bytes() -> serde_json::Result<()> {
        let uncertified = Completed { row: saved_row(), coverage: Some(coverage(QpcTick(50), SessionTimeUs(100))) };
        assert_eq!(serde_json::to_string(&uncertified.publishable().value()?)?,
            r#"{"adjustment":null,"admission_epoch":3,"element":null,"event_ref":null,"file":null,"frame":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"height":null,"kind":"still_dropped","monitor":1,"outcome":{"kind":"dropped","reason":"elevated_frame_uncertified"},"reason":"elevated_frame_uncertified","role":"before","save":null,"scroll_device":null,"settled_confirmed_t_us":null,"source_t_us":null,"trigger":"pre_action","trigger_t_us":1000,"width":null}"#);
        let certified = Completed { row: saved_row(), coverage: Some(coverage(QpcTick(200), SessionTimeUs(100))) };
        assert_eq!(certified.publishable().value()?, saved_row().value()?);
        Ok(())
    }
}
