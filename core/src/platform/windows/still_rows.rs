//! Receipt-ordered staging for the attempt's still journal. Producers hand off
//! reservations and progress on the same local timeline; job results fill slots.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    capture::frame::FrameCoverage,
    package::{
        diagnostics::{Failure, FailureComponent},
        journal::{HealthRecord, JournalIngress},
        outcome::{HealthKind, LossEvidence, LossReason},
    },
    protocol::SessionTimeUs,
};

use super::{ProbeError, package_journal_error, unavailable};

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
    fallback: Option<Value>,
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
    row: Value,
    coverage: Option<StillCoverage>,
}

impl Completed {
    fn publishable(mut self) -> Value {
        if self.coverage.as_ref().is_some_and(|coverage| !coverage.allows()) {
            self.row["kind"] = serde_json::json!("still_dropped");
            for field in ["source_t_us", "frame_obs_id", "file", "width", "height", "frame", "settled_confirmed_t_us"] {
                self.row[field] = Value::Null;
            }
            self.row["reason"] = serde_json::json!("elevated_frame_uncertified");
            self.row["outcome"] = serde_json::json!({ "kind": "dropped", "reason": "elevated_frame_uncertified" });
        }
        self.row
    }
}

/// One row per trigger (IMG-2): the before half's row with the seen frame's
/// id, and the seen file only when it differs from the row's file.
fn with_seen(mut row: Value, seen: Value) -> Value {
    row["seen_obs_id"] = seen["frame_obs_id"].clone();
    if let Some(file) = seen["file"].as_str().filter(|file| row["file"].as_str() != Some(*file)) {
        row["seen_file"] = file.into();
    }
    row["seen_frame"] = seen["frame"].clone();
    row["seen_outcome"] = seen["outcome"].clone();
    row
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
    pub fn approve(&self, fallback: Value) -> bool {
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.expired { return false; }
        shared.fallback = Some(fallback);
        true
    }

    pub fn suppress(self) {
        let _ = self.result.try_send((self.half, None));
    }

    pub fn finish(self, row: Value) {
        self.finish_frame(row, None);
    }

    pub fn finish_frame(self, row: Value, coverage: Option<StillCoverage>) {
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
    fn row(self) -> Option<Value> {
        let [row, seen] = self.halves;
        let row = row?.publishable();
        Some(match seen {
            Some(seen) => with_seen(row, seen.publishable()),
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
    pub fn reserve(&self, at: SessionTimeUs, fallback: Option<Value>) -> Result<Reservation, LossReason> {
        self.reserve_halves(at, fallback, 1)
    }

    /// Reserve a public row; a failure is accounted as a loss.
    pub fn reserve_public(&self, at: SessionTimeUs, fallback: Value) -> Option<Reservation> {
        self.reserve(at, Some(fallback)).inspect_err(|reason| self.note_loss(at, *reason)).ok()
    }

    /// One row of a pre-motion or pre-action trigger, finished by its before
    /// and seen halves (IMG-2). Its fallback is set at approval.
    pub fn reserve_pair(&self, at: SessionTimeUs) -> Result<[Reservation; 2], LossReason> {
        let before = self.reserve_halves(at, None, 2)?;
        let seen = Reservation { result: before.result.clone(), shared: Arc::clone(&before.shared), half: 1 };
        Ok([before, seen])
    }

    fn reserve_halves(&self, at: SessionTimeUs, fallback: Option<Value>, halves: u8) -> Result<Reservation, LossReason> {
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
    changed: Instant,
    excluded: bool,
}

pub(super) struct Merge {
    receive: Receiver<Slot>,
    lanes: Vec<Lane>,
    budget: Arc<Budget>,
    ordered: BTreeMap<(SessionTimeUs, u64), Slot>,
    ordinal: u64,
    last: SessionTimeUs,
    certified_before: SessionTimeUs,
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
            lanes.push(Lane { progress, before: SessionTimeUs(0), changed: Instant::now(), excluded: false });
        }
        (rows, Self { receive, lanes, budget, ordered: BTreeMap::new(), ordinal: 0, last: SessionTimeUs(0), certified_before: SessionTimeUs(0) })
    }

    pub fn finish(&mut self, journal: &mut JournalIngress) -> Result<(), ProbeError> {
        if self.lanes.iter().any(|lane| !lane.progress.closed.load(Ordering::Acquire)) {
            return Err(unavailable("A still producer did not close before journal finalization"));
        }
        // A closed lane no longer holds a cutoff-equal receipt. A disconnected
        // reservation resolves to its approved fallback or is silently retired.
        self.publish_prefix(journal, Instant::now(), true)?;
        if !self.ordered.is_empty() {
            return Err(unavailable("The closed still journal prefix did not drain"));
        }
        Ok(())
    }

    pub fn publish(&mut self, journal: &mut JournalIngress, now: Instant) -> Result<(), ProbeError> {
        self.publish_prefix(journal, now, false)
    }

    pub fn latest_receipt(&self) -> SessionTimeUs { self.last }

    pub fn includes(&self, cutoff: SessionTimeUs) -> bool {
        self.certified_before > cutoff && self.ordered.first_key_value().is_none_or(|(key, _)| key.0 > cutoff)
    }

    fn publish_prefix(&mut self, journal: &mut JournalIngress, now: Instant, finishing: bool)
        -> Result<(), ProbeError>
    {
        // An Acquire snapshot taken before draining the shared reservation
        // channel includes the producer's sends preceding each Release marker.
        let mut before = SessionTimeUs(i64::MAX);
        let mut stalled = 0;
        for lane in &mut self.lanes {
            if lane.excluded || lane.progress.closed.load(Ordering::Acquire) { continue; }
            let stamp = SessionTimeUs(lane.progress.before.load(Ordering::Acquire));
            if stamp != lane.before || self.ordered.is_empty() {
                lane.before = stamp;
                lane.changed = now;
            }
            if now.saturating_duration_since(lane.changed) >= WAIT {
                lane.excluded = true;
                lane.progress.excluded.store(true, Ordering::Release);
                stalled += 1;
                continue;
            }
            before = before.min(stamp);
        }
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
                journal.record_loss(loss, at).map_err(package_journal_error)?;
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
            // A half that already saved its PNG leaves that file unreferenced
            // and uncounted, so `unreferenced_images` is a lower bound.
            let row = match failed {
                None => slot.row(),
                Some(error) => {
                    let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                    shared.expired = true;
                    let mut fallback = shared.fallback.take();
                    if error == TryRecvError::Empty && let Some(row) = fallback.as_mut() {
                        let reason = row["reason"].take();
                        row["fallback_reason"] = reason;
                        row["reason"] = serde_json::json!(
                            if finishing { "still_owner_closed" } else { "still_ordered_deadline" }
                        );
                    }
                    fallback
                }
            };
            if let Some(row) = row {
                if key.0 < self.last {
                    self.budget.omit(key.0, LossReason::LateRow);
                } else {
                    journal.try_submit(key.0, row).map_err(package_journal_error)?;
                    self.last = key.0;
                }
            }
        }
        if stalled != 0 {
            journal.submit_health(HealthRecord {
                kind: HealthKind::StillProgressStall,
                failure: Failure::new(FailureComponent::Stills, "Advance the still producer progress", None),
                detected: self.last, first_affected: None, ongoing: true, monitor: None, count: Some(stalled),
            }).map_err(package_journal_error)?;
        }
        journal.offer_gap(self.last).map_err(package_journal_error)?;
        Ok(())
    }
}
