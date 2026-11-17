use screen_fix_protocol::ipc::{GapCause, HelperToCore, Producer, ProducerGap, Receipt};

/// Pending gaps of one producer kept apart by cause before they merge.
const MAX_PENDING: usize = 8;

/// Undelivered sequence ranges per producer, in sequence order. While a gap
/// is pending, every later record of that producer is lost too, so the
/// ranges stay contiguous. Adjacent ranges of the same cause merge; at the
/// bound, a different cause extends the last range instead.
#[derive(Default)]
pub struct Losses([Vec<ProducerGap>; Producer::ALL.len()]);

impl Losses {
    /// The earliest pending gap of the producer, which is delivered first.
    pub fn front(&self, producer: Producer) -> Option<ProducerGap> {
        self.0[producer.index()].first().copied()
    }

    /// Remove the front gap after it was delivered.
    pub fn delivered(&mut self, producer: Producer) {
        let pending = &mut self.0[producer.index()];
        if !pending.is_empty() {
            pending.remove(0);
        }
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
        let (producer, receipt) = match record {
            HelperToCore::Input(input) => (input.sample.producer(), input.receipt),
            HelperToCore::DeviceArrived { receipt, .. } | HelperToCore::DeviceRemoved { receipt, .. } =>
                (Producer::DeviceNotifications, *receipt),
            HelperToCore::Gap(gap) => {
                self.absorb(*gap);
                return;
            }
            _ => return,
        };
        self.absorb(gap(producer, cause, receipt));
    }

    pub fn finish(self) -> impl Iterator<Item = ProducerGap> {
        self.0.into_iter().flatten()
    }

    fn absorb(&mut self, next: ProducerGap) {
        let pending = &mut self.0[next.producer.index()];
        let full = pending.len() == MAX_PENDING;
        match pending.last_mut() {
            Some(previous) if previous.cause == next.cause || full => {
                previous.last_sequence = next.last_sequence;
                previous.last_qpc = next.last_qpc;
            }
            _ => pending.push(next),
        }
    }
}

fn gap(producer: Producer, cause: GapCause, receipt: Receipt) -> ProducerGap {
    ProducerGap {
        producer, cause,
        first_sequence: receipt.sequence, last_sequence: receipt.sequence,
        first_qpc: receipt.qpc, last_qpc: receipt.qpc,
    }
}

pub fn producer(record: &HelperToCore) -> Option<Producer> {
    match record {
        HelperToCore::Input(input) => Some(input.sample.producer()),
        HelperToCore::DeviceArrived { .. } | HelperToCore::DeviceRemoved { .. } =>
            Some(Producer::DeviceNotifications),
        HelperToCore::Gap(gap) => Some(gap.producer),
        HelperToCore::Progress(progress) => Some(progress.producer),
        _ => None,
    }
}
