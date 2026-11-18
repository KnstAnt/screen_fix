//! Loss evidence of lost input records, split by privacy class (§6.1 rule 5).

use std::num::NonZeroU64;

use crate::{
    package::outcome::{LossEvidence, LossProducer, LossReason},
    protocol::{SessionTimeUs, ipc::{InputSample, Producer}},
    util::widen,
};

use super::{HeldInput, InputGap};

/// A keyboard record, raw or hook.
pub(crate) fn is_key(sample: &InputSample) -> bool {
    matches!(sample, InputSample::RawKey(_) | InputSample::HookKey(_))
}

/// The loss evidence of one record of `producer` by its privacy class (§6.1
/// rule 5): a key record is count-only, since its visit may be sensitive;
/// any other keeps its receipt range from `first` to `at`.
pub(crate) fn record_loss(
    reason: LossReason, producer: Producer, key: bool, first: SessionTimeUs, at: SessionTimeUs,
) -> LossEvidence {
    let producer = Some(LossProducer::from(producer));
    if key {
        LossEvidence::sensitive(reason, producer, NonZeroU64::MIN)
    } else {
        LossEvidence::ordinary(reason, producer, NonZeroU64::MIN, first, at)
    }
}

/// The lost records of one producer split by privacy class (§6.1 rule 5):
/// key records keep only their count, since their visit may be sensitive;
/// the others keep their merged receipt range.
#[derive(Clone, Copy, Default)]
pub(crate) struct ProducerLoss {
    keys: u64,
    others: Option<InputGap>,
}

impl ProducerLoss {
    /// One lost record, as its logical gap.
    pub(crate) fn record(&mut self, gap: InputGap, key: bool) {
        if key {
            self.keys = self.keys.saturating_add(1);
        } else {
            self.others = Some(self.others.map_or(gap, |old| old.merge(gap)));
        }
    }

    /// The loss evidence of these records: the key count first, then the
    /// range of the others.
    pub(crate) fn evidence(self, reason: LossReason, producer: Producer) -> impl Iterator<Item = LossEvidence> {
        let keys = NonZeroU64::new(self.keys)
            .map(|count| LossEvidence::sensitive(reason, Some(LossProducer::from(producer)), count));
        keys.into_iter().chain(self.others.map(|gap| gap.evidence()))
    }
}

#[derive(Default)]
pub(crate) struct SampleLoss {
    pub(crate) interval: Option<(SessionTimeUs, SessionTimeUs)>,
    /// Every lost record of each producer, for the held invalidation.
    pub(crate) by_producer: [Option<InputGap>; Producer::ALL.len()],
    /// The same records split by privacy class, for their loss evidence.
    evidence: [ProducerLoss; Producer::ALL.len()],
}

impl SampleLoss {
    /// One staged input lost to staging pressure. The caller retires it.
    pub(crate) fn record(&mut self, input: &HeldInput) {
        let slot = input.first();
        let at = slot.time.recv_t_us;
        let producer = slot.packet.sample.producer();
        let gap = InputGap::logical(producer, LossReason::StagingPressure, at);
        let pending = &mut self.by_producer[producer.index()];
        *pending = Some(pending.map_or(gap, |old| old.merge(gap)));
        self.evidence[producer.index()].record(gap, is_key(&slot.packet.sample));
        widen(&mut self.interval, at);
    }

    /// The loss evidence of every producer, in producer order.
    pub(crate) fn evidence(&self) -> impl Iterator<Item = LossEvidence> + '_ {
        Producer::ALL.into_iter().flat_map(|producer| {
            self.evidence[producer.index()].evidence(LossReason::StagingPressure, producer)
        })
    }
}
