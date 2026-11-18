//! The context owner publishes only a bounded foreground admission interval.
//! Capture and video take nonblocking copies without querying a window.

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError, Weak};

use crate::{
    capture::frame::{ApprovalGeneration, FrameCoverage, FrameMetadata},
    protocol::{AdmissionEpoch, ClockOrigin, QpcTick, SessionTimeUs, ipc::{ForegroundAtReceipt, elevated::VisitBinding}},
};
use crate::session::AdmissionBoundary;

use super::identity::WindowProcess;
use crate::devlog::devlog;

const MAX_AGE_US: i64 = 50_000;
pub(in crate::platform::windows) const ELEVATED_FRAME_WAIT_US: i64 = 250_000;

#[derive(Default)]
pub(in crate::platform::windows) struct FramePolicy(RwLock<State>);

#[derive(Default)]
struct State {
    generation: ApprovalGeneration,
    approval: Option<(WindowProcess, FrameApproval)>,
    // Only media already owned by a bounded pipeline keeps a token alive.
    coverage: Vec<Weak<FrameCoverage>>,
}

impl State {
    /// Replace the approval with a new one under the next generation. When
    /// the generations are exhausted, nothing is approved.
    fn approve(&mut self, window: WindowProcess, approval: impl FnOnce(ApprovalGeneration) -> FrameApproval) {
        self.approval = self.generation.0.checked_add(1).map(|generation| (window, approval(ApprovalGeneration(generation))));
        if let Some((_, approval)) = &self.approval { self.generation = approval.generation; }
    }
}

#[derive(Clone)]
pub(in crate::platform::windows) struct FrameApproval {
    pub generation: ApprovalGeneration,
    epoch: AdmissionEpoch,
    since: SessionTimeUs,
    observed: SessionTimeUs,
    certified_before: Option<SessionTimeUs>,
    pub coverage: Option<Arc<FrameCoverage>>,
}

impl FramePolicy {
    /// A writer recovers a poisoned lock: every update leaves a valid state.
    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.0.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// A nonblocking read: `None` when a writer panicked.
    fn read(&self) -> Result<Option<RwLockReadGuard<'_, State>>, ApprovalReadError> {
        match self.0.try_read() {
            Ok(state) => Ok(Some(state)),
            Err(TryLockError::WouldBlock) => Err(ApprovalReadError::Contended),
            Err(TryLockError::Poisoned(_)) => Ok(None),
        }
    }

    pub fn restrict_elevated(&self, visit: VisitBinding, before: QpcTick) {
        let mut state = self.write();
        state.coverage.retain(|coverage| {
            let Some(coverage) = coverage.upgrade() else { return false; };
            if coverage.visit == visit { coverage.restrict(before); }
            true
        });
        if state.approval.as_ref().is_some_and(|(_, approval)| approval.elevated_visit() == Some(visit)) {
            state.approval = None;
        }
    }

    pub fn clear(&self) {
        self.write().approval = None;
    }

    pub fn publish(
        &self, window: Option<WindowProcess>, observed: SessionTimeUs, boundary: AdmissionBoundary, interrupted: bool,
    ) {
        let mut state = self.write();
        if let Some(window) = window {
            if let Some((previous, approval)) = &mut state.approval
                && *previous == window && approval.epoch == boundary.epoch && !interrupted
                && approval.coverage.is_none() && approval.fresh(observed)
            {
                approval.observed = observed;
                return;
            }
            devlog!("approval", "new approval: since_us={} interrupted={} same_window={}", observed.0, interrupted,
                state.approval.as_ref().is_some_and(|(previous, _)| *previous == window));
            state.approve(window, |generation| FrameApproval {
                generation, epoch: boundary.epoch, since: observed.max(boundary.time), observed, certified_before: None, coverage: None,
            });
        } else {
            if state.approval.is_some() {
                devlog!("approval", "approval cleared: foreground not approved at_us={}", observed.0);
            }
            state.approval = None;
        }
    }

    pub fn publish_elevated(
        &self, visit: VisitBinding, since: SessionTimeUs, observed: SessionTimeUs,
        certified_before: SessionTimeUs, boundary: AdmissionBoundary, clock: ClockOrigin,
    ) {
        let since = since.max(boundary.time);
        if visit.epoch != boundary.epoch || since > observed || certified_before > observed {
            self.clear();
            return;
        }
        let certified_before = certified_before.max(since);
        let mut state = self.write();
        let window = WindowProcess::from(visit.foreground);
        if let Some((_, approval)) = &mut state.approval && approval.elevated_visit() == Some(visit)
            && approval.fresh(observed)
        {
            approval.observed = observed;
            approval.certified_before = Some(certified_before);
            return;
        }
        state.coverage.retain(|coverage| coverage.strong_count() != 0);
        let existing = state.coverage.iter().filter_map(Weak::upgrade).find(|coverage| coverage.visit == visit);
        let coverage = existing.unwrap_or_else(|| {
            let coverage = Arc::new(FrameCoverage::new(visit, clock));
            state.coverage.push(Arc::downgrade(&coverage));
            coverage
        });
        if coverage.before().is_some() {
            state.approval = None;
            return;
        }
        state.approve(window, |generation| FrameApproval {
            generation, epoch: boundary.epoch, since, observed,
            certified_before: Some(certified_before), coverage: Some(coverage),
        });
    }

    pub fn for_receipt(
        &self, proof: ForegroundAtReceipt, epoch: AdmissionEpoch, visit: Option<VisitBinding>,
        at: SessionTimeUs, now: SessionTimeUs,
    ) -> Result<Option<FrameApproval>, ApprovalReadError> {
        let Some(state) = self.read()? else { return Ok(None) };
        let Some((window, approval)) = &state.approval else {
            devlog!("approval", "receipt approval none: no current approval at_us={} now_us={}", at.0, now.0);
            return Ok(None);
        };
        if window.window != proof.window || window.process != proof.process || approval.epoch != epoch
            || approval.elevated_visit() != visit || at < approval.since || now < at
        {
            devlog!("approval", "receipt approval none: same_window={} same_process={} same_epoch={} same_visit={} at_us={} since_us={} observed_us={} now_us={} generation={}",
                window.window == proof.window, window.process == proof.process, approval.epoch == epoch,
                approval.elevated_visit() == visit, at.0, approval.since.0, approval.observed.0, now.0, approval.generation.0);
            return Ok(None);
        }
        if now < approval.observed {
            return Err(ApprovalReadError::NewerThanReceipt);
        }
        if !approval.fresh(now) {
            devlog!("approval", "receipt approval none: stale observation at_us={} observed_us={} now_us={}",
                at.0, approval.observed.0, now.0);
            return Ok(None);
        }
        if approval.coverage.as_ref().and_then(|coverage| coverage.before()).is_some_and(|before| at >= before) {
            devlog!("approval", "receipt approval none: after coverage at_us={}", at.0);
            return Ok(None);
        }
        if approval.certified_before.is_some_and(|before| at >= before) {
            return Err(ApprovalReadError::AwaitingCertificate);
        }
        Ok(Some(approval.clone()))
    }

    pub fn current(&self, epoch: AdmissionEpoch, now: SessionTimeUs) -> Option<FrameApproval> {
        self.try_current(epoch, now).ok().flatten()
    }

    pub fn try_current(&self, epoch: AdmissionEpoch, now: SessionTimeUs) -> Result<Option<FrameApproval>, ApprovalReadError> {
        let Some(state) = self.read()? else { return Ok(None) };
        let Some((_, approval)) = state.approval.as_ref().filter(|(_, approval)| approval.epoch == epoch) else {
            return Ok(None);
        };
        if approval.observed > now {
            return Err(ApprovalReadError::NewerThanReceipt);
        }
        Ok(approval.fresh(now).then(|| approval.clone()))
    }
}

