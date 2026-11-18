use std::collections::VecDeque;

use crate::protocol::{ClockOrigin, FocusGeneration, QpcTick, SessionTimeUs, ipc::ForegroundAtReceipt};

use super::{HookReceipt, ReceiptFocus, Visit};
use super::super::{ProbeError, unavailable};

/// Ended visits kept for receipt attribution.
const PREVIOUS_VISIT_LIMIT: usize = 32;
/// The receipt uncertainty of a hook-only record (remediation §6.4).
const HOOK_UNCERTAINTY_MS: u64 = 16;
const HOOK_UNCERTAINTY_US: i64 = HOOK_UNCERTAINTY_MS as i64 * 1_000;

#[derive(Clone, Copy)]
struct EndedVisit {
    visit: Visit,
    /// The last sweep that verified the visit unchanged.
    verified: QpcTick,
    /// The stamp at which its replacement was established.
    ended: QpcTick,
}

/// The current visit, the ended visits that can still attribute a receipt,
/// and the sweep horizon that verifies them.
pub(super) struct VisitTimeline {
    generation: u64,
    current: Option<Visit>,
    previous: VecDeque<EndedVisit>,
    /// The notice horizon of the latest window sweep. Every foreground or
    /// focus change before it has replaced the current visit.
    swept: Option<QpcTick>,
    focus_boundary: Option<QpcTick>,
}

impl VisitTimeline {
    pub fn new() -> Self {
        Self { generation: 0, current: None, previous: VecDeque::with_capacity(PREVIOUS_VISIT_LIMIT), swept: None,
            focus_boundary: None }
    }

    pub fn current(&self) -> Option<Visit> {
        self.current
    }

    pub fn current_mut(&mut self) -> Option<&mut Visit> {
        self.current.as_mut()
    }

    pub fn focus_boundary(&self) -> Option<QpcTick> {
        self.focus_boundary
    }

    pub fn swept(&self) -> Option<QpcTick> {
        self.swept
    }

    /// Notices through `at` were processed and the sweep's reads followed it.
    pub fn mark_swept(&mut self, at: QpcTick) {
        self.swept = Some(at);
    }

    pub fn next_generation(&mut self) -> Result<FocusGeneration, ProbeError> {
        self.generation = self.generation.checked_add(1)
            .ok_or_else(|| unavailable("The focus generation counter overflowed"))?;
        Ok(FocusGeneration(self.generation))
    }

    /// End every visit at a pause at `at`; returns the generation that
    /// follows them.
    pub fn reset(&mut self, at: QpcTick) -> Result<FocusGeneration, ProbeError> {
        let generation = self.next_generation()?;
        self.focus_boundary = Some(at);
        self.current = None;
        self.previous.clear();
        self.swept = None;
        Ok(generation)
    }

    /// Open a certified elevated visit. It replaces the current visit
    /// without an ended record: an elevated visit is never swept.
    pub fn activate(&mut self, visit: Visit) {
        self.current = Some(visit);
        self.focus_boundary = Some(visit.started_at);
    }

    /// Replace the current visit with `next`, established at `stamp`.
    /// Returns the last verification of the ended visit.
    pub fn replace(&mut self, next: Option<Visit>, stamp: QpcTick) -> Option<QpcTick> {
        self.focus_boundary = Some(stamp);
        let ended = self.current.map(|visit| EndedVisit { visit, verified: self.verified(visit), ended: stamp });
        if let Some(ended) = ended {
            if self.previous.len() == PREVIOUS_VISIT_LIMIT { self.previous.pop_front(); }
            self.previous.push_back(ended);
        }
        self.current = next;
        ended.map(|ended| ended.verified)
    }

    /// The ended visit of `proof` that started by `original` and outlived
    /// `latest`, the historical part of a receipt's approval.
    pub fn historical(&self, proof: ForegroundAtReceipt, original: QpcTick, latest: QpcTick) -> ReceiptFocus {
        self.previous.iter().rev()
            .find(|ended| ended.visit.receipt_proof() == proof && ended.visit.started_at <= original && latest < ended.ended)
            .map_or(ReceiptFocus::Unknown, |ended| ReceiptFocus::Historical(ended.visit))
    }

    pub fn focus_end(&self, generation: FocusGeneration, clock: ClockOrigin) -> Option<SessionTimeUs> {
        let ended = self.previous.iter().find(|ended| ended.visit.generation == generation)?;
        clock.time(ended.ended).ok()
    }

    /// The last stamp at which a sweep confirmed `visit` unchanged; no
    /// earlier than its start. An elevated visit is never swept.
    fn verified(&self, visit: Visit) -> QpcTick {
        self.swept.filter(|_| !visit.is_elevated()).map_or(visit.started_at, |swept| swept.max(visit.started_at))
    }

