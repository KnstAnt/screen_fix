//! A bounded ingress and one append-only file owner for each logical stream.
//!
//! The ingress keeps a keyed loss ledger outside the data queue. Every pending
//! ledger entry becomes a typed `gap` row before the next row of the stream,
//! at a boundary, or at close. Gap rows never compete with a full data queue
//! for their existence: at worst they stay pending until the ingress tail.
//! Boundary and health rows use a fixed room reserved above the data limit and
//! shared by every lifecycle entry; when that room is full the submission
//! fails with [`JournalStep::Reserve`], distinct from a failed writer, and the
//! pending gaps stay in the ledger.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::{EventRef, MonitorId, SessionTimeUs, StreamId, StreamSequence};

use super::{
    diagnostics::Failure,
    metadata::InputBlind,
    outcome::{DroppedTotal, HealthKind, LossCount, LossEvidence, LossProducer, LossRange, LossReason, LossScope,
        OrdinaryLoss, SuppressionClass, UiaOutcome, UiaRead},
    row::{BlindRow, RowCounts, RowKind, RowTally, stamp_receipt},
};

const BUFFER_INTERVAL: Duration = Duration::from_secs(1);
const DISK_INTERVAL: Duration = Duration::from_secs(10);
pub(super) const MAX_ROW_BYTES: usize = 1024 * 1024;
/// Lifecycle entries (gap batches, boundary and health rows) that may wait in
/// the writer queue above the data limit at once.
const BOUNDARY_ENTRIES: usize = 24;
const MAX_BOUNDARY_ROWS: usize = 512;
const MAX_BOUNDARY_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JournalStep {
    Start,
    Create,
    Order,
    Encode,
    Append,
    /// The room reserved for lifecycle entries was full.
    Reserve,
    BufferedFlush,
    OsFlush,
    Close,
}

#[derive(Debug)]
pub(crate) struct JournalError {
    pub(crate) stream: StreamId,
    pub(crate) step: JournalStep,
    pub(crate) source: Option<io::Error>,
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = match self.step {
            JournalStep::Start => "start",
            JournalStep::Create => "create",
            JournalStep::Order => "order records in",
            JournalStep::Encode => "encode a record for",
            JournalStep::Append => "append to",
            JournalStep::Reserve => "reserve lifecycle room in",
            JournalStep::BufferedFlush => "flush the buffer of",
            JournalStep::OsFlush => "flush to disk for",
            JournalStep::Close => "close",
        };
        write!(f, "Could not {action} the {:?} journal.", self.stream)
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

/// Closed totals of one stream. `offered` counts every item that reached a
/// terminal outcome through the ingress: persisted data rows, ledger losses
/// with a known count and policy suppressions.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StreamSummary {
    pub rows: u64,
    pub data_rows: u64,
    /// Sum of the known counts of this stream's gap rows.
    pub dropped: u64,
    #[serde(default)]
    pub gap_rows: u64,
    #[serde(default)]
    pub offered: u64,
    #[serde(default)]
    pub health_rows: u64,
    #[serde(default)]
    pub pause_rows: u64,
    #[serde(default)]
    pub blind_rows: u64,
    /// Unapproved-foreground suppression interval rows and their total duration (C5).
    #[serde(default)]
    pub suppressed_interval_rows: u64,
    #[serde(default)]
    pub suppressed_interval_us: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dropped_by_reason: BTreeMap<LossReason, DroppedTotal>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub suppressed_by_class: BTreeMap<SuppressionClass, u64>,
    /// Persisted `still_dropped` rows, by their reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unavailable_by_reason: BTreeMap<String, u64>,
    /// Persisted focus and element rows whose UI Automation read has no
    /// element, by read and outcome.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub uia_unavailable: BTreeMap<UiaRead, BTreeMap<UiaOutcome, u64>>,
    /// Health conditions counted instead of written while the stream was paused.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub paused_health: BTreeMap<HealthKind, u64>,
    #[serde(default)]
    pub buffered_flushes: u64,
    #[serde(default)]
    pub os_flushes: u64,
    #[serde(default)]
    pub os_flush_max_us: u64,
    #[serde(default)]
    pub normal_entries_peak: usize,
    #[serde(default)]
    pub prepared_bytes_peak: usize,
    pub last_receipt: Option<SessionTimeUs>,
    pub last_data_receipt: Option<SessionTimeUs>,
}

