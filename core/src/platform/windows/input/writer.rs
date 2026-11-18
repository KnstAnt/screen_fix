//! The input pump's two journal streams. Each writer owns its ingress with
//! the receipt prefix it has published, so a row is never submitted without
//! advancing that prefix, and neither is reachable apart from the other.

use std::{collections::BTreeMap, sync::mpsc, thread, time::{Duration, Instant}};

use serde_json::Value;

use crate::{
    package::{
        journal::{Flush, FlushOutcome, FlushRequest, HealthRecord, JournalIngress, Submit},
        outcome::{HealthKind, LossEvidence, SuppressionClass},
        row::RowEnvelope,
    },
    platform::windows::controller::{Receipt, WorkerSession},
    protocol::{AdmissionEpoch, CoarseTime, EventRef, MessageTickMs, QpcTick, RecordTime, SessionTimeUs},
};

use super::{ProbeError, clock, unavailable};

/// The bound of a lifecycle journal flush at a pause or stop boundary.
const BOUNDARY_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// The published prefix of one stream: its queued rows and the latest
/// receipts it admitted.
#[derive(Default)]
pub(in crate::platform::windows) struct ReceiptPrefix {
    pub(in crate::platform::windows) queued: u64,
    pub(in crate::platform::windows) last_queued: Option<SessionTimeUs>,
    last_receipt: Option<QpcTick>,
    last_time: Option<SessionTimeUs>,
}

impl ReceiptPrefix {
    fn advance(&mut self, received: QpcTick, admitted: SessionTimeUs, disposition: Submit) -> Result<(), ProbeError> {
        if self.last_receipt.is_some_and(|previous| previous > received) {
            return Err(unavailable("Input receipts are out of order"));
        }
        self.last_receipt = Some(received);
        self.advance_time(admitted, disposition)
    }

    /// A ledger admission raised the stream's receipt without adding a row.
    fn admit(&mut self, admitted: SessionTimeUs) -> Result<(), ProbeError> {
        if self.last_time.is_some_and(|previous| previous > admitted) {
            return Err(unavailable("Input receipts are out of order"));
        }
        self.last_time = Some(admitted);
        Ok(())
    }

    /// `at` raised to the published prefix.
    fn raised(&self, at: SessionTimeUs) -> SessionTimeUs {
        self.last_time.map_or(at, |last| last.max(at))
    }

    fn advance_time(&mut self, admitted: SessionTimeUs, disposition: Submit) -> Result<(), ProbeError> {
        self.admit(admitted)?;
        if disposition == Submit::Queued {
            self.queued = self.queued.checked_add(1)
                .ok_or_else(|| unavailable("The input record counter overflowed"))?;
            self.last_queued = Some(admitted);
        }
        Ok(())
    }
}

/// The context stream. Every row carries the receipt timing of the poll
/// that wrote it, and takes the next context index.
///
/// Fields drop in this order: the pending flush outcome before the ingress.
pub(super) struct ContextWriter {
    flush: DeferredFlush,
    prefix: ReceiptPrefix,
    next_index: u64,
    journal: JournalIngress,
}

impl ContextWriter {
    pub(super) fn new(journal: JournalIngress) -> Self {
        Self { flush: DeferredFlush::default(), prefix: ReceiptPrefix::default(), next_index: 0, journal }
    }

    /// The timing of a context row received at `received`; none before the
    /// admission boundary.
    pub(super) fn timing(receipt: Receipt, received: QpcTick) -> Result<Option<InputTiming>, ProbeError> {
        input_timing(receipt, received, None)
    }

    /// The context index the next row takes.
    pub(super) fn next_index(&self) -> u64 {
        self.next_index
    }

    /// The latest receipt the stream admitted.
    pub(super) fn last_receipt(&self) -> Option<QpcTick> {
        self.prefix.last_receipt
    }

    /// Offer a row in normal capacity; a full stream drops it.
    pub(super) fn event(&mut self, event: Value, timing: InputTiming) -> Result<(), ProbeError> {
        self.send(event, timing, false)
    }