    /// Attribute a hook-only receipt `at` (remediation §6.4). A visit covers
    /// it when the visit started before the receipt's uncertainty interval
    /// `[at − HOOK_UNCERTAINTY_MS, at]` and a sweep verified it unchanged through `at`.
    pub fn hook_receipt(&self, at: QpcTick, clock: ClockOrigin) -> HookReceipt {
        let uncertainty = i64::try_from(clock.qpc_frequency.0.get() / 1_000 * HOOK_UNCERTAINTY_MS).unwrap_or(i64::MAX);
        let lower = QpcTick(at.0.saturating_sub(uncertainty));
        if let Some(visit) = self.current.filter(|visit| visit.is_elevated()) {
            // Elevated records reach the matcher only inside the certified visit.
            return if visit.started_at <= lower { HookReceipt::Covered(visit) } else { HookReceipt::Unknown };
        }
        if self.swept.is_none_or(|swept| swept < at) {
            return HookReceipt::Pending;
        }
        let current = self.current.map(|visit| (visit, self.verified(visit), None));
        let mut before = None;
        let mut after = None;
        for (visit, verified, ended) in self.previous.iter()
            .map(|ended| (ended.visit, ended.verified, Some(ended.ended))).chain(current)
        {
            if visit.started_at <= lower {
                if at <= verified {
                    return HookReceipt::Covered(visit);
                }
                before = Some((visit, ended));
            } else if after.is_none() {
                after = Some(visit);
            }
        }
        match (before, after) {
            // The replacement was established in the same sweep, so no other
            // foreground came between the two ordinary visits of one approved
            // process instance.
            (Some((before, Some(ended))), Some(after)) if ended == after.started_at
                && before.foreground.process == after.foreground.process
                && !before.is_elevated() && !after.is_elevated() =>
                HookReceipt::FocusUncertain { before: before.generation, after },
            _ => HookReceipt::Unknown,
        }
    }

    /// Drop ended visits that can no longer attribute an unpublished receipt:
    /// they ended an uncertainty interval before the published input prefix.
    /// The visit of the active key focus keeps its end for redaction.
    pub fn prune(&mut self, published: SessionTimeUs, clock: ClockOrigin, keep: Option<FocusGeneration>) {
        self.previous.retain(|ended| Some(ended.visit.generation) == keep
            || clock.time(ended.ended).is_ok_and(|end| end.0.saturating_add(HOOK_UNCERTAINTY_US) >= published.0));
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use crate::protocol::{QpcFrequency, WindowId, ipc::ProcessInstance};

    use super::super::identity::WindowProcess;
    use super::*;

    /// One tick per microsecond: the hook uncertainty is 16 000 ticks.
    const CLOCK: ClockOrigin = ClockOrigin { origin_qpc: QpcTick(0), qpc_frequency: QpcFrequency(NonZeroU64::MIN.saturating_add(999_999)) };

    fn visit(generation: u64, window: u64, pid: u32, started_at: i64) -> Visit {
        let pid = NonZeroU32::new(pid).expect("a nonzero pid");
        Visit {
            generation: FocusGeneration(generation), started_at: QpcTick(started_at),
            foreground: WindowProcess { window: WindowId(window), process: ProcessInstance { pid, creation_filetime: 5 }, thread: NonZeroU32::MIN },
            native: None, uia_blocked: None, elevated: None,
        }
    }

    /// Visit `before` ends at 100 000 and `after` replaces it in the same sweep;
    /// a hook-only receipt at 105 000 falls inside the seam.
    fn seam(before: Visit, after: Visit) -> HookReceipt {
        let mut timeline = VisitTimeline::new();
        timeline.replace(Some(before), before.started_at);
        timeline.mark_swept(QpcTick(50_000));
        timeline.replace(Some(after), QpcTick(100_000));
        timeline.mark_swept(QpcTick(200_000));
        timeline.hook_receipt(QpcTick(105_000), CLOCK)
    }

    #[test]
    fn same_process_seam_is_focus_uncertain() {
        let receipt = seam(visit(1, 10, 7, 0), visit(2, 11, 7, 100_000));
        assert!(matches!(receipt, HookReceipt::FocusUncertain { before: FocusGeneration(1), after }
            if after.generation == FocusGeneration(2)));
    }

    #[test]
    fn other_process_seam_is_unknown() {
        assert!(matches!(seam(visit(1, 10, 7, 0), visit(2, 11, 8, 100_000)), HookReceipt::Unknown));
    }

    #[test]
    fn seam_with_gap_is_unknown() {
        assert!(matches!(seam(visit(1, 10, 7, 0), visit(2, 11, 7, 100_500)), HookReceipt::Unknown));
    }
}