impl StreamSummary {
    /// The terminal outcomes reconcile with `offered` unless a loss count is unknown.
    pub(crate) fn outcome_total(&self) -> Option<u64> {
        let mut total = self.data_rows;
        for dropped in self.dropped_by_reason.values() {
            if dropped.unknown_gaps != 0 || dropped.sensitive_unknown != 0 { return None; }
            total = total.checked_add(dropped.gap_items)?.checked_add(dropped.sensitive_items)?;
        }
        self.suppressed_by_class.values().try_fold(total, |total, count| total.checked_add(*count))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Flush {
    Buffered,
    Os,
}

impl Flush {
    fn step(self) -> JournalStep {
        match self {
            Self::Buffered => JournalStep::BufferedFlush,
            Self::Os => JournalStep::OsFlush,
        }
    }
}

pub(crate) type FlushOutcome = Receiver<Result<(), JournalError>>;

pub(crate) enum FlushRequest {
    Queued(FlushOutcome),
    Busy,
}

/// One runtime or final health condition. The row's receipt is never null:
/// a condition detected before the session clock exists goes only to diagnostics.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HealthRecord {
    pub(crate) kind: HealthKind,
    pub(crate) failure: Failure,
    pub(crate) detected: SessionTimeUs,
    pub(crate) first_affected: Option<SessionTimeUs>,
    pub(crate) ongoing: bool,
    pub(crate) monitor: Option<MonitorId>,
    pub(crate) count: Option<u64>,
}

impl HealthRecord {
    fn row(&self, stream: StreamId) -> Value {
        let failure = self.failure.without_report_detail();
        serde_json::json!({
            "kind": RowKind::health(stream),
            "health": self.kind,
            "component": failure.component(),
            "failure": failure,
            "detected_t_us": self.detected,
            "first_affected_t_us": self.first_affected,
            "ongoing": self.ongoing,
            "monitor": self.monitor,
            "count": self.count,
        })
    }
}

/// A row written after its stream's ingress closed (§6.1): a context health
/// condition, or the input blind interval of an input activity loss.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FinalRow {
    Health(HealthRecord),
    Blind(InputBlind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Submit {
    Queued,
    Dropped,
}

/// Gap rows are written in ascending first loss time.
fn gap_order(loss: &OrdinaryLoss) -> (SessionTimeUs, SessionTimeUs) {
    (loss.range.first_t_us, loss.range.last_t_us)
}

/// Pending ledger entries never merge across keys.
type LedgerKey = (LossReason, Option<LossProducer>);

fn add_count<K: Ord>(totals: &mut BTreeMap<K, u64>, key: K, count: u64) {
    let total = totals.entry(key).or_default();
    *total = total.saturating_add(count);
}

struct QueuedRow {
    receipt: SessionTimeUs,
    object: Box<[u8]>,
    tally: RowTally,
    bytes_held: Arc<AtomicUsize>,
}

impl Drop for QueuedRow {
    fn drop(&mut self) {
        self.bytes_held.fetch_sub(self.object.len(), Ordering::Relaxed);
    }
}

enum Entry {
    Data(QueuedRow, Option<SyncSender<EventRef>>),
    Boundary(SessionTimeUs, Vec<(Box<[u8]>, RowTally)>),
    Gaps(SessionTimeUs, Vec<OrdinaryLoss>),
    Health(SessionTimeUs, Box<[u8]>),
    Flush(Flush, SyncSender<Result<(), JournalError>>),
}

impl Entry {
    /// The step a disconnected writer fails for this entry.
    fn step(&self) -> JournalStep {
        match self {
            Self::Flush(kind, _) => kind.step(),
            _ => JournalStep::Append,
        }
    }
}

struct EntryCredit(Arc<AtomicUsize>);

impl Drop for EntryCredit {
    fn drop(&mut self) { self.0.fetch_sub(1, Ordering::Relaxed); }
}

struct QueuedEntry {
    entry: Entry,
    _credit: Option<EntryCredit>,
}

impl QueuedEntry {
    fn priority(entry: Entry) -> Self { Self { entry, _credit: None } }
}

#[derive(Default)]
struct QueuePeaks {
    normal_entries: AtomicUsize,
    prepared_bytes: AtomicUsize,
}

/// Ingress-side totals delivered to the writer once, when the ingress closes.
#[derive(Default)]
struct IngressTotals {
    offered: u64,
    suppressed: BTreeMap<SuppressionClass, u64>,
    sensitive: BTreeMap<LossReason, DroppedTotal>,
    paused_health: BTreeMap<HealthKind, u64>,
}

/// The single reserved message an ingress sends when it closes.
struct IngressTail {
    admission: Option<SessionTimeUs>,
    pending: Vec<OrdinaryLoss>,
    unreported_paused_health: BTreeMap<HealthKind, u64>,
    totals: IngressTotals,
}

/// How an entry may use the entry queue.
#[derive(Clone, Copy)]
enum Capacity {
    /// Within the normal entry limit; a full queue refuses the entry.
    Normal,
    /// Lifecycle entries in the room reserved above the data limit; a full
    /// reserve refuses the entry.
    Reserved,
    /// Frames rows, outside the normal entry count: the frames queue has no
    /// reserve, so its channel bound is the data limit.
    Unreserved,
    /// Waits for room; only for owners that may wait on IO.
    Blocking,
}

/// Only the logical stream's receipt-order stage owns this endpoint.
/// It does not expose Clone or any file access.
pub(crate) struct JournalIngress {
    stream: StreamId,
    entries: SyncSender<QueuedEntry>,
    normal_entries: Arc<AtomicUsize>,
    row_limit: usize,
    tail: SyncSender<IngressTail>,
    bytes_held: Arc<AtomicUsize>,
    byte_limit: usize,
    peaks: Arc<QueuePeaks>,
    /// The latest receipt this ingress admitted: a queued or dropped row, or a ledger admission.
    last_admission: Option<SessionTimeUs>,
    ledger: BTreeMap<LedgerKey, OrdinaryLoss>,
    totals: IngressTotals,
    /// Health counted, not written, while the controller holds a pause.
    paused: Option<BTreeMap<HealthKind, u64>>,
    sealed: Option<SessionTimeUs>,
}

impl JournalIngress {
    pub(crate) fn try_submit(&mut self, receipt: SessionTimeUs, value: Value) -> Result<Submit, JournalError> {
        self.submit(receipt, value, None)
    }

    /// A dependent observation can retain this receipt until its input row
    /// has actually been assigned and appended by the sole writer.
    pub(crate) fn try_submit_confirmed(&mut self, receipt: SessionTimeUs, value: Value) -> Result<(Submit, Receiver<EventRef>), JournalError> {
        let (reply, confirmed) = mpsc::sync_channel(1);
        self.submit(receipt, value, Some(reply)).map(|submitted| (submitted, confirmed))
    }

    fn submit(&mut self, receipt: SessionTimeUs, value: Value, confirmation: Option<SyncSender<EventRef>>) -> Result<Submit, JournalError> {
        self.totals.offered = self.totals.offered.saturating_add(1);
        let row = self.prepare(receipt, value)?;
        // A data row follows its pending gaps. While paused, pending gaps wait
        // for the resume rows (gate 4); they cover only receipts at or after the
        // pause cutoff, so a row admitted inside the pause keeps journal order.
        let ordered = self.paused.is_some() || self.emit_pending(receipt, Capacity::Normal)?;
        if ordered && let Some(row) = row && self.queue(Entry::Data(row, confirmation), Capacity::Normal)? {
            return Ok(Submit::Queued);
        }
        self.ledger_add(OrdinaryLoss {
            reason: LossReason::QueueOverflow, producer: None, count: LossCount::Known(std::num::NonZeroU64::MIN),
            range: LossRange { first_t_us: receipt, last_t_us: receipt, source_seq: None, qpc: None },
        }, receipt)?;
        Ok(Submit::Dropped)
    }

    /// Queue one entry in `room`. `false` means the room was full and the
    /// entry was dropped; a disconnected writer is an error.
    fn queue(&self, entry: Entry, room: Capacity) -> Result<bool, JournalError> {
        let step = entry.step();
        let sent = match room {
            Capacity::Normal => self.offer_entry(entry),
            Capacity::Reserved | Capacity::Unreserved => self.entries.try_send(QueuedEntry::priority(entry)),
            Capacity::Blocking => self.entries.send(QueuedEntry::priority(entry))
                .map_err(|mpsc::SendError(queued)| TrySendError::Disconnected(queued)),
        };
        match sent {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(self.error(step, None)),
        }
    }

    fn offer_entry(&self, entry: Entry) -> Result<(), TrySendError<QueuedEntry>> {
        let Ok(previous) = self.normal_entries.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < self.row_limit).then_some(count + 1)
        }) else {
            return Err(TrySendError::Full(QueuedEntry::priority(entry)));
        };
        self.peaks.normal_entries.fetch_max(previous + 1, Ordering::Relaxed);
        let credit = EntryCredit(Arc::clone(&self.normal_entries));
        self.entries.try_send(QueuedEntry { entry, _credit: Some(credit) })
    }

    /// The single receipt owner queues a lifecycle batch in FIFO order using
    /// room reserved above the normal entry limit. No producer waits on IO.
    /// Pending gaps precede the batch unless the stream is paused; then they
    /// wait until after the resume rows (gate 4).
    pub(crate) fn submit_boundary(
        &mut self, receipt: SessionTimeUs, rows: impl IntoIterator<Item = Value>,
    ) -> Result<(), JournalError> {
        let mut encoded = Vec::new();
        let mut bytes = 0;
        for value in rows {
            let tally = RowTally::of(&value);
            let row = self.encode(receipt, value)?;
            if encoded.len() == MAX_BOUNDARY_ROWS || row.len() > MAX_BOUNDARY_BYTES - bytes {
                return Err(self.error(JournalStep::Encode, None));
            }
            bytes += row.len();
            encoded.push((row, tally));
        }
        if encoded.is_empty() { return Ok(()); }
        self.totals.offered = self.totals.offered.saturating_add(encoded.len() as u64);
        if self.paused.is_none() && !self.emit_pending(receipt, Capacity::Reserved)? {
            return Err(self.error(JournalStep::Reserve, None));
        }
        self.send_reserved(Entry::Boundary(receipt, encoded))
    }

    /// Write one health condition in reserved capacity, or count it while
    /// paused (PRIV-2). The receipt is the detection receipt, raised to the
    /// stream's last receipt so journal order is preserved.
    pub(crate) fn submit_health(&mut self, health: HealthRecord) -> Result<(), JournalError> {
        if let Some(paused) = &mut self.paused {
            add_count(paused, health.kind, 1);
            return Ok(());
        }
        let receipt = self.last_admission.map_or(health.detected, |last| last.max(health.detected));
        let row = self.encode(receipt, health.row(self.stream))?;
        if !self.emit_pending(receipt, Capacity::Reserved)? {
            return Err(self.error(JournalStep::Reserve, None));
        }
        self.send_reserved(Entry::Health(receipt, row))
    }

    /// Account a policy suppression. Suppressions have no times.
    pub(crate) fn record_suppressed(&mut self, class: SuppressionClass, count: u64) {
        if count == 0 { return; }
        self.totals.offered = self.totals.offered.saturating_add(count);
        add_count(&mut self.totals.suppressed, class, count);
    }

    /// Record typed loss evidence. An ordinary loss keeps its original range
    /// while its gap row joins the stream at a nondecreasing admission receipt.
    /// A sensitive loss is only counted.
    pub(crate) fn record_loss(&mut self, evidence: LossEvidence, admitted: SessionTimeUs) -> Result<(), JournalError> {
        if let Some(count) = evidence.count.known() {
            self.totals.offered = self.totals.offered.saturating_add(count);
        }
        match evidence.scope {
            LossScope::Sensitive => {
                let total = self.totals.sensitive.entry(evidence.reason).or_default();
                match evidence.count.known() {
                    Some(count) => total.sensitive_items = total.sensitive_items.saturating_add(count),
                    None => total.sensitive_unknown = total.sensitive_unknown.saturating_add(1),
                }
                Ok(())
            }
            LossScope::Ordinary(range) => {
                let loss = OrdinaryLoss { reason: evidence.reason, producer: evidence.producer, count: evidence.count, range };
                if !loss.record().is_valid(admitted) {
                    return Err(self.error(JournalStep::Order, None));
                }
                self.ledger_add(loss, admitted)
            }
        }
    }

    fn ledger_add(&mut self, loss: OrdinaryLoss, admitted: SessionTimeUs) -> Result<(), JournalError> {
        self.check_order(admitted)?;
        self.ledger.entry((loss.reason, loss.producer))
            .and_modify(|pending| pending.merge(loss))
            .or_insert(loss);
        self.last_admission = Some(admitted);
        Ok(())
    }

    /// Emit every pending entry, in ascending first loss time, at one admission
    /// receipt. `false` means the normal or reserved room was full and the
    /// entries stay pending.
    fn emit_pending(&mut self, admission: SessionTimeUs, capacity: Capacity) -> Result<bool, JournalError> {
        if self.ledger.is_empty() { return Ok(true); }
        self.check_order(admission)?;
        let mut gaps: Vec<_> = self.ledger.values().copied().collect();
        gaps.sort_by_key(gap_order);
        if !self.queue(Entry::Gaps(admission, gaps), capacity)? {
            return Ok(false);
        }
        self.ledger.clear();
        self.last_admission = Some(admission);
        Ok(true)
    }

    fn prepare(&mut self, receipt: SessionTimeUs, value: Value) -> Result<Option<QueuedRow>, JournalError> {
        let tally = RowTally::of(&value);
        let object = self.encode(receipt, value)?;
        if !self.reserve(object.len()) {
            return Ok(None);
        }
        Ok(Some(QueuedRow { receipt, object, tally, bytes_held: Arc::clone(&self.bytes_held) }))
    }

    fn encode(&mut self, receipt: SessionTimeUs, mut value: Value) -> Result<Box<[u8]>, JournalError> {
        self.check_order(receipt)?;
        value.as_object_mut().ok_or_else(|| self.error(JournalStep::Encode, None))?.remove("seq");
        stamp_receipt(&mut value, receipt).map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        let mut row = RowBytes(Vec::new());
        serde_json::to_writer(&mut row, &value)
            .map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        self.last_admission = Some(receipt);
        Ok(row.0.into_boxed_slice())
    }

    fn reserve(&self, bytes: usize) -> bool {
        let Ok(previous) = self.bytes_held.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            let updated = current.checked_add(bytes)?;
            (updated <= self.byte_limit).then_some(updated)
        }) else {
            return false;
        };
        self.peaks.prepared_bytes.fetch_max(previous + bytes, Ordering::Relaxed);
        true
    }

    /// Also called during an idle producer wake so a tail gap can be emitted
    /// when capacity reappears without waiting for another application event.
    /// While paused, pending entries wait for the resume rows.
    pub(crate) fn offer_gap(&mut self, admission: SessionTimeUs) -> Result<bool, JournalError> {
        if self.paused.is_some() { return Ok(true); }
        self.emit_pending(admission, Capacity::Normal)
    }

    /// The controller's pause holds from here: pending and later losses wait
    /// for the resume rows, and health conditions are counted instead of written.
    pub(crate) fn pause(&mut self) {
        self.paused.get_or_insert_with(BTreeMap::new);
    }

    /// Take the health counts of the current pause for its resume row.
    /// The stream stays paused until `resume`.
    pub(crate) fn take_paused_health(&mut self) -> BTreeMap<HealthKind, u64> {
        let counts = self.paused.as_mut().map(std::mem::take).unwrap_or_default();
        for (kind, count) in &counts {
            add_count(&mut self.totals.paused_health, *kind, *count);
        }
        counts
    }

    /// Called after the resume rows: losses noticed during the pause follow them.
    pub(crate) fn resume(&mut self) {
        self.paused = None;
    }

    /// Close the ingress at its closing boundary. Pending entries are then
    /// admitted at the cutoff, before the stream's final health rows.
    pub(crate) fn seal(mut self, cutoff: SessionTimeUs) -> Result<(), JournalError> {
        self.check_order(cutoff)?;
        self.sealed = Some(cutoff);
        Ok(())
    }

    /// Enqueue a prefix flush without blocking. Busy means the caller can
    /// retry. Consuming the ingress always requests a final OS flush.
    pub(crate) fn try_flush(
        &mut self, kind: Flush, admission: SessionTimeUs,
    ) -> Result<FlushRequest, JournalError> {
        if !self.offer_gap(admission)? {
            return Ok(FlushRequest::Busy);
        }
        let (done, outcome) = mpsc::sync_channel(1);
        Ok(if self.queue(Entry::Flush(kind, done), Capacity::Normal)? {
            FlushRequest::Queued(outcome)
        } else {
            FlushRequest::Busy
        })
    }

    /// Only a non-capture worker can wait for this prefix.
    pub(crate) fn flush_wait(&mut self, kind: Flush, admission: SessionTimeUs) -> Result<(), JournalError> {
        let outcome = match self.try_flush(kind, admission)? {
            FlushRequest::Queued(outcome) => outcome,
            FlushRequest::Busy => {
                if self.paused.is_none() {
                    self.emit_pending(admission, Capacity::Blocking)?;
                }
                let (done, outcome) = mpsc::sync_channel(1);
                self.queue(Entry::Flush(kind, done), Capacity::Blocking)?;
                outcome
            }
        };
        outcome.recv().map_err(|_| self.error(kind.step(), None))?
    }

    /// A stream whose rows are all retained by blocking submission. The
    /// returned endpoint has no loss path (§6.1 rule 6).
    pub(crate) fn into_verified(self) -> VerifiedIngress {
        VerifiedIngress(self)
    }

    /// Admissions are nondecreasing.
    fn check_order(&self, at: SessionTimeUs) -> Result<(), JournalError> {
        if self.last_admission.is_some_and(|last| at < last) {
            return Err(self.error(JournalStep::Order, None));
        }
        Ok(())
    }

    /// Queue a lifecycle entry in the room reserved above the data limit.
    fn send_reserved(&self, entry: Entry) -> Result<(), JournalError> {
        if !self.queue(entry, Capacity::Reserved)? {
            return Err(self.error(JournalStep::Reserve, None));
        }
        Ok(())
    }

    fn error(&self, step: JournalStep, source: Option<io::Error>) -> JournalError {
        JournalError { stream: self.stream, step, source }
    }
}

