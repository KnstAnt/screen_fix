use std::collections::VecDeque;

use screen_fix_elevated::{Failure, Result};
use screen_fix_protocol::{QpcTick, ipc::{FocusAtReceipt, GapCause, Producer, ProducerGap, ProducerProgress,
    elevated::{ContextEvidence, GrantedInput, InputEvidence, VisitBinding, runtime::{ContextProducer, Observation}},
}};

use crate::util::keep_earlier;

const CAPACITY: usize = 256;
const PRODUCERS: usize = Producer::ALL.len();

#[derive(Clone, Copy)]
pub(in crate::platform::windows) struct InputCertificate {
    pub visit: VisitBinding,
    pub opening: QpcTick,
    pub before: QpcTick,
    pub context_before: QpcTick,
    pub producers: [ProducerProgress; PRODUCERS],
}

pub(in crate::platform::windows) struct InputStagingLoss {
    pub visit: VisitBinding,
    pub before: QpcTick,
    pub gaps: [Option<ProducerGap>; PRODUCERS],
    /// Records with valid framing whose payload the Recorder could not decode.
    pub malformed: u64,
}

/// The last context position per producer already consumed when ingress
/// opens. The visit's held-state read is NativeRead position 1; the visit
/// consumes it (`visit.rs` `observation`) before ingress exists, so ingress
/// starts that producer at 1 (`[0, 1, 0, 0, 0]`).
const OPENING_CONTEXT_POSITION: [u64; ContextProducer::ALL.len()] = {
    let mut position = [0; ContextProducer::ALL.len()];
    position[ContextProducer::NativeRead.index()] = 1;
    position
};

pub(super) struct Ingress {
    visit: VisitBinding,
    focus: FocusAtReceipt,
    sample: QpcTick,
    opening: QpcTick,
    sources: [Option<QpcTick>; PRODUCERS],
    source_positions: [u64; PRODUCERS],
    context: Option<(QpcTick, QpcTick)>,
    context_position: [u64; ContextProducer::ALL.len()],
    lost_notice_before: Option<QpcTick>,
    uncovered_before: Option<QpcTick>,
    pending: VecDeque<InputEvidence>,
    delivered_before: QpcTick,
    pressure: Option<InputStagingLoss>,
    /// Native gaps before the resync sample, carried from the previous
    /// binding. They only count (E3).
    prior: [Option<ProducerGap>; PRODUCERS],
}

impl Ingress {
    pub(super) fn new(visit: VisitBinding, focus: FocusAtReceipt, sample: QpcTick, opening: QpcTick) -> Self {
        Self { visit, focus, sample, opening, sources: [None; PRODUCERS], source_positions: [0; PRODUCERS], context: None,
            context_position: OPENING_CONTEXT_POSITION, lost_notice_before: None, uncovered_before: None,
            pending: VecDeque::with_capacity(CAPACITY), delivered_before: sample, pressure: None, prior: [None; PRODUCERS] }
    }

    pub(super) fn visit(&self) -> VisitBinding { self.visit }

    pub(super) fn opening(&self) -> QpcTick { self.opening }

    pub(super) fn media_open(&self) -> bool { self.uncovered_before.is_none() }

    fn restrict(&mut self, before: QpcTick, notify: &impl Fn(VisitBinding, QpcTick)) {
        keep_earlier(&mut self.uncovered_before, before);
        notify(self.visit, before);
    }

    /// The staging loss, started or lowered to a record from `first`.
    fn pressure_at(&mut self, first: QpcTick) -> &mut InputStagingLoss {
        let loss = self.pressure.get_or_insert(InputStagingLoss { visit: self.visit, before: first, gaps: [None; PRODUCERS], malformed: 0 });
        loss.before = loss.before.min(first);
        loss
    }

