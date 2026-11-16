use screen_fix_protocol::ipc::{HelperToCore, Producer, ProducerGap};

#[derive(Default)]
pub(super) struct Losses([Option<ProducerGap>; 4]);

impl Losses {
    pub(super) fn pending(&self, producer: Producer) -> Option<ProducerGap> {
        self.0[index(producer)]
    }

    pub(super) fn clear(&mut self, producer: Producer) {
        self.0[index(producer)] = None;
    }

    pub(super) fn drop_record(&mut self, record: &HelperToCore) {
        let lost = match record {
            HelperToCore::Input(input) => (input.sample.producer(), input.receipt),
            HelperToCore::DeviceArrived { receipt, .. } | HelperToCore::DeviceRemoved { receipt, .. } =>
                (Producer::DeviceNotifications, *receipt),
            HelperToCore::Gap(gap) => {
                self.absorb(*gap);
                return;
            }
            _ => return,
        };
        let (producer, receipt) = lost;
        self.absorb(ProducerGap {
            producer,
            first_sequence: receipt.sequence,
            last_sequence: receipt.sequence,
            first_qpc: receipt.qpc,
            last_qpc: receipt.qpc,
        });
    }

    pub(super) fn finish(self) -> impl Iterator<Item = ProducerGap> {
        self.0.into_iter().flatten()
    }

    fn absorb(&mut self, next: ProducerGap) {
        let range = &mut self.0[index(next.producer)];
        if let Some(previous) = range {
            previous.last_sequence = next.last_sequence;
            previous.last_qpc = next.last_qpc;
        } else {
            *range = Some(next);
        }
    }
}

pub(super) fn producer(record: &HelperToCore) -> Option<Producer> {
    match record {
        HelperToCore::Input(input) => Some(input.sample.producer()),
        HelperToCore::DeviceArrived { .. } | HelperToCore::DeviceRemoved { .. } =>
            Some(Producer::DeviceNotifications),
        HelperToCore::Gap(gap) => Some(gap.producer),
        HelperToCore::Progress(progress) => Some(progress.producer),
        _ => None,
    }
}

fn index(producer: Producer) -> usize {
    match producer {
        Producer::RawInput => 0,
        Producer::DeviceNotifications => 1,
        Producer::KeyboardHook => 2,
        Producer::PointerHook => 3,
    }
}