impl Drop for JournalIngress {
    fn drop(&mut self) {
        // Reserved, single-use slot. The receiver observes channel
        // disconnection only after all accepted entries and this final tail.
        let admission = match (self.sealed, self.last_admission) {
            (Some(sealed), Some(last)) => Some(sealed.max(last)),
            (sealed, last) => sealed.or(last),
        };
        let unreported_paused_health = self.take_paused_health();
        let tail = IngressTail {
            admission, pending: self.ledger.values().copied().collect(), unreported_paused_health,
            totals: std::mem::take(&mut self.totals),
        };
        let _ = self.tail.try_send(tail);
    }
}

/// The frames stream ingress, owned by the frames merge thread. A row that
/// does not fit stays with the caller, which retries it, so the stream never
/// records a loss and never emits a gap row.
pub(crate) struct VerifiedIngress(JournalIngress);

impl VerifiedIngress {
    /// Queue one row without waiting for the writer. `false` means the entry
    /// queue or the byte budget is full; the caller retries the same row.
    pub(crate) fn try_submit_verified(&mut self, receipt: SessionTimeUs, value: Value) -> Result<bool, JournalError> {
        let ingress = &mut self.0;
        let Some(record) = ingress.prepare(receipt, value)? else { return Ok(false) };
        // A refused row releases its byte reservation when dropped.
        let queued = ingress.queue(Entry::Data(record, None), Capacity::Unreserved)?;
        if queued {
            ingress.totals.offered = ingress.totals.offered.saturating_add(1);
        }
        Ok(queued)
    }

