//! The context owner publishes only a bounded foreground admission interval.
//! Capture and video take nonblocking copies without querying a window.

use std::sync::{RwLock, TryLockError};

use crate::protocol::{AdmissionEpoch, SessionTimeUs, ipc::ForegroundAtReceipt};
use crate::session::AdmissionBoundary;

use super::identity::WindowProcess;

const MAX_AGE_US: i64 = 50_000;

#[derive(Default)]
pub(in crate::platform::windows) struct FramePolicy(RwLock<State>);

#[derive(Default)]
struct State {
    generation: u64,
    approval: Option<(WindowProcess, FrameApproval)>,
}

#[derive(Clone, Copy)]
pub(in crate::platform::windows) struct FrameApproval {
    pub generation: u64,
    epoch: AdmissionEpoch,
    since: SessionTimeUs,
    observed: SessionTimeUs,
}

impl FramePolicy {
    pub fn publish(
        &self, window: Option<WindowProcess>, observed: SessionTimeUs, boundary: AdmissionBoundary, interrupted: bool,
    ) {
        let mut state = self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(window) = window {
            if let Some((previous, mut approval)) = state.approval
                && previous == window && approval.epoch == boundary.epoch && !interrupted
                && (0..=MAX_AGE_US).contains(&observed.0.saturating_sub(approval.observed.0))
            {
                approval.observed = observed;
                state.approval = Some((window, approval));
                return;
            }
            state.approval = state.generation.checked_add(1).map(|generation| (window, FrameApproval {
                generation, epoch: boundary.epoch, since: observed.max(boundary.time), observed,
            }));
            if let Some((_, approval)) = state.approval {
                state.generation = approval.generation;
            }
        } else {
            state.approval = None;
        }
    }

    pub fn for_receipt(
        &self, proof: ForegroundAtReceipt, epoch: AdmissionEpoch, at: SessionTimeUs, now: SessionTimeUs,
    ) -> Option<FrameApproval> {
        let state = self.0.try_read().ok()?;
        let (window, approval) = state.approval?;
        (window.window == proof.window && window.process == proof.process && approval.epoch == epoch
            && approval.since <= at && at <= now && approval.observed <= now
            && now.0.saturating_sub(approval.observed.0) <= MAX_AGE_US).then_some(approval)
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
        let Some((_, approval)) = state.approval.filter(|(_, approval)| approval.epoch == epoch) else {
            return Ok(None);
        };
        if approval.observed > now {
            return Err(ApprovalReadError::NewerThanReceipt);
        }
        Ok((now.0.saturating_sub(approval.observed.0) <= MAX_AGE_US).then_some(approval))
    }
}

#[derive(Debug)]
pub(in crate::platform::windows) enum ApprovalReadError {
    Contended,
    NewerThanReceipt,
}

impl FrameApproval {
    pub fn contains_source(self, at: SessionTimeUs) -> bool {
        self.since <= at && at <= self.observed
    }

    pub fn contains_slot(self, start: SessionTimeUs) -> bool {
        self.since <= start
    }
}