    /// Queue a lifecycle row in reserved capacity.
    pub(super) fn boundary(&mut self, event: Value, timing: InputTiming) -> Result<(), ProbeError> {
        self.send(event, timing, true)
    }

    /// Queue lifecycle rows as one batch in reserved capacity.
    pub(super) fn boundaries(&mut self, mut events: Vec<Value>, timing: InputTiming) -> Result<(), ProbeError> {
        let count = events.len();
        for event in &mut events {
            apply_context_envelope(event, timing)?;
        }
        self.journal.submit_boundary(timing.time.recv_t_us, events)?;
        for _ in 0..count {
            self.prefix.advance(timing.received, timing.time.recv_t_us, Submit::Queued)?;
            self.advance_index()?;
        }
        Ok(())
    }

    fn send(&mut self, mut event: Value, timing: InputTiming, lifecycle: bool) -> Result<(), ProbeError> {
        apply_context_envelope(&mut event, timing)?;
        let disposition = if lifecycle {
            self.journal.submit_boundary(timing.time.recv_t_us, [event])?;
            Submit::Queued
        } else {
            self.journal.try_submit(timing.time.recv_t_us, event)?
        };
        self.prefix.advance(timing.received, timing.time.recv_t_us, disposition)?;
        self.advance_index()
    }

    fn advance_index(&mut self) -> Result<(), ProbeError> {
        // Control rows use this context index. The journal writer allocates its own sequence.
        self.next_index = self.next_index.checked_add(1)
            .ok_or_else(|| unavailable("The recording event counter overflowed"))?;
        Ok(())
    }

    pub(super) fn submit_health(&mut self, record: HealthRecord) -> Result<(), ProbeError> {
        Ok(self.journal.submit_health(record)?)
    }

    /// Record a context loss admitted at `at` raised to the published prefix.
    pub(super) fn record_loss(&mut self, evidence: LossEvidence, at: SessionTimeUs) -> Result<(), ProbeError> {
        let admitted = self.prefix.raised(at);
        Ok(self.journal.record_loss(evidence, admitted)?)
    }

    pub(super) fn record_suppressed(&mut self, class: SuppressionClass, count: u64) {
        self.journal.record_suppressed(class, count);
    }

    /// Emit pending gap entries at `at` when capacity allows.
    pub(super) fn offer_gap(&mut self, at: SessionTimeUs) -> Result<(), ProbeError> {
        self.journal.offer_gap(at)?;
        Ok(())
    }

    /// Queue an OS flush through `at` when one is due or `requested`.
    pub(super) fn flush(&mut self, at: SessionTimeUs, requested: bool) -> Result<(), ProbeError> {
        self.flush.poll(&mut self.journal, at, requested)
    }

    /// Flush the lifecycle prefix through `at`, running `drain` while waiting.
    pub(super) fn flush_boundary(
        &mut self, at: SessionTimeUs, session: &WorkerSession, drain: impl FnMut() -> Result<(), ProbeError>,
    ) -> Result<(), ProbeError> {
        flush_boundary(&mut self.journal, at, session, drain)
    }

    pub(super) fn pause(&mut self) {
        self.journal.pause();
    }

    pub(super) fn take_paused_health(&mut self) -> BTreeMap<HealthKind, u64> {
        self.journal.take_paused_health()
    }

    pub(super) fn resume(&mut self) {
        self.journal.resume();
    }

    /// Close the stream at its closing boundary.
    pub(super) fn seal(self, at: SessionTimeUs) -> Result<ReceiptPrefix, ProbeError> {
        self.journal.seal(at)?;
        Ok(self.prefix)
    }

    /// The published prefix of a stream closed without a boundary.
    pub(super) fn into_prefix(self) -> ReceiptPrefix {
        self.prefix
    }
}

/// The input stream, published by the gate in receipt order.
///
/// Fields drop in this order: the pending flush outcome before the ingress.
pub(super) struct InputWriter {
    flush: DeferredFlush,
    prefix: ReceiptPrefix,
    journal: JournalIngress,
}

impl InputWriter {
    pub(super) fn new(journal: JournalIngress) -> Self {
        Self { flush: DeferredFlush::default(), prefix: ReceiptPrefix::default(), journal }
    }

