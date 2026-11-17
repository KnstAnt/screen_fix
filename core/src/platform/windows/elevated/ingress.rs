use std::collections::VecDeque;

use screen_fix_elevated::{Failure, Result};
use screen_fix_protocol::{QpcTick, ipc::{FocusAtReceipt, Producer, ProducerGap, ProducerProgress,
    elevated::{ContextEvidence, GrantedInput, InputEvidence, VisitBinding, runtime::{ContextProducer, Observation}},
}};

const CAPACITY: usize = 256;
const PRODUCERS: [Producer; 4] = [Producer::RawInput, Producer::DeviceNotifications, Producer::KeyboardHook, Producer::PointerHook];

#[derive(Clone, Copy)]
pub(in crate::platform::windows) struct InputCertificate {
    pub visit: VisitBinding,
    pub opening: QpcTick,
    pub before: QpcTick,
    pub context_before: QpcTick,
    pub producers: [ProducerProgress; 4],
}

pub(in crate::platform::windows) struct InputStagingLoss {
    pub visit: VisitBinding,
    pub before: QpcTick,
    pub gaps: [Option<ProducerGap>; 4],
}

pub(super) struct Ingress {
    visit: VisitBinding,
    focus: FocusAtReceipt,
    sample: QpcTick,
    opening: QpcTick,
    sources: [Option<QpcTick>; 4],
    source_positions: [u64; 4],
    context: Option<(QpcTick, QpcTick)>,
    context_position: [u64; 5],
    lost_notice_before: Option<QpcTick>,
    uncovered_before: Option<QpcTick>,
    pending: VecDeque<InputEvidence>,
    delivered_before: QpcTick,
    pressure: Option<InputStagingLoss>,
}

impl Ingress {
    pub(super) fn new(visit: VisitBinding, focus: FocusAtReceipt, sample: QpcTick, opening: QpcTick) -> Self {
        Self { visit, focus, sample, opening, sources: [None; 4], source_positions: [0; 4], context: None,
            context_position: [0, 1, 0, 0, 0], lost_notice_before: None, uncovered_before: None,
            pending: VecDeque::with_capacity(CAPACITY), delivered_before: sample, pressure: None }
    }

    pub(super) fn visit(&self) -> VisitBinding { self.visit }

    pub(super) fn opening(&self) -> QpcTick { self.opening }

    pub(super) fn media_open(&self) -> bool { self.uncovered_before.is_none() }

    fn restrict(&mut self, before: QpcTick, notify: &impl Fn(VisitBinding, QpcTick)) {
        self.uncovered_before = Some(self.uncovered_before.map_or(before, |old| old.min(before)));
        notify(self.visit, before);
    }

    pub(super) fn input(
        &mut self, item: GrantedInput, now: QpcTick, restrict: impl Fn(VisitBinding, QpcTick),
    ) -> Result<()> {
        if self.visit != item.visit { return Ok(()); }
        if let InputEvidence::Progress(progress) = item.evidence {
            if progress.before_qpc > now { return Err(Failure::Protocol); }
            let source = &mut self.sources[crate::capture::input::producer_index(progress.producer)];
            if source.is_some_and(|before| before > progress.before_qpc) { return Err(Failure::Protocol); }
            *source = Some(progress.before_qpc);
            return Ok(());
        }
        let at = last_receipt(&item.evidence);
        if at > now { return Err(Failure::Protocol); }
        match &item.evidence {
            InputEvidence::Input(input) => {
                if input.receipt.qpc <= self.opening { return Ok(()); }
                if input.sample.producer() == Producer::RawInput && !input.foreground_at_receipt.is_some_and(|foreground| {
                    foreground.window == self.visit.foreground.window && foreground.process == self.visit.foreground.process
                        && foreground.focus == Some(self.focus)
                }) {
                    return Err(Failure::Protocol);
                }
            }
            InputEvidence::Gap(gap) if gap.first_qpc <= self.sample => return Ok(()),
            _ if at <= self.sample => return Ok(()),
            _ => {}
        }
        let first = first_receipt(&item.evidence);
        if first < self.delivered_before || producer(&item.evidence).is_some_and(|producer| {
            self.sources[crate::capture::input::producer_index(producer)].is_some_and(|before| first < before)
        }) {
            self.restrict(first, &restrict);
            return Err(Failure::Witness);
        }
        if let InputEvidence::Gap(gap) = &item.evidence {
            self.restrict(gap.first_qpc, &restrict);
        }
        let range = input_range(&item.evidence);
        if let Some(gap) = range {
            let position = &mut self.source_positions[crate::capture::input::producer_index(gap.producer)];
            if gap.first_sequence.0 <= *position { return Err(Failure::Protocol); }
            *position = gap.last_sequence.0;
        }
        if self.pending.len() == CAPACITY || self.pressure.is_some() {
            self.restrict(first, &restrict);
            let loss = self.pressure.get_or_insert(InputStagingLoss { visit: self.visit, before: first, gaps: [None; 4] });
            loss.before = loss.before.min(first);
            if let Some(gap) = range {
                let slot = &mut loss.gaps[crate::capture::input::producer_index(gap.producer)];
                if let Some(existing) = slot {
                    existing.last_sequence = existing.last_sequence.max(gap.last_sequence);
                    existing.last_qpc = existing.last_qpc.max(gap.last_qpc);
                } else {
                    *slot = Some(gap);
                }
            }
            return Ok(());
        }
        self.pending.push_back(item.evidence);
        Ok(())
    }

