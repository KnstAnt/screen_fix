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

    /// Publish the foreground approval. An ordinary approval is per process
    /// instance: another top-level window of the approved process continues
    /// it under the same generation and `since`. `interrupted` reports a
    /// foreground change that may have left that process: a foreground notice
    /// for another process, a desktop switch or a lost notice. Returns the
    /// transition, if any, for the development log.
    pub fn publish(
        &self, window: Option<WindowProcess>, observed: SessionTimeUs, boundary: AdmissionBoundary, interrupted: bool,
    ) -> Option<Transition> {
        let mut state = self.write();
        let previous = state.approval.as_ref().map(|(previous, _)| *previous);
        if let Some(window) = window {
            if let Some((previous, approval)) = &mut state.approval
                && previous.process == window.process && approval.epoch == boundary.epoch && !interrupted
                && approval.coverage.is_none() && approval.fresh(observed)
            {
                let changed = previous.window != window.window;
                *previous = window;
                approval.observed = observed;
                return changed.then_some(Transition::Continued);
            }
            devlog!("approval", "new approval: since_us={} interrupted={} same_process={}", observed.0, interrupted,
                previous.is_some_and(|previous| previous.process == window.process));
            state.approve(window, |generation| FrameApproval {
                generation, epoch: boundary.epoch, since: observed.max(boundary.time), observed, certified_before: None, coverage: None,
            });
            Some(Transition::Replaced(previous))
        } else {
            if previous.is_some() {
                devlog!("approval", "approval cleared: foreground not approved at_us={}", observed.0);
            }
            state.approval = None;
            previous.map(|previous| Transition::Replaced(Some(previous)))
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
        // An ordinary approval spans every window of its process; an elevated
        // one stays bound to its visit's window.
        let same_window = window.window == proof.window || approval.coverage.is_none();
        if !same_window || window.process != proof.process || approval.epoch != epoch
            || approval.elevated_visit() != visit || at < approval.since || now < at
        {
            devlog!("approval", "receipt approval none: same_window={} same_process={} same_epoch={} same_visit={} at_us={} since_us={} observed_us={} now_us={} generation={}",
                same_window, window.process == proof.process, approval.epoch == epoch,
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

/// An approval change published by `FramePolicy::publish`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::platform::windows) enum Transition {
    /// Another window of the approved process instance is foreground; the
    /// generation and `since` are kept.
    Continued,
    /// A new generation or a clear; the previously approved window, if any.
    Replaced(Option<WindowProcess>),
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

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use crate::protocol::{FocusGeneration, QpcFrequency, WindowId, ipc::ProcessInstance};

    use super::*;

    const BOUNDARY: AdmissionBoundary = AdmissionBoundary { epoch: AdmissionEpoch(1), qpc: QpcTick(0), time: SessionTimeUs(0) };

    fn window(window: u64, pid: u32) -> WindowProcess {
        let pid = NonZeroU32::new(pid).expect("a nonzero pid");
        WindowProcess { window: WindowId(window), process: ProcessInstance { pid, creation_filetime: 5 }, thread: NonZeroU32::MIN }
    }

    fn visit(foreground: WindowProcess) -> VisitBinding {
        VisitBinding {
            epoch: BOUNDARY.epoch, source_generation: NonZeroU64::MIN, grant: NonZeroU64::MIN, witness_revision: 0,
            foreground: foreground.into(), focus_generation: FocusGeneration(1), focus_revision: 0,
            resync_revision: NonZeroU64::MIN,
        }
    }

    /// An elevated approval of `visit` observed at 200 since 100.
    fn publish_elevated(policy: &FramePolicy, visit: VisitBinding) {
        let clock = ClockOrigin { origin_qpc: QpcTick(0), qpc_frequency: QpcFrequency(NonZeroU64::MIN) };
        policy.publish_elevated(visit, SessionTimeUs(100), SessionTimeUs(200), SessionTimeUs(200), BOUNDARY, clock);
    }

    fn receipt(policy: &FramePolicy, proof: WindowProcess, visit: Option<VisitBinding>, at: i64, now: i64) -> Option<ApprovalGeneration> {
        let proof = ForegroundAtReceipt { window: proof.window, process: proof.process, focus: None };
        policy.for_receipt(proof, BOUNDARY.epoch, visit, SessionTimeUs(at), SessionTimeUs(now))
            .expect("a readable approval").map(|approval| approval.generation)
    }

    fn current(policy: &FramePolicy, now: i64) -> Option<(ApprovalGeneration, SessionTimeUs)> {
        policy.current(BOUNDARY.epoch, SessionTimeUs(now)).map(|approval| (approval.generation, approval.since))
    }

    #[test]
    fn another_window_of_the_process_continues_the_approval() {
        let policy = FramePolicy::default();
        assert!(policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false) == Some(Transition::Replaced(None)));
        let first = current(&policy, 100);
        assert!(policy.publish(Some(window(11, 7)), SessionTimeUs(120), BOUNDARY, false) == Some(Transition::Continued));
        assert!(policy.publish(Some(window(10, 7)), SessionTimeUs(140), BOUNDARY, false) == Some(Transition::Continued));
        assert!(policy.publish(Some(window(10, 7)), SessionTimeUs(160), BOUNDARY, false).is_none());
        assert_eq!(current(&policy, 160), first);
        assert_eq!(first, Some((ApprovalGeneration(1), SessionTimeUs(100))));
    }

    #[test]
    fn another_process_starts_a_new_generation() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        let other = window(10, 8);
        assert!(policy.publish(Some(other), SessionTimeUs(120), BOUNDARY, false)
            == Some(Transition::Replaced(Some(window(10, 7)))));
        assert_eq!(current(&policy, 120), Some((ApprovalGeneration(2), SessionTimeUs(120))));
    }

    #[test]
    fn an_interruption_starts_a_new_generation() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        policy.publish(Some(window(11, 7)), SessionTimeUs(120), BOUNDARY, true);
        assert_eq!(current(&policy, 120), Some((ApprovalGeneration(2), SessionTimeUs(120))));
    }

    #[test]
    fn an_unapproved_foreground_clears_the_approval() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        assert!(policy.publish(None, SessionTimeUs(120), BOUNDARY, false) == Some(Transition::Replaced(Some(window(10, 7)))));
        assert_eq!(current(&policy, 120), None);
        policy.publish(Some(window(10, 7)), SessionTimeUs(140), BOUNDARY, false);
        assert_eq!(current(&policy, 140), Some((ApprovalGeneration(2), SessionTimeUs(140))));
    }

    #[test]
    fn another_epoch_starts_a_new_generation() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        let boundary = AdmissionBoundary { epoch: AdmissionEpoch(2), ..BOUNDARY };
        assert!(policy.publish(Some(window(11, 7)), SessionTimeUs(120), boundary, false)
            == Some(Transition::Replaced(Some(window(10, 7)))));
        let approval = policy.current(boundary.epoch, SessionTimeUs(120)).map(|approval| (approval.generation, approval.since));
        assert_eq!(approval, Some((ApprovalGeneration(2), SessionTimeUs(120))));
    }

    #[test]
    fn a_stale_approval_starts_a_new_generation() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        let late = 100 + MAX_AGE_US + 1;
        assert!(policy.publish(Some(window(11, 7)), SessionTimeUs(late), BOUNDARY, false)
            == Some(Transition::Replaced(Some(window(10, 7)))));
        assert_eq!(current(&policy, late), Some((ApprovalGeneration(2), SessionTimeUs(late))));
    }

    #[test]
    fn an_elevated_approval_is_not_continued() {
        let policy = FramePolicy::default();
        publish_elevated(&policy, visit(window(10, 7)));
        assert_eq!(current(&policy, 200), Some((ApprovalGeneration(1), SessionTimeUs(100))));
        assert!(policy.publish(Some(window(11, 7)), SessionTimeUs(210), BOUNDARY, false)
            == Some(Transition::Replaced(Some(window(10, 7)))));
        assert_eq!(current(&policy, 210), Some((ApprovalGeneration(2), SessionTimeUs(210))));
    }

    #[test]
    fn a_reused_pid_starts_a_new_generation() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        let reused = window(11, 7);
        let reused = WindowProcess { process: ProcessInstance { creation_filetime: 6, ..reused.process }, ..reused };
        assert!(policy.publish(Some(reused), SessionTimeUs(120), BOUNDARY, false)
            == Some(Transition::Replaced(Some(window(10, 7)))));
        assert_eq!(current(&policy, 120), Some((ApprovalGeneration(2), SessionTimeUs(120))));
    }

    #[test]
    fn an_ordinary_receipt_accepts_another_window_of_the_process() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        assert_eq!(receipt(&policy, window(11, 7), None, 110, 120), Some(ApprovalGeneration(1)));
    }

    #[test]
    fn a_receipt_of_another_process_is_refused() {
        let policy = FramePolicy::default();
        policy.publish(Some(window(10, 7)), SessionTimeUs(100), BOUNDARY, false);
        assert_eq!(receipt(&policy, window(10, 8), None, 110, 120), None);
        assert_eq!(receipt(&policy, window(11, 8), None, 110, 120), None);
    }

    #[test]
    fn an_elevated_receipt_of_another_window_is_refused() {
        let policy = FramePolicy::default();
        let visit = visit(window(10, 7));
        publish_elevated(&policy, visit);
        assert_eq!(receipt(&policy, window(10, 7), Some(visit), 150, 200), Some(ApprovalGeneration(1)));
        assert_eq!(receipt(&policy, window(11, 7), Some(visit), 150, 200), None);
    }
}