    /// The latest receipt the stream published.
    pub(super) fn published(&self) -> Option<SessionTimeUs> {
        self.prefix.last_time
    }

    /// The published prefix, or `boundary` when nothing was published yet.
    pub(super) fn published_or(&self, boundary: SessionTimeUs) -> SessionTimeUs {
        self.prefix.last_time.unwrap_or(boundary)
    }

    /// `at` raised to the published prefix.
    pub(super) fn raised(&self, at: SessionTimeUs) -> SessionTimeUs {
        self.prefix.raised(at)
    }

    /// Offer a row at `at` in normal capacity; a full stream drops it.
    pub(super) fn submit(&mut self, at: SessionTimeUs, row: Value) -> Result<Submit, ProbeError> {
        let disposition = self.journal.try_submit(at, row)?;
        self.prefix.advance_time(at, disposition)?;
        Ok(disposition)
    }

    /// [`Self::submit`], with the row's event reference once it is appended.
    pub(super) fn submit_confirmed(&mut self, at: SessionTimeUs, row: Value)
        -> Result<(Submit, mpsc::Receiver<EventRef>), ProbeError>
    {
        let (disposition, event) = self.journal.try_submit_confirmed(at, row)?;
        self.prefix.advance_time(at, disposition)?;
        Ok((disposition, event))
    }

    /// Queue lifecycle rows at `at`; each is queued and advances the prefix.
    pub(super) fn submit_boundary(&mut self, at: SessionTimeUs, rows: Vec<Value>) -> Result<(), ProbeError> {
        let count = rows.len();
        self.journal.submit_boundary(at, rows)?;
        (0..count).try_for_each(|_| self.prefix.advance_time(at, Submit::Queued))
    }

    /// Write a health condition admitted at its detection raised to the prefix.
    pub(super) fn submit_health(&mut self, record: HealthRecord) -> Result<(), ProbeError> {
        let admitted = self.prefix.raised(record.detected);
        self.journal.submit_health(record)?;
        self.prefix.admit(admitted)
    }

    /// Record a loss admitted at `at` raised to the prefix, which it advances.
    pub(super) fn admit_loss(&mut self, evidence: LossEvidence, at: SessionTimeUs) -> Result<(), ProbeError> {
        let admitted = self.prefix.raised(at);
        self.journal.record_loss(evidence, admitted)?;
        self.prefix.admit(admitted)
    }

    /// Record a loss at `admitted` without advancing the prefix.
    pub(super) fn record_loss(&mut self, evidence: LossEvidence, admitted: SessionTimeUs) -> Result<(), ProbeError> {
        Ok(self.journal.record_loss(evidence, admitted)?)
    }

    pub(super) fn record_suppressed(&mut self, class: SuppressionClass, count: u64) {
        self.journal.record_suppressed(class, count);
    }

    /// Queue an OS flush when one is due or `requested`. Pending input losses
    /// are admitted at the published prefix, never ahead of records that the
    /// matcher or gate still holds.
    pub(super) fn flush(&mut self, boundary: SessionTimeUs, requested: bool) -> Result<(), ProbeError> {
        let at = self.published_or(boundary);
        self.flush.poll(&mut self.journal, at, requested)
    }

    /// Flush the lifecycle prefix through `at`, running `drain` while waiting.
    pub(super) fn flush_boundary(
        &mut self, at: SessionTimeUs, session: &WorkerSession, drain: impl FnMut() -> Result<(), ProbeError>,
    ) -> Result<(), ProbeError> {
        flush_boundary(&mut self.journal, at, session, drain)
    }

    pub(super) fn pause(&mut self) {
        self.journal.pause();
    }

    pub(super) fn take_paused_health(&mut self) -> BTreeMap<HealthKind, u64> {
        self.journal.take_paused_health()
    }

    pub(super) fn resume(&mut self) {
        self.journal.resume();
    }

    /// Close the stream at its closing boundary.
    pub(super) fn seal(self, at: SessionTimeUs) -> Result<ReceiptPrefix, ProbeError> {
        self.journal.seal(at)?;
        Ok(self.prefix)
    }