    /// Request an OS flush of the written prefix without waiting for it.
    pub(crate) fn request_flush(&mut self, admission: SessionTimeUs) -> Result<bool, JournalError> {
        Ok(matches!(self.0.try_flush(Flush::Os, admission)?, FlushRequest::Queued(_)))
    }
}

struct RowBytes(Vec<u8>);

impl Write for RowBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_ROW_BYTES - self.0.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Journal row exceeds its byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Entry and byte bounds of one stream's data queue.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JournalLimits {
    rows: usize,
    bytes: usize,
}

impl JournalLimits {
    /// REL-2 queue targets.
    pub(crate) fn for_stream(stream: StreamId) -> Self {
        let (rows, bytes) = match stream {
            StreamId::Input => (65_536, 8 * 1024 * 1024),
            StreamId::Context => (8_192, 8 * 1024 * 1024),
            StreamId::Frames => (8_192, 4 * 1024 * 1024),
            StreamId::Stills => (8_192, 2 * 1024 * 1024),
        };
        Self { rows, bytes }
    }
}

pub(crate) struct JournalOwner {
    stream: StreamId,
    final_rows: Option<SyncSender<Vec<FinalRow>>>,
    thread: JoinHandle<Result<StreamSummary, JournalError>>,
    peaks: Arc<QueuePeaks>,
}

