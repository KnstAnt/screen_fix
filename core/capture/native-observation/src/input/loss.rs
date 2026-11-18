use std::collections::VecDeque;

use screen_fix_protocol::ipc::{EnvironmentState, GapCause, HelperToCore, Producer, ProducerGap, Receipt};

/// Pending gaps of one producer kept apart by cause before they merge.
const MAX_PENDING: usize = 8;

/// Undelivered sequence ranges per producer, in sequence order. While a gap
/// is pending, every later record of that producer is lost too, so the
/// ranges stay contiguous. Adjacent ranges of the same cause merge; at the
/// bound, a different cause extends the last range instead.
#[derive(Default)]
pub struct Losses([VecDeque<ProducerGap>; Producer::ALL.len()]);

impl Losses {
    /// The earliest pending gap of the producer, which is delivered first.
    fn front(&self, producer: Producer) -> Option<ProducerGap> {
        self.0[producer.index()].front().copied()
    }

    /// Remove the front gap after it was delivered.
    fn delivered(&mut self, producer: Producer) {
        self.0[producer.index()].pop_front();
    }

    /// Deliver the producer's pending gaps in order through `send`, which
    /// reports whether the queue accepted one. `false` when a gap is still
    /// pending: a record of the producer must not overtake it.
    pub fn flush<E>(
        &mut self, producer: Producer, mut send: impl FnMut(ProducerGap) -> Result<bool, E>,
    ) -> Result<bool, E> {
        while let Some(gap) = self.front(producer) {
            if !send(gap)? { return Ok(false); }
            self.delivered(producer);
        }
        Ok(true)
    }

    pub fn drop_record(&mut self, record: &HelperToCore) {
        self.lose(record, GapCause::Overflow);
    }

    /// A record that could not be encoded is skipped, not fatal.
    pub fn malformed_record(&mut self, record: &HelperToCore) {
        self.lose(record, GapCause::Malformed);
    }

    /// A native packet rejected before it became a record still consumed its
    /// producer sequence number.
    pub fn malformed(&mut self, producer: Producer, receipt: Receipt) {
        self.absorb(gap(producer, GapCause::Malformed, receipt));
    }

    fn lose(&mut self, record: &HelperToCore, cause: GapCause) {
        if let HelperToCore::Gap(gap) = record {
            self.absorb(*gap);
        } else if let Some((producer, receipt)) = receipt(record) {
            self.absorb(gap(producer, cause, receipt));
        }
    }

    pub fn finish(self) -> impl Iterator<Item = ProducerGap> {
        self.0.into_iter().flatten()
    }

    fn absorb(&mut self, next: ProducerGap) {
        let pending = &mut self.0[next.producer.index()];
        let full = pending.len() == MAX_PENDING;
        match pending.back_mut() {
            Some(previous) if previous.cause == next.cause || full => {
                previous.last_sequence = next.last_sequence;
                previous.last_qpc = next.last_qpc;
            }
            _ => pending.push_back(next),
        }
    }
}

/// The producer and receipt of a record that consumed one producer sequence number.
fn receipt(record: &HelperToCore) -> Option<(Producer, Receipt)> {
    match record {
        HelperToCore::Input(input) => Some((input.sample.producer(), input.receipt)),
        HelperToCore::DeviceArrived { receipt, .. } | HelperToCore::DeviceRemoved { receipt, .. } =>
            Some((Producer::DeviceNotifications, *receipt)),
        _ => None,
    }
}

fn gap(producer: Producer, cause: GapCause, receipt: Receipt) -> ProducerGap {
    ProducerGap {
        producer, cause,
        first_sequence: receipt.sequence, last_sequence: receipt.sequence,
        first_qpc: receipt.qpc, last_qpc: receipt.qpc,
    }
}

/// Status records waiting for queue room: the latest environment state, which
/// accumulates the changes of the states it replaced, and the
/// optional-collection notices in arrival order. `defer` keeps every notice;
/// the producer reports each lost collection once.
#[derive(Default)]
pub struct PendingStatus {
    environment: Option<EnvironmentState>,
    notices: VecDeque<HelperToCore>,
}

impl PendingStatus {
    /// Queue a status record behind the pending ones. A record that is not a
    /// status record is returned.
    pub fn defer(&mut self, record: HelperToCore) -> Result<(), HelperToCore> {
        match record {
            HelperToCore::Environment(mut state) => {
                if let Some(pending) = self.environment.take() {
                    state.changes |= pending.changes;
                }
                self.environment = Some(state);
            }
            HelperToCore::OptionalUnavailable { .. } => self.notices.push_back(record),
            record => return Err(record),
        }
        Ok(())
    }

    /// Drop a pending environment state that no longer applies.
    pub fn discard_environment(&mut self) {
        self.environment = None;
    }