    /// The published prefix of a stream closed without a boundary.
    pub(super) fn into_prefix(self) -> ReceiptPrefix {
        self.prefix
    }
}

/// Queue OS flushes without introducing disk waits on the collection path.
#[derive(Default)]
struct DeferredFlush {
    requested: bool,
    waiting: Option<FlushOutcome>,
}

impl DeferredFlush {
    fn poll(&mut self, journal: &mut JournalIngress, at: SessionTimeUs, requested: bool) -> Result<(), ProbeError> {
        self.requested |= requested;
        if let Some(waiting) = &self.waiting {
            if !poll_flush(waiting, "A requested journal flush did not complete")? {
                return Ok(());
            }
            self.waiting = None;
        }
        if self.requested && let FlushRequest::Queued(waiting) = journal.try_flush(Flush::Os, at)? {
            self.waiting = Some(waiting);
            self.requested = false;
        }
        Ok(())
    }
}

/// Whether a queued flush completed; its failure, or `lost` when the journal
/// dropped it, is an error.
fn poll_flush(waiting: &FlushOutcome, lost: &'static str) -> Result<bool, ProbeError> {
    match waiting.try_recv() {
        Ok(outcome) => outcome.map(|()| true).map_err(ProbeError::from),
        Err(mpsc::TryRecvError::Empty) => Ok(false),
        Err(mpsc::TryRecvError::Disconnected) => Err(unavailable(lost)),
    }
}

fn flush_boundary(
    journal: &mut JournalIngress, at: SessionTimeUs, session: &WorkerSession,
    mut drain: impl FnMut() -> Result<(), ProbeError>,
) -> Result<(), ProbeError> {
    let deadline = Instant::now() + BOUNDARY_FLUSH_TIMEOUT;
    let mut waiting: Option<FlushOutcome> = None;
    loop {
        if let Some(result) = &waiting {
            if poll_flush(result, "The journal pause flush did not complete")? {
                return Ok(());
            }
        } else if let FlushRequest::Queued(result) = journal.try_flush(Flush::Os, at)? {
            waiting = Some(result);
        }
        if Instant::now() >= session.stop_deadline().map_or(deadline, |stop| stop.min(deadline)) {
            return Err(unavailable("The lifecycle journal flush exceeded its deadline"));
        }
        drain()?;
        thread::park_timeout(Duration::from_millis(2));
    }
}

/// The receipt timing of one row.
#[derive(Clone, Copy)]
pub(super) struct InputTiming {
    pub(super) time: RecordTime,
    pub(super) received: QpcTick,
    pub(super) admitted_100ns: i64,
    pub(super) epoch: AdmissionEpoch,
}

/// The timing of a record received at `received` with its message `tick`;
/// none before the admission boundary.
pub(super) fn input_timing(
    receipt: Receipt,
    received: QpcTick,
    tick: Option<MessageTickMs>,
) -> Result<Option<InputTiming>, ProbeError> {
    if received < receipt.boundary.qpc {
        return Ok(None);
    }
    let recv_t_us = receipt.clock.time(received)?;
    let coarse = tick.map(CoarseTime::windows_message);
    // A queued message can survive the boundary. Exclude it only when
    // its entire coarse time interval predates admission.
    if coarse.and_then(|coarse| clock::message_tick_estimate(receipt.clock, receipt.tick_anchor, coarse))
        .is_some_and(|estimate| estimate.predates(receipt.boundary.time))
    {
        return Ok(None);
    }
    Ok(Some(InputTiming {
        time: RecordTime { recv_t_us, source_t_us: None, source_uncertainty_us: None, coarse },
        received,
        admitted_100ns: receipt.clock.offset_100ns(received)?,
        epoch: receipt.boundary.epoch,
    }))
}

/// The envelope of a context row. The journal stamps its `recv_t_us`, the
/// receipt it is submitted at. The package format version (manifest v6)
/// versions every row; a row's own `version` is the foreground product version (D-1).
fn apply_context_envelope(event: &mut Value, timing: InputTiming) -> Result<(), ProbeError> {
    RowEnvelope::new(timing.time, None, timing.epoch).write_keeping_tick(event)
        .map_err(|_| unavailable("The input record is not an object"))
}