impl JournalOwner {
    pub(crate) fn spawn(
        stream: StreamId, path: PathBuf, limits: JournalLimits, on_failure: impl FnOnce(&JournalError) + Send + 'static,
    ) -> Result<(JournalIngress, Self), JournalError> {
        let JournalLimits { rows: row_limit, bytes: byte_limit } = limits;
        // Frames rows wait in their per-monitor lanes instead of using a reserve.
        let reserve = if stream == StreamId::Frames { 0 } else { BOUNDARY_ENTRIES };
        let (entries, pending) = mpsc::sync_channel(row_limit + reserve);
        let (tail, closed) = mpsc::sync_channel(1);
        let (final_rows, finals) = mpsc::sync_channel(1);
        let (final_rows, finals) = if matches!(stream, StreamId::Context | StreamId::Input) {
            (Some(final_rows), Some(finals))
        } else {
            (None, None)
        };
        let thread = thread::Builder::new().name(format!("{stream:?}-journal")).spawn(move || {
            let result = write_stream(stream, path, pending, closed, finals);
            if let Err(error) = &result {
                on_failure(error);
            }
            result
        }).map_err(|source| JournalError { stream, step: JournalStep::Start, source: Some(source) })?;
        let peaks = Arc::new(QueuePeaks::default());
        let ingress = JournalIngress {
            stream, entries, normal_entries: Arc::default(), row_limit, tail, bytes_held: Arc::default(), byte_limit,
            peaks: Arc::clone(&peaks), last_admission: None, ledger: BTreeMap::new(), totals: IngressTotals::default(),
            paused: None, sealed: None,
        };
        Ok((ingress, Self { stream, final_rows, thread, peaks }))
    }