    /// Deliver the pending records through `send`, which reports whether the
    /// queue accepted one: the environment state first, then the notices in
    /// arrival order. The merged state has no single arrival position, so it
    /// may precede notices deferred before it. `false` when a record is still
    /// pending.
    pub fn flush<E>(&mut self, mut send: impl FnMut(HelperToCore) -> Result<bool, E>) -> Result<bool, E> {
        if let Some(state) = self.environment {
            if !send(HelperToCore::Environment(state))? { return Ok(false); }
            self.environment = None;
        }
        while let Some(notice) = self.notices.front() {
            if !send(notice.clone())? { return Ok(false); }
            self.notices.pop_front();
        }
        Ok(true)
    }

    /// The records still pending when their producer stopped.
    pub fn finish(self) -> impl Iterator<Item = HelperToCore> {
        self.environment.map(HelperToCore::Environment).into_iter().chain(self.notices)
    }
}

/// How the collector's queue took one record.
pub enum Sent {
    Accepted,
    /// The queue has no room; the record comes back.
    Full(HelperToCore),
    /// The record cannot be encoded and is skipped; it comes back.
    Malformed(HelperToCore),
}

/// What one collector still owes its queue: pending gaps per producer and
/// pending status records. Each collector supplies only its `send`.
#[derive(Default)]
pub struct Backlog {
    losses: Losses,
    status: PendingStatus,
}

impl Backlog {
    /// Send `record`, or account for it. A status record waits behind the
    /// pending status records. Any other record follows its producer's pending
    /// gaps and never overtakes one (E3): while a gap stays pending, and when
    /// the queue refuses the record, the record joins a gap.
    pub fn offer<E>(&mut self, record: HelperToCore, mut send: impl FnMut(HelperToCore) -> Result<Sent, E>) -> Result<(), E> {
        // `producer` is `None` for status records, so deferring them first
        // keeps the order of a collector that flushes gaps first.
        let record = match self.status.defer(record) {
            Ok(()) => return self.flush_status(send).map(drop),
            Err(record) => record,
        };
        if let Some(producer) = producer(&record)
            && !self.flush_losses(producer, &mut send)?
        {
            self.losses.drop_record(&record);
            return Ok(());
        }
        match send(record)? {
            Sent::Accepted => {}
            Sent::Full(record) => self.losses.drop_record(&record),
            Sent::Malformed(record) => self.losses.malformed_record(&record),
        }
        Ok(())
    }

    /// Account for a record that was never offered.
    pub fn drop_record(&mut self, record: &HelperToCore) {
        self.losses.drop_record(record);
    }

    /// See [`Losses::malformed`].
    pub fn malformed(&mut self, producer: Producer, receipt: Receipt) {
        self.losses.malformed(producer, receipt);
    }

    /// Deliver the producer's pending gaps in order. `false` when a gap is
    /// still pending. A gap reported `Malformed` stays pending, as on `Full`.
    pub fn flush_losses<E>(
        &mut self, producer: Producer, mut send: impl FnMut(HelperToCore) -> Result<Sent, E>,
    ) -> Result<bool, E> {
        self.losses.flush(producer, |gap| Ok(matches!(send(HelperToCore::Gap(gap))?, Sent::Accepted)))
    }

    /// Deliver the pending status records (see [`PendingStatus::flush`]). A
    /// record reported `Malformed` is skipped. `false` when one is still pending.
    pub fn flush_status<E>(&mut self, mut send: impl FnMut(HelperToCore) -> Result<Sent, E>) -> Result<bool, E> {
        self.status.flush(|record| Ok(!matches!(send(record)?, Sent::Full(_))))
    }

    /// Deliver the pending gaps of `producers` in turn, then the pending
    /// status records. Stops at the first record still pending: `false`.
    pub fn flush<E>(
        &mut self, producers: impl IntoIterator<Item = Producer>, mut send: impl FnMut(HelperToCore) -> Result<Sent, E>,
    ) -> Result<bool, E> {
        for producer in producers {
            if !self.flush_losses(producer, &mut send)? { return Ok(false); }
        }
        self.flush_status(send)
    }

    /// Drop a pending environment state that no longer applies.
    pub fn discard_environment(&mut self) {
        self.status.discard_environment();
    }

    /// Drop every pending status record; pending gaps stay (E3).
    pub fn discard_status(&mut self) {
        self.status = PendingStatus::default();
    }

    /// What is still pending when the producers stopped: the gaps, then the
    /// status records.
    pub fn finish(self) -> impl Iterator<Item = HelperToCore> {
        self.losses.finish().map(HelperToCore::Gap).chain(self.status.finish())
    }
}

pub fn producer(record: &HelperToCore) -> Option<Producer> {
    match record {
        HelperToCore::Gap(gap) => Some(gap.producer),
        HelperToCore::Progress(progress) => Some(progress.producer),
        _ => receipt(record).map(|(producer, _)| producer),
    }
}
