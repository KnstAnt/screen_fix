//! The context owner publishes only a bounded foreground admission interval.
//! Capture and video take nonblocking copies without querying a window.

use std::sync::{Arc, RwLock, TryLockError, Weak};

use crate::{
    capture::frame::{FrameCoverage, FrameMetadata},
    protocol::{AdmissionEpoch, ClockOrigin, QpcTick, SessionTimeUs, ipc::{ForegroundAtReceipt, elevated::VisitBinding}},
};
use crate::session::AdmissionBoundary;

use super::identity::WindowProcess;

const MAX_AGE_US: i64 = 50_000;
pub(in crate::platform::windows) const ELEVATED_FRAME_WAIT_US: i64 = 250_000;

#[derive(Default)]
pub(in crate::platform::windows) struct FramePolicy(RwLock<State>);

#[derive(Default)]
struct State {
    generation: u64,
    approval: Option<(WindowProcess, FrameApproval)>,
    // Only media already owned by a bounded pipeline keeps a token alive.
    coverage: Vec<Weak<FrameCoverage>>,
}

#[derive(Clone)]
pub(in crate::platform::windows) struct FrameApproval {
    pub generation: u64,
    epoch: AdmissionEpoch,
    since: SessionTimeUs,
    observed: SessionTimeUs,
    certified_before: Option<SessionTimeUs>,
    pub coverage: Option<Arc<FrameCoverage>>,
}

impl FramePolicy {
    pub fn restrict_elevated(&self, visit: VisitBinding, before: QpcTick) {
        let mut state = self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).approval = None;
    }

    pub fn publish(
        &self, window: Option<WindowProcess>, observed: SessionTimeUs, boundary: AdmissionBoundary, interrupted: bool,
    ) {
        let mut state = self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(window) = window {
            if let Some((previous, approval)) = &mut state.approval
                && *previous == window && approval.epoch == boundary.epoch && !interrupted
                && approval.coverage.is_none()
                && (0..=MAX_AGE_US).contains(&observed.0.saturating_sub(approval.observed.0))
            {
                approval.observed = observed;
                return;
            }
            state.approval = state.generation.checked_add(1).map(|generation| (window, FrameApproval {
                generation, epoch: boundary.epoch, since: observed.max(boundary.time), observed, certified_before: None, coverage: None,
            }));
            if let Some((_, approval)) = &state.approval {
                state.generation = approval.generation;
            }
        } else {
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
        let mut state = self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let window = WindowProcess { window: visit.foreground.window, process: visit.foreground.process,
            thread: visit.foreground.thread };
        if let Some((_, approval)) = &mut state.approval && approval.elevated_visit() == Some(visit)
            && approval.observed <= observed && observed.0.saturating_sub(approval.observed.0) <= MAX_AGE_US
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
        state.approval = state.generation.checked_add(1).map(|generation| (window, FrameApproval {
            generation, epoch: boundary.epoch, since, observed,
            certified_before: Some(certified_before), coverage: Some(coverage),
        }));
        if let Some((_, approval)) = &state.approval { state.generation = approval.generation; }
    }

    pub fn for_receipt(
        &self, proof: ForegroundAtReceipt, epoch: AdmissionEpoch, visit: Option<VisitBinding>,
        at: SessionTimeUs, now: SessionTimeUs,
    ) -> Result<Option<FrameApproval>, ApprovalReadError> {
        let state = match self.0.try_read() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(ApprovalReadError::Contended),
            Err(TryLockError::Poisoned(_)) => return Ok(None),
        };
        let Some((window, approval)) = &state.approval else { return Ok(None); };
        if window.window != proof.window || window.process != proof.process || approval.epoch != epoch
            || approval.elevated_visit() != visit || at < approval.since || now < at
        {
            return Ok(None);
        }
        if now < approval.observed {
            return Err(ApprovalReadError::NewerThanReceipt);
        }
        if now.0.saturating_sub(approval.observed.0) > MAX_AGE_US {
            return Ok(None);
        }
        if approval.coverage.as_ref().and_then(|coverage| coverage.before()).is_some_and(|before| at >= before) {
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
        let state = match self.0.try_read() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(ApprovalReadError::Contended),
            Err(TryLockError::Poisoned(_)) => return Ok(None),
        };
        let Some((_, approval)) = state.approval.as_ref().filter(|(_, approval)| approval.epoch == epoch) else {
            return Ok(None);
        };
        if approval.observed > now {
            return Err(ApprovalReadError::NewerThanReceipt);
        }
        Ok((now.0.saturating_sub(approval.observed.0) <= MAX_AGE_US).then(|| approval.clone()))
    }
}

#[derive(Debug)]
pub(in crate::platform::windows) enum ApprovalReadError {
    Contended,
    NewerThanReceipt,
    AwaitingCertificate,
}

impl FrameApproval {
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