    pub(crate) fn finish(self) -> Result<StreamSummary, JournalError> {
        self.finish_with(Vec::new())
    }

    /// Close the stream after its final rows; only Context and Input take them.
    pub(crate) fn finish_with(self, rows: Vec<FinalRow>) -> Result<StreamSummary, JournalError> {
        let Self { stream, final_rows, thread, peaks } = self;
        if let Some(send) = &final_rows && !rows.is_empty() {
            // If the stream has failed, report once to the UI.
            let _ = send.try_send(rows);
        }
        drop(final_rows);
        let mut summary = thread.join().map_err(|_| JournalError { stream, step: JournalStep::Close, source: None })??;
        summary.normal_entries_peak = peaks.normal_entries.load(Ordering::Relaxed);
        summary.prepared_bytes_peak = peaks.prepared_bytes.load(Ordering::Relaxed);
        Ok(summary)
    }
}

struct JournalFile {
    stream: StreamId,
    file: BufWriter<File>,
    /// The totals other than the row counts, which `counts` keeps.
    summary: StreamSummary,
    counts: RowCounts,
}

fn write_stream(
    stream: StreamId, path: PathBuf, pending: Receiver<QueuedEntry>, tail: Receiver<IngressTail>,
    finals: Option<Receiver<Vec<FinalRow>>>,
) -> Result<StreamSummary, JournalError> {
    let file = File::create_new(path)
        .map_err(|source| JournalError { stream, step: JournalStep::Create, source: Some(source) })?;
    let mut journal = JournalFile {
        stream, file: BufWriter::new(file), summary: StreamSummary::default(), counts: RowCounts::default(),
    };
    let result = journal.drain(pending, tail, finals);
    // BufWriter's implicit drop must not attempt another write after failure.
    let _ = journal.file.into_parts();
    result
}

