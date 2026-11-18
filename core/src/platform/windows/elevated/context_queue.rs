use std::collections::VecDeque;

use screen_fix_native_observation::LossSpan;
use screen_fix_protocol::{QpcTick, ipc::elevated::{
    ContextEvidence, NativeObservation, ReadBinding, ReadUnavailable, VisitBinding, WindowNotice,
    runtime::{ContextGap, ContextGapCause, ContextProducer, Observation},
}};

use crate::util::keep_earlier;

use super::WORK_PER_POLL;

const CONTEXT_CAPACITY: usize = 1024;

pub(in crate::platform::windows) enum ContextDelivery {
    Observation(Observation),
    Rejected(ReadBinding, ReadUnavailable),
}

/// Collector context deliveries of the open visit, staged until the
/// collector's context progress covers them. What the staging cannot hold is
/// counted: a native notice as notice pressure, which revokes the visit, and
/// any other delivery as an optional loss.
#[derive(Default)]
pub(super) struct ContextQueue {
    items: VecDeque<(QpcTick, ContextDelivery)>,
    pressure: Option<ContextGap>,
    optional_dropped: Option<LossSpan>,
}

impl ContextQueue {
    /// Stage `delivery`, received at `arrived`. A notice the staging cannot
    /// hold restricts its visit before its first possible receipt.
    pub(super) fn push(&mut self, arrived: QpcTick, delivery: ContextDelivery, restrict: impl Fn(VisitBinding, QpcTick)) {
        if self.items.len() < CONTEXT_CAPACITY {
            self.items.push_back((arrived, delivery));
            return;
        }
        match delivery {
            ContextDelivery::Observation(Observation::Item {
                producer: ContextProducer::Notices, position, evidence: ContextEvidence::Notice(notice),
            }) => {
                restrict(notice.visit, notice.first_qpc);
                merge_context_gap(&mut self.pressure, ContextGap {
                    visit: notice.visit, producer: ContextProducer::Notices, cause: ContextGapCause::Overflow,
                    first_position: position, last_position: position,
                    first_qpc: notice.first_qpc, last_qpc: notice.observed_qpc,
                });
            }
            ContextDelivery::Observation(Observation::Gap(gap)) if gap.producer == ContextProducer::Notices =>
                merge_context_gap(&mut self.pressure, gap),
            ContextDelivery::Observation(Observation::Item { evidence, .. }) => self.drop_optional(evidence.source().1),
            ContextDelivery::Observation(Observation::Gap(gap)) => self.drop_optional(gap.first_qpc),
            ContextDelivery::Rejected(read, _) => self.drop_optional(read.requested_qpc),
            ContextDelivery::Observation(_) => {}
        }
    }

    fn drop_optional(&mut self, at: QpcTick) {
        match &mut self.optional_dropped {
            Some(span) => span.note(at),
            None => self.optional_dropped = Some(LossSpan::at(at)),
        }
    }

    /// Up to `WORK_PER_POLL` deliveries received through `at`, in receipt
    /// order. Evidence waits until the context progress (`before`) covers
    /// its source, and an optional delivery waits behind any optional one
    /// that waits. Evidence of a read past its deadline is a timed-out
    /// rejection.
    pub(super) fn ready(&mut self, before: Option<QpcTick>, at: QpcTick) -> Vec<(QpcTick, ContextDelivery)> {
        let mut ready = Vec::new();
        let mut retained = VecDeque::new();
        let mut optional_waiting = false;
        for _ in 0..self.items.len() {
            if ready.len() == WORK_PER_POLL { break; }
            let Some((arrived, item)) = self.items.pop_front() else { break; };
            let item = match &item {
                ContextDelivery::Observation(Observation::Item { evidence, .. }) => {
                    let essential = matches!(evidence, ContextEvidence::Notice(_));
                    let covered = before.is_some_and(|before| evidence.source().1 < before);
                    if arrived > at || (!essential && optional_waiting) || !covered {
                        optional_waiting |= !essential;
                        retained.push_back((arrived, item));
                        continue;
                    }
                    if let Some(read) = evidence.read_binding() && arrived >= read.deadline_qpc {
                        ContextDelivery::Rejected(read, ReadUnavailable::TimedOut)
                    } else {
                        item
                    }
                }
                _ if arrived > at || optional_waiting => {
                    retained.push_back((arrived, item));
                    continue;
                }
                _ => item,
            };
            ready.push((arrived, item));
        }
        retained.append(&mut self.items);
        self.items = retained;
        ready
    }

    /// Whether evidence of `expected` arrived before its deadline and waits here.
    pub(super) fn deferred_read(&self, expected: ReadBinding) -> bool {
        self.items.iter().any(|(arrived, item)| {
            *arrived < expected.deadline_qpc
                && matches!(item, ContextDelivery::Observation(Observation::Item { evidence, .. })
                    if evidence.read_binding() == Some(expected))
        })
    }

    fn notices(&self) -> impl Iterator<Item = &NativeObservation<WindowNotice>> + '_ {
        self.items.iter().filter_map(|(_, delivery)| match delivery {
            ContextDelivery::Observation(Observation::Item { evidence: ContextEvidence::Notice(notice), .. }) => Some(notice),
            _ => None,
        })
    }

    /// The first possible receipt of the earliest staged notice.
    pub(super) fn earliest_notice(&self) -> Option<QpcTick> {
        self.notices().map(|notice| notice.first_qpc).min()
    }

    /// Drop every staged delivery. An unmaterialized notice still ends its
    /// visit's coverage, and `cutoff`, before its first possible receipt.
    pub(super) fn retire(&mut self, cutoff: &mut Option<QpcTick>, restrict: impl Fn(VisitBinding, QpcTick)) {
        if let Some(notice) = self.notices().min_by_key(|notice| notice.first_qpc) {
            keep_earlier(cutoff, notice.first_qpc);
            restrict(notice.visit, notice.first_qpc);
        }
        self.items.clear();
    }

    /// Drop every staged delivery of a visit that opened anew.
    pub(super) fn clear(&mut self) {
        self.items.clear();
    }

    pub(super) fn pressure_first(&self) -> Option<QpcTick> {
        self.pressure.map(|gap| gap.first_qpc)
    }

    pub(super) fn take_pressure(&mut self) -> Option<ContextGap> {
        self.pressure.take()
    }

    pub(super) fn take_optional_loss(&mut self) -> Option<LossSpan> {
        self.optional_dropped.take()
    }
}

/// Extend the pending notice pressure gap with a later one, or start it. The
/// pending gap keeps its visit, cause and first position.
fn merge_context_gap(pending: &mut Option<ContextGap>, gap: ContextGap) {
    match pending {
        Some(previous) => {
            previous.last_position = gap.last_position;
            previous.first_qpc = previous.first_qpc.min(gap.first_qpc);
            previous.last_qpc = previous.last_qpc.max(gap.last_qpc);
        }
        None => *pending = Some(gap),
    }
}