#[derive(Debug)]
pub(in crate::platform::windows) enum ApprovalReadError {
    Contended,
    NewerThanReceipt,
    AwaitingCertificate,
}

impl FrameApproval {
    /// Whether the last foreground observation is at most `MAX_AGE_US`
    /// before `now`, and not after it.
    fn fresh(&self, now: SessionTimeUs) -> bool {
        (0..=MAX_AGE_US).contains(&now.0.saturating_sub(self.observed.0))
    }

    pub fn elevated_visit(&self) -> Option<VisitBinding> { self.coverage.as_ref().map(|coverage| coverage.visit) }

    pub fn matches(&self, frame: &FrameMetadata) -> bool {
        self.owns(frame)
            && frame.covered() && (self.coverage.is_none() || self.contains_source(frame.source_t_us))
    }

    pub fn owns(&self, frame: &FrameMetadata) -> bool {
        self.epoch == frame.admission && self.generation == frame.approval_generation
            && self.elevated_visit() == frame.elevated_visit()
    }

    pub fn contains_source(&self, at: SessionTimeUs) -> bool {
        self.since <= at && at <= self.observed && self.certified_before.is_none_or(|before| at < before)
            && self.coverage.as_ref().and_then(|coverage| coverage.before()).is_none_or(|before| at < before)
    }

    /// An ordinary approval that admits `at` once a later foreground
    /// observation confirms it.
    pub fn awaits_source(&self, at: SessionTimeUs) -> bool {
        self.coverage.is_none() && self.certified_before.is_none() && self.since <= at && self.observed < at
    }

    pub fn can_stage(&self, at: SessionTimeUs, receipt: SessionTimeUs) -> bool {
        self.coverage.as_ref().is_some_and(|coverage| coverage.before().is_none_or(|before| at < before))
            && self.since <= at && at <= receipt
    }

    pub fn contains_slot(&self, start: SessionTimeUs, end: SessionTimeUs) -> bool {
        self.since <= start && self.certified_before.is_none_or(|before| end <= before)
            && self.coverage.as_ref().and_then(|coverage| coverage.before()).is_none_or(|before| end <= before)
    }

    pub fn awaits_slot(&self, start: SessionTimeUs, end: SessionTimeUs) -> bool {
        self.since <= start && self.certified_before.is_some_and(|before| before < end)
            && self.coverage.as_ref().and_then(|coverage| coverage.before()).is_none_or(|before| end <= before)
    }
}