impl JournalFile {
    fn drain(
        &mut self, pending: Receiver<QueuedEntry>, tail: Receiver<IngressTail>,
        finals: Option<Receiver<Vec<FinalRow>>>,
    ) -> Result<StreamSummary, JournalError> {
        let mut buffered = Instant::now();
        let mut disk = buffered;
        loop {
            let now = Instant::now();
            if now.duration_since(disk) >= DISK_INTERVAL {
                self.flush(Flush::Os)?;
                disk = Instant::now();
                buffered = disk;
            } else if now.duration_since(buffered) >= BUFFER_INTERVAL {
                self.flush(Flush::Buffered)?;
                buffered = Instant::now();
            }
            let wait = BUFFER_INTERVAL.saturating_sub(buffered.elapsed());
            match pending.recv_timeout(wait).map(|queued| queued.entry) {
                Ok(Entry::Data(row, confirmation)) => {
                    self.data(row.receipt, &row.object, &row.tally)?;
                    if let Some(reply) = confirmation {
                        let _ = reply.try_send(EventRef { stream: self.stream, seq: StreamSequence(self.summary.rows) });
                    }
                }
                Ok(Entry::Boundary(receipt, rows)) => {
                    for (row, tally) in rows {
                        self.data(receipt, &row, &tally)?;
                    }
                }
                Ok(Entry::Gaps(admission, gaps)) => {
                    for gap in gaps {
                        self.gap(admission, gap)?;
                    }
                }
                Ok(Entry::Health(receipt, row)) => {
                    self.append(Some(receipt), &row)?;
                    self.counts.count_health();
                }
                Ok(Entry::Flush(kind, done)) => {
                    match self.flush(kind) {
                        Ok(()) => { let _ = done.try_send(Ok(())); }
                        Err(error) => {
                            let cause = error.source.as_ref().map(|source| match source.raw_os_error() {
                                Some(code) => io::Error::from_raw_os_error(code),
                                None => io::Error::from(source.kind()),
                            });
                            let _ = done.try_send(Err(self.error(error.step, cause)));
                            return Err(error);
                        }
                    }
                    buffered = Instant::now();
                    if matches!(kind, Flush::Os) {
                        disk = buffered;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let closed = tail.recv().map_err(|_| self.error(JournalStep::Close, None))?;
        self.close(closed)?;
        if let Some(receiver) = finals {
            self.flush(Flush::Os)?;
            let Ok(rows) = receiver.recv() else { return Ok(self.summary()) };
            for row in rows {
                self.final_row(row)?;
            }
        }
        self.flush(Flush::Os)?;
        Ok(self.summary())
    }

    /// The closed totals with the row counts.
    fn summary(&self) -> StreamSummary {
        let mut summary = self.summary.clone();
        self.counts.record_in(&mut summary);
        summary
    }

    /// Pending ledger entries precede the stream's final health rows.
    fn close(&mut self, closed: IngressTail) -> Result<(), JournalError> {
        let IngressTail { admission, mut pending, unreported_paused_health, totals } = closed;
        if !pending.is_empty() {
            let admission = admission.ok_or_else(|| self.error(JournalStep::Order, None))?;
            pending.sort_by_key(gap_order);
            for gap in pending {
                self.gap(admission, gap)?;
            }
        }
        if !unreported_paused_health.is_empty() && let Some(receipt) = admission.or(self.summary.last_receipt) {
            self.append_row(receipt, serde_json::json!({
                "kind": RowKind::health(self.stream), "health": HealthKind::PausedSummary, "counts": unreported_paused_health,
            }))?;
            self.counts.count_health();
        }
        let IngressTotals { offered, suppressed, sensitive, paused_health } = totals;
        self.summary.offered = offered;
        self.summary.suppressed_by_class = suppressed;
        self.summary.paused_health = paused_health;
        for (reason, loss) in sensitive {
            let total = self.summary.dropped_by_reason.entry(reason).or_default();
            total.sensitive_items = total.sensitive_items.saturating_add(loss.sensitive_items);
            total.sensitive_unknown = total.sensitive_unknown.saturating_add(loss.sensitive_unknown);
        }
        Ok(())
    }

    fn data(&mut self, receipt: SessionTimeUs, object: &[u8], tally: &RowTally) -> Result<(), JournalError> {
        self.append(Some(receipt), object)?;
        self.summary.data_rows += 1;
        self.summary.last_data_receipt = Some(receipt);
        self.counts.count_row(tally);
        Ok(())
    }

    fn final_row(&mut self, row: FinalRow) -> Result<(), JournalError> {
        match row {
            FinalRow::Health(record) => {
                self.append_row(record.detected, record.row(self.stream))?;
                self.counts.count_health();
            }
            FinalRow::Blind(blind) => {
                let row = BlindRow::new(blind.reason, blind.start_t_us, blind.end_t_us, &[]).value()
                    .map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
                self.append_row(blind.end_t_us, row)?;
                self.counts.count_row(&RowTally::Blind);
            }
        }
        Ok(())
    }

    fn gap(&mut self, admission: SessionTimeUs, loss: OrdinaryLoss) -> Result<(), JournalError> {
        let record = loss.record();
        let mut row = serde_json::to_value(record).map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        row["kind"] = serde_json::json!("gap");
        self.append_row(admission, row)?;
        self.counts.count_gap(&record);
        if let Some(count) = record.count {
            self.summary.dropped = self.summary.dropped.saturating_add(count);
        }
        self.summary.gap_rows += 1;
        Ok(())
    }

    /// Append a writer-made row at `at`, raised to the last appended receipt.
    fn append_row(&mut self, at: SessionTimeUs, mut row: Value) -> Result<(), JournalError> {
        let receipt = self.summary.last_receipt.map_or(at, |last| last.max(at));
        stamp_receipt(&mut row, receipt).map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        let row = serde_json::to_vec(&row).map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        self.append(Some(receipt), &row)
    }

    fn append(&mut self, receipt: Option<SessionTimeUs>, object: &[u8]) -> Result<(), JournalError> {
        if receipt.zip(self.summary.last_receipt).is_some_and(|(received, previous)| received < previous) {
            return Err(self.error(JournalStep::Order, None));
        }
        let seq = self.summary.rows.checked_add(1).ok_or_else(|| self.error(JournalStep::Append, None))?;
        write!(self.file, "{{\"seq\":{seq},").and_then(|()| self.file.write_all(&object[1..]))
            .and_then(|()| self.file.write_all(b"\n"))
            .map_err(|source| self.error(JournalStep::Append, Some(source)))?;
        self.summary.rows = seq;
        self.summary.last_receipt = receipt.or(self.summary.last_receipt);
        Ok(())
    }

    fn flush(&mut self, kind: Flush) -> Result<(), JournalError> {
        let started = Instant::now();
        self.file.flush().map_err(|source| self.error(JournalStep::BufferedFlush, Some(source)))?;
        self.summary.buffered_flushes += 1;
        if matches!(kind, Flush::Os) {
            self.file.get_ref().sync_all().map_err(|source| self.error(JournalStep::OsFlush, Some(source)))?;
            self.summary.os_flushes += 1;
            let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            self.summary.os_flush_max_us = self.summary.os_flush_max_us.max(elapsed);
        }
        Ok(())
    }

    fn error(&self, step: JournalStep, source: Option<io::Error>) -> JournalError {
        JournalError { stream: self.stream, step, source }
    }
}