    pub(super) fn input(
        &mut self, mut item: GrantedInput, now: QpcTick, restrict: impl Fn(VisitBinding, QpcTick),
    ) -> Result<()> {
        if self.visit != item.visit { return Ok(()); }
        // A gap crossing the resync sample keeps its part after the sample (E3).
        if let InputEvidence::Gap(gap) = &mut item.evidence && gap.first_qpc <= self.sample && gap.last_qpc > self.sample {
            gap.first_qpc = QpcTick(self.sample.0 + 1);
        }
        if let InputEvidence::Progress(progress) = item.evidence {
            if progress.before_qpc > now { return Err(Failure::Protocol); }
            let source = &mut self.sources[progress.producer.index()];
            if source.is_some_and(|before| before > progress.before_qpc) { return Err(Failure::Protocol); }
            *source = Some(progress.before_qpc);
            return Ok(());
        }
        let at = item.evidence.last_qpc();
        if at > now { return Err(Failure::Protocol); }
        match &item.evidence {
            InputEvidence::Input(input) => {
                if input.receipt.qpc <= self.opening { return Ok(()); }
                if input.sample.producer().raw_input() && !input.foreground_at_receipt.is_some_and(|foreground| {
                    foreground.window == self.visit.foreground.window && foreground.process == self.visit.foreground.process
                        && foreground.focus == Some(self.focus)
                }) {
                    return Err(Failure::Protocol);
                }
            }
            InputEvidence::Gap(gap) if gap.first_qpc <= self.sample => {
                merge_gap(&mut self.prior[gap.producer.index()], *gap);
                return Ok(());
            }
            _ if at <= self.sample => return Ok(()),
            _ => {}
        }
        let first = item.evidence.first_qpc();
        if first < self.delivered_before || item.evidence.producer().is_some_and(|producer| {
            self.sources[producer.index()].is_some_and(|before| first < before)
        }) {
            self.restrict(first, &restrict);
            return Err(Failure::Witness);
        }
        if let InputEvidence::Gap(gap) = &item.evidence {
            self.restrict(gap.first_qpc, &restrict);
        }
        let range = input_range(&item.evidence);
        if let Some(gap) = range {
            let position = &mut self.source_positions[gap.producer.index()];
            if gap.first_sequence.0 <= *position { return Err(Failure::Protocol); }
            *position = gap.last_sequence.0;
        }
        if self.pending.len() == CAPACITY || self.pressure.is_some() {
            self.restrict(first, &restrict);
            let loss = self.pressure_at(first);
            if let Some(gap) = range {
                merge_gap(&mut loss.gaps[gap.producer.index()], gap);
            }
            return Ok(());
        }
        self.pending.push_back(item.evidence);
        Ok(())
    }

    /// A record with valid framing whose payload did not decode (E3, §6.2).
    /// It is counted like staging pressure; its receipt is unknown, so coverage
    /// closes at the least delivered producer progress, the earliest receipt
    /// the record could have had.
    pub(super) fn malformed(&mut self, restrict: impl Fn(VisitBinding, QpcTick)) {
        let least = self.sources.iter().map(|bound| bound.unwrap_or(self.sample)).min().unwrap_or(self.sample);
        let first = least.max(self.delivered_before);
        self.restrict(first, &restrict);
        let loss = self.pressure_at(first);
        loss.malformed = loss.malformed.saturating_add(1);
    }

    pub(super) fn take_pressure(&mut self) -> Option<InputStagingLoss> { self.pressure.take() }

