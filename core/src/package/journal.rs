//! A bounded ingress and one append-only file owner for each logical stream.

use std::{
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

use super::diagnostics::Failure;

const BUFFER_INTERVAL: Duration = Duration::from_secs(1);
const DISK_INTERVAL: Duration = Duration::from_secs(10);
pub(super) const MAX_ROW_BYTES: usize = 1024 * 1024;
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
            JournalStep::BufferedFlush => "flush the buffer of",
            JournalStep::OsFlush => "flush to disk for",
            JournalStep::Close => "close",
        };
        write!(f, "Could not {action} the {:?} journal.", self.stream)
    }
}

impl std::error::Error for JournalError {}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct StreamSummary {
    pub rows: u64,
    pub data_rows: u64,
    pub dropped: u64,
    #[serde(default)]
    pub gap_rows: u64,
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

pub(crate) type FlushOutcome = Receiver<Result<StreamSummary, JournalError>>;

pub(crate) enum FlushRequest {
    Queued(FlushOutcome),
    Busy,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FinalHealth {
    pub(crate) at: Option<SessionTimeUs>,
    pub(crate) failure: Failure,
    pub(crate) monitor: Option<MonitorId>,
    pub(crate) blind: Option<super::metadata::InputBlind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Submit {
    Queued,
    Dropped,
}

#[derive(Clone, Copy, Debug)]
struct Loss {
    count: u64,
    first: SessionTimeUs,
    last: SessionTimeUs,
    admitted: SessionTimeUs,
}

struct QueuedRow {
    receipt: SessionTimeUs,
    object: Box<[u8]>,
    bytes_held: Arc<AtomicUsize>,
}

impl Drop for QueuedRow {
    fn drop(&mut self) {
        self.bytes_held.fetch_sub(self.object.len(), Ordering::Relaxed);
    }
}

enum Entry {
    Data(QueuedRow, Option<SyncSender<EventRef>>),
    Boundary(SessionTimeUs, Vec<Box<[u8]>>),
    Gap(Loss),
    Flush(Flush, SyncSender<Result<StreamSummary, JournalError>>),
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

/// Only the logical stream's receipt-order stage owns this endpoint.
/// It does not expose Clone or any file access.
pub(crate) struct JournalIngress {
    stream: StreamId,
    entries: SyncSender<QueuedEntry>,
    normal_entries: Arc<AtomicUsize>,
    row_limit: usize,
    final_loss: SyncSender<Option<Loss>>,
    bytes_held: Arc<AtomicUsize>,
    byte_limit: usize,
    peaks: Arc<QueuePeaks>,
    last: Option<SessionTimeUs>,
    loss: Option<Loss>,
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
        let row = self.prepare(receipt, value)?;
        if self.offer_gap(receipt)? && let Some(row) = row {
            match self.offer_entry(Entry::Data(row, confirmation)) {
                Ok(()) => return Ok(Submit::Queued),
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return Err(self.error(JournalStep::Append, None)),
            }
        }
        self.record_loss(1, receipt, receipt)?;
        Ok(Submit::Dropped)
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
    pub(crate) fn submit_boundary(
        &mut self, receipt: SessionTimeUs, rows: impl IntoIterator<Item = Value>,
    ) -> Result<(), JournalError> {
        let mut encoded = Vec::new();
        let mut bytes = 0;
        for value in rows {
            let row = self.encode(receipt, value)?;
            if encoded.len() == MAX_BOUNDARY_ROWS || row.len() > MAX_BOUNDARY_BYTES - bytes {
                return Err(self.error(JournalStep::Encode, None));
            }
            bytes += row.len();
            encoded.push(row);
        }
        if encoded.is_empty() { return Ok(()); }
        if let Some(mut loss) = self.loss {
            loss.admitted = receipt;
            self.entries.try_send(QueuedEntry::priority(Entry::Gap(loss)))
                .map_err(|_| self.error(JournalStep::Append, None))?;
            self.loss = None;
        }
        self.entries.try_send(QueuedEntry::priority(Entry::Boundary(receipt, encoded)))
            .map_err(|_| self.error(JournalStep::Append, None))
    }

    /// May wait for IO consumption. Only an offline verifier that must retain
    /// every accepted row should use this endpoint.
    pub(crate) fn submit_verified(&mut self, receipt: SessionTimeUs, value: Value) -> Result<(), JournalError> {
        let object = self.encode(receipt, value)?;
        if let Some(mut loss) = self.loss.take() {
            loss.admitted = receipt;
            self.entries.send(QueuedEntry::priority(Entry::Gap(loss))).map_err(|_| self.error(JournalStep::Append, None))?;
        }
        // A full byte budget can only be held by the consumer or by queued
        // rows. Waiting for a prefix flush frees them without busy polling.
        if !self.reserve(object.len()) {
            self.flush_wait(Flush::Buffered, receipt)?;
            if !self.reserve(object.len()) {
                return Err(self.error(JournalStep::Append, None));
            }
        }
        let bytes_held = Arc::clone(&self.bytes_held);
        let record = QueuedRow { receipt, object, bytes_held };
        self.entries.send(QueuedEntry::priority(Entry::Data(record, None))).map_err(|_| self.error(JournalStep::Append, None))
    }

    fn prepare(&mut self, receipt: SessionTimeUs, value: Value) -> Result<Option<QueuedRow>, JournalError> {
        let object = self.encode(receipt, value)?;
        if !self.reserve(object.len()) {
            return Ok(None);
        }
        Ok(Some(QueuedRow { receipt, object, bytes_held: Arc::clone(&self.bytes_held) }))
    }

    fn encode(&mut self, receipt: SessionTimeUs, mut value: Value) -> Result<Box<[u8]>, JournalError> {
        if self.last.is_some_and(|previous| receipt < previous) {
            return Err(self.error(JournalStep::Order, None));
        }
        let fields = value.as_object_mut().ok_or_else(|| self.error(JournalStep::Encode, None))?;
        fields.remove("seq");
        fields.insert("recv_t_us".into(), serde_json::json!(receipt));
        fields.entry("source_t_us").or_insert(Value::Null);
        let mut row = RowBytes(Vec::new());
        serde_json::to_writer(&mut row, &value)
            .map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        self.last = Some(receipt);
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
    pub(crate) fn offer_gap(&mut self, admission: SessionTimeUs) -> Result<bool, JournalError> {
        let Some(mut loss) = self.loss else { return Ok(true); };
        if self.last.is_some_and(|last| admission < last) {
            return Err(self.error(JournalStep::Order, None));
        }
        loss.admitted = admission;
        match self.offer_entry(Entry::Gap(loss)) {
            Ok(()) => {
                self.loss = None;
                self.last = Some(admission);
                Ok(true)
            }
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(self.error(JournalStep::Append, None)),
        }
    }

    fn record_loss(
        &mut self, count: u64, first: SessionTimeUs, last: SessionTimeUs,
    ) -> Result<(), JournalError> {
        if self.last.is_some_and(|previous| first < previous) {
            return Err(self.error(JournalStep::Order, None));
        }
        self.record_loss_at(count, first, last, last)
    }

    /// Delayed loss evidence retains its original range while the gap joins
    /// the stream at a nondecreasing admission time.
    pub(crate) fn record_loss_at(
        &mut self, count: u64, first: SessionTimeUs, last: SessionTimeUs, admitted: SessionTimeUs,
    ) -> Result<(), JournalError> {
        if count == 0 || first > last || last > admitted
            || self.last.is_some_and(|previous| admitted < previous)
        {
            return Err(self.error(JournalStep::Order, None));
        }
        let loss = self.loss.get_or_insert(Loss { count: 0, first, last, admitted });
        loss.count = loss.count.saturating_add(count);
        loss.first = loss.first.min(first);
        loss.last = loss.last.max(last);
        loss.admitted = admitted;
        self.last = Some(admitted);
        Ok(())
    }

    pub(crate) fn seal(mut self, cutoff: SessionTimeUs) -> Result<(), JournalError> {
        if self.last.is_some_and(|last| cutoff < last) {
            return Err(self.error(JournalStep::Order, None));
        }
        if let Some(loss) = &mut self.loss {
            loss.admitted = cutoff;
        }
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
        match self.offer_entry(Entry::Flush(kind, done)) {
            Ok(()) => Ok(FlushRequest::Queued(outcome)),
            Err(TrySendError::Full(_)) => Ok(FlushRequest::Busy),
            Err(TrySendError::Disconnected(_)) => Err(self.error(kind.step(), None)),
        }
    }

    /// Only a non-capture worker can wait for this prefix.
    pub(crate) fn flush_wait(&mut self, kind: Flush, admission: SessionTimeUs) -> Result<StreamSummary, JournalError> {
        let outcome = match self.try_flush(kind, admission)? {
            FlushRequest::Queued(outcome) => outcome,
            FlushRequest::Busy => {
                if let Some(mut loss) = self.loss.take() {
                    loss.admitted = admission;
                    self.entries.send(QueuedEntry::priority(Entry::Gap(loss))).map_err(|_| self.error(JournalStep::Append, None))?;
                    self.last = Some(admission);
                }
                let (done, outcome) = mpsc::sync_channel(1);
                self.entries.send(QueuedEntry::priority(Entry::Flush(kind, done)))
                    .map_err(|_| self.error(kind.step(), None))?;
                outcome
            }
        };
        outcome.recv().map_err(|_| self.error(kind.step(), None))?
    }

    fn error(&self, step: JournalStep, source: Option<io::Error>) -> JournalError {
        JournalError { stream: self.stream, step, source }
    }
}

impl Drop for JournalIngress {
    fn drop(&mut self) {
        // Reserved, single-use slot. The receiver observes channel
        // disconnection only after all accepted entries and this final tail.
        let _ = self.final_loss.try_send(self.loss);
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

pub(crate) struct JournalOwner {
    stream: StreamId,
    final_health: Option<SyncSender<Vec<FinalHealth>>>,
    thread: JoinHandle<Result<StreamSummary, JournalError>>,
    peaks: Arc<QueuePeaks>,
}

impl JournalOwner {
    pub(crate) fn spawn(
        stream: StreamId, path: PathBuf, on_failure: impl FnOnce() + Send + 'static,
    ) -> Result<(JournalIngress, Self), JournalError> {
        let (row_limit, byte_limit) = match stream {
            StreamId::Input => (65_536, 8 * 1024 * 1024),
            StreamId::Context => (8_192, 8 * 1024 * 1024),
            StreamId::Frames => (8_192, 4 * 1024 * 1024),
            StreamId::Stills => (8_192, 2 * 1024 * 1024),
        };
        let reserve = if matches!(stream, StreamId::Input | StreamId::Context) { BOUNDARY_ENTRIES } else { 0 };
        let (entries, pending) = mpsc::sync_channel(row_limit + reserve);
        let (final_loss, tail) = mpsc::sync_channel(1);
        let (final_health, health) = mpsc::sync_channel(1);
        let (final_health, health) = if stream == StreamId::Context {
            (Some(final_health), Some(health))
        } else {
            (None, None)
        };
        let thread = thread::Builder::new().name(format!("{stream:?}-journal")).spawn(move || {
            let result = write_stream(stream, path, pending, tail, health);
            if result.is_err() {
                on_failure();
            }
            result
        }).map_err(|source| JournalError { stream, step: JournalStep::Start, source: Some(source) })?;
        let peaks = Arc::new(QueuePeaks::default());
        let ingress = JournalIngress {
            stream, entries, normal_entries: Arc::default(), row_limit, final_loss, bytes_held: Arc::default(), byte_limit,
            peaks: Arc::clone(&peaks), last: None, loss: None,
        };
        Ok((ingress, Self { stream, final_health, thread, peaks }))
    }

    pub(crate) fn finish(self) -> Result<StreamSummary, JournalError> {
        self.finish_with_health(Vec::new())
    }

    pub(crate) fn finish_with_health(self, health: Vec<FinalHealth>) -> Result<StreamSummary, JournalError> {
        let Self { stream, final_health, thread, peaks } = self;
        if let Some(send) = &final_health && !health.is_empty() {
            // If Context has failed, report once to the UI.
            let _ = send.try_send(health);
        }
        drop(final_health);
        let mut summary = thread.join().map_err(|_| JournalError { stream, step: JournalStep::Close, source: None })??;
        summary.normal_entries_peak = peaks.normal_entries.load(Ordering::Relaxed);
        summary.prepared_bytes_peak = peaks.prepared_bytes.load(Ordering::Relaxed);
        Ok(summary)
    }
}

struct JournalFile {
    stream: StreamId,
    file: BufWriter<File>,
    summary: StreamSummary,
}

fn write_stream(
    stream: StreamId, path: PathBuf, pending: Receiver<QueuedEntry>, tail: Receiver<Option<Loss>>,
    health: Option<Receiver<Vec<FinalHealth>>>,
) -> Result<StreamSummary, JournalError> {
    let file = File::create_new(path)
        .map_err(|source| JournalError { stream, step: JournalStep::Create, source: Some(source) })?;
    let mut journal = JournalFile { stream, file: BufWriter::new(file), summary: StreamSummary::default() };
    let result = journal.drain(pending, tail, health);
    // BufWriter's implicit drop must not attempt another write after failure.
    let _ = journal.file.into_parts();
    result
}

impl JournalFile {
    fn drain(
        &mut self, pending: Receiver<QueuedEntry>, tail: Receiver<Option<Loss>>,
        health: Option<Receiver<Vec<FinalHealth>>>,
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
                    self.append(Some(row.receipt), &row.object)?;
                    self.summary.data_rows += 1;
                    self.summary.last_data_receipt = Some(row.receipt);
                    if let Some(reply) = confirmation {
                        let _ = reply.try_send(EventRef { stream: self.stream, seq: StreamSequence(self.summary.rows) });
                    }
                }
                Ok(Entry::Boundary(receipt, rows)) => {
                    for row in rows {
                        self.append(Some(receipt), &row)?;
                        self.summary.data_rows += 1;
                        self.summary.last_data_receipt = Some(receipt);
                    }
                }
                Ok(Entry::Gap(loss)) => self.gap(loss)?,
                Ok(Entry::Flush(kind, done)) => {
                    match self.flush(kind) {
                        Ok(summary) => { let _ = done.try_send(Ok(summary)); }
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
        if let Some(loss) = tail.recv().map_err(|_| self.error(JournalStep::Close, None))? {
            self.gap(loss)?;
        }
        if let Some(receiver) = health {
            let summary = self.flush(Flush::Os)?;
            if let Ok(health) = receiver.recv() {
                for entry in health {
                    self.health(entry)?;
                }
                return self.flush(Flush::Os);
            }
            return Ok(summary);
        }
        self.flush(Flush::Os)
    }

    fn health(&mut self, health: FinalHealth) -> Result<(), JournalError> {
        let at = match (health.at, self.summary.last_receipt) {
            (Some(at), Some(previous)) => Some(at.max(previous)),
            (at, _) => at,
        };
        let row = serde_json::to_vec(&serde_json::json!({
            "kind": "health",
            "recv_t_us": at,
            "source_t_us": null,
            "failure": health.failure,
            "monitor": health.monitor,
            "detected_t_us": health.at,
        })).map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        self.append(at, &row)?;
        if let Some(blind) = health.blind {
            let receipt = Some(at.map_or(blind.end_t_us, |previous| previous.max(blind.end_t_us)));
            let row = serde_json::to_vec(&serde_json::json!({
                "kind": "blind", "recv_t_us": receipt, "source_t_us": null,
                "start_t_us": blind.start_t_us, "end_t_us": blind.end_t_us, "reason": blind.reason,
            })).map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
            self.append(receipt, &row)?;
        }
        Ok(())
    }

    fn gap(&mut self, loss: Loss) -> Result<(), JournalError> {
        let row = serde_json::to_vec(&serde_json::json!({
            "kind": "gap",
            "recv_t_us": loss.admitted,
            "source_t_us": null,
            "reason": "queue_overflow",
            "count": loss.count,
            "first_recv_t_us": loss.first,
            "last_recv_t_us": loss.last,
        })).map_err(|err| self.error(JournalStep::Encode, Some(err.into())))?;
        self.append(Some(loss.admitted), &row)?;
        self.summary.dropped = self.summary.dropped.saturating_add(loss.count);
        self.summary.gap_rows += 1;
        Ok(())
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

    fn flush(&mut self, kind: Flush) -> Result<StreamSummary, JournalError> {
        let started = Instant::now();
        self.file.flush().map_err(|source| self.error(JournalStep::BufferedFlush, Some(source)))?;
        self.summary.buffered_flushes += 1;
        if matches!(kind, Flush::Os) {
            self.file.get_ref().sync_all().map_err(|source| self.error(JournalStep::OsFlush, Some(source)))?;
            self.summary.os_flushes += 1;
            let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            self.summary.os_flush_max_us = self.summary.os_flush_max_us.max(elapsed);
        }
        Ok(self.summary)
    }

    fn error(&self, step: JournalStep, source: Option<io::Error>) -> JournalError {
        JournalError { stream: self.stream, step, source }
    }
}