    pub(super) fn take_pressure(&mut self) -> Option<InputStagingLoss> { self.pressure.take() }

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
                    self.lost_notice_before = Some(self.lost_notice_before.map_or(gap.first_qpc, |old| old.min(gap.first_qpc)));
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn position(&mut self, producer: ContextProducer, first: u64, last: u64) -> Result<()> {
        let slot = &mut self.context_position[match producer {
            ContextProducer::Notices => 0,
            ContextProducer::NativeRead => 1,
            ContextProducer::Automation => 2,
            ContextProducer::Ime => 3,
            ContextProducer::TextEdit => 4,
        }];
        if slot.checked_add(1) != Some(first) || last < first { return Err(Failure::Protocol); }
        *slot = last;
        Ok(())
    }

    pub(super) fn context_before(&self) -> Option<QpcTick> {
        self.context.map(|(before, _)| self.lost_notice_before.map_or(before, |lost| before.min(lost)))
            .filter(|before| *before > self.opening)
    }

    pub(super) fn certificate(&self, cutoff: Option<QpcTick>) -> Option<InputCertificate> {
        let bounds = |producer| self.sources[crate::capture::input::producer_index(producer)]
            .map(|before_qpc| ProducerProgress { producer, before_qpc });
        let [raw, devices, hook_key, hook_pointer] = PRODUCERS.map(bounds);
        let producers = [raw?, devices?, hook_key?, hook_pointer?];
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

    pub(super) fn take(&mut self, certificate: InputCertificate, mut accept: impl FnMut(InputEvidence)) {
        if certificate.visit != self.visit || certificate.before <= self.delivered_before { return; }
        for _ in 0..self.pending.len() {
            let Some(item) = self.pending.pop_front() else { break; };
            if last_receipt(&item) < certificate.before {
                accept(item);
            } else {
                self.pending.push_back(item);
            }
        }
        self.delivered_before = certificate.before;
    }
}

fn producer(item: &InputEvidence) -> Option<Producer> {
    match item {
        InputEvidence::Input(sample) => Some(sample.sample.producer()),
        InputEvidence::DeviceArrived { .. } | InputEvidence::DeviceRemoved { .. } => Some(Producer::DeviceNotifications),
        InputEvidence::Progress(progress) => Some(progress.producer),
        InputEvidence::Gap(gap) => Some(gap.producer),
        InputEvidence::Environment(_) => None,
    }
}

fn input_range(item: &InputEvidence) -> Option<ProducerGap> {
    let receipt = match item {
        InputEvidence::Input(sample) => sample.receipt,
        InputEvidence::DeviceArrived { receipt, .. } | InputEvidence::DeviceRemoved { receipt, .. } => *receipt,
        InputEvidence::Gap(gap) => return Some(*gap),
        _ => return None,
    };
    Some(ProducerGap { producer: producer(item)?, first_sequence: receipt.sequence, last_sequence: receipt.sequence,
        first_qpc: receipt.qpc, last_qpc: receipt.qpc })
}

fn first_receipt(item: &InputEvidence) -> QpcTick {
    match item {
        InputEvidence::Gap(gap) => gap.first_qpc,
        _ => last_receipt(item),
    }
}

fn last_receipt(item: &InputEvidence) -> QpcTick {
    match item {
        InputEvidence::Input(input) => input.receipt.qpc,
        InputEvidence::DeviceArrived { receipt, .. } | InputEvidence::DeviceRemoved { receipt, .. } => receipt.qpc,
        InputEvidence::Environment(state) => state.observed_qpc,
        InputEvidence::Progress(progress) => progress.before_qpc,
        InputEvidence::Gap(gap) => gap.last_qpc,
    }
}