    pub(super) fn take_prior(&mut self) -> impl Iterator<Item = ProducerGap> + '_ {
        self.prior.iter_mut().filter_map(Option::take)
    }

    pub(super) fn pressure_before(&self) -> Option<QpcTick> { self.pressure.as_ref().map(|loss| loss.before) }

    pub(super) fn received_through(&self, before: QpcTick) -> bool {
        before <= self.opening || (self.sources.iter().all(|bound| bound.is_some_and(|bound| bound >= before))
            && self.context.is_some_and(|(bound, _)| bound >= before))
    }

    pub(super) fn observation(
        &mut self, item: &Observation, now: QpcTick, restrict: impl Fn(VisitBinding, QpcTick),
    ) -> Result<bool> {
        match item {
            Observation::Progress { visit, before_qpc, witness_qpc } if *visit == self.visit => {
                if *before_qpc > *witness_qpc || *witness_qpc > now
                    || self.context.is_some_and(|(before, witness)| before > *before_qpc || witness > *witness_qpc)
                {
                    return Err(Failure::Witness);
                }
                self.context = Some((*before_qpc, *witness_qpc));
            }
            Observation::Item { producer, position, evidence } if evidence.source().0 == self.visit => {
                let stamp = evidence.source().1;
                if stamp > now || stamp < self.sample { return Err(Failure::Protocol); }
                if let ContextEvidence::Notice(notice) = evidence
                    && (notice.first_qpc < self.delivered_before
                        || self.context.is_some_and(|(before, _)| notice.first_qpc < before))
                {
                    self.restrict(notice.first_qpc, &restrict);
                    return Err(Failure::Witness);
                }
                self.position(*producer, position.get(), position.get())?;
            }
            Observation::Gap(gap) if gap.visit == self.visit => {
                if gap.last_qpc > now { return Err(Failure::Protocol); }
                self.position(gap.producer, gap.first_position.get(), gap.last_position.get())?;
                if gap.producer == ContextProducer::Notices {
                    self.restrict(gap.first_qpc, &restrict);
                    if gap.first_qpc < self.delivered_before { return Err(Failure::Witness); }
                    keep_earlier(&mut self.lost_notice_before, gap.first_qpc);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn position(&mut self, producer: ContextProducer, first: u64, last: u64) -> Result<()> {
        let slot = &mut self.context_position[producer.index()];
        if slot.checked_add(1) != Some(first) || last < first { return Err(Failure::Protocol); }
        *slot = last;
        Ok(())
    }

    pub(super) fn context_before(&self) -> Option<QpcTick> {
        self.context.map(|(before, _)| self.lost_notice_before.map_or(before, |lost| before.min(lost)))
            .filter(|before| *before > self.opening)
    }

    pub(super) fn certificate(&self, cutoff: Option<QpcTick>) -> Option<InputCertificate> {
        let bounds = |producer: Producer| self.sources[producer.index()]
            .map(|before_qpc| ProducerProgress { producer, before_qpc });
        let [raw, devices, hook_key, hook_pointer, optional_hid] = Producer::ALL.map(bounds);
        let producers = [raw?, devices?, hook_key?, hook_pointer?, optional_hid?];
        let context_before = self.context_before()?;
        let before = producers.iter().fold(cutoff.map_or(context_before, |cut| context_before.min(cut)),
            |before, progress| before.min(progress.before_qpc));
        // A loss interval is indivisible. The first possible missing receipt
        // closes exposure of other producers too until the scoped resync.
        let before = self.uncovered_before.map_or(before, |cutoff| before.min(cutoff));
        let before = self.pending.iter().filter_map(|item| match item {
            InputEvidence::Gap(gap) => Some(gap.first_qpc),
            _ => None,
        }).fold(before, QpcTick::min);
        (before > self.opening).then_some(InputCertificate { visit: self.visit, opening: self.opening, before, context_before, producers })
    }

    pub(super) fn can_synchronize(&self, visit: VisitBinding, completed: QpcTick) -> bool {
        self.visit == visit && completed >= self.delivered_before
    }

    /// Every record still waiting for a certificate.
    pub(super) fn drain(&mut self) -> impl Iterator<Item = InputEvidence> + '_ {
        self.pending.drain(..)
    }

    pub(super) fn take(&mut self, certificate: InputCertificate, mut accept: impl FnMut(InputEvidence)) {
        if certificate.visit != self.visit || certificate.before <= self.delivered_before { return; }
        for _ in 0..self.pending.len() {
            let Some(item) = self.pending.pop_front() else { break; };
            if item.last_qpc() < certificate.before {
                accept(item);
            } else {
                self.pending.push_back(item);
            }
        }
        self.delivered_before = certificate.before;
    }
}

fn merge_gap(slot: &mut Option<ProducerGap>, gap: ProducerGap) {
    if let Some(existing) = slot {
        existing.first_sequence = existing.first_sequence.min(gap.first_sequence);
        existing.last_sequence = existing.last_sequence.max(gap.last_sequence);
        existing.first_qpc = existing.first_qpc.min(gap.first_qpc);
        existing.last_qpc = existing.last_qpc.max(gap.last_qpc);
    } else {
        *slot = Some(gap);
    }
}

fn input_range(item: &InputEvidence) -> Option<ProducerGap> {
    let receipt = match item {
        InputEvidence::Input(sample) => sample.receipt,
        InputEvidence::DeviceArrived { receipt, .. } | InputEvidence::DeviceRemoved { receipt, .. } => *receipt,
        InputEvidence::Gap(gap) => return Some(*gap),
        _ => return None,
    };
    Some(ProducerGap { producer: item.producer()?, cause: GapCause::Overflow, first_sequence: receipt.sequence, last_sequence: receipt.sequence,
        first_qpc: receipt.qpc, last_qpc: receipt.qpc })
}
