//! Capture source selection: the elevated source's dispatch and activity
//! proofs, the source facts the workers report, and the selection the
//! controller makes at each opening boundary.

use std::sync::{Arc, Weak};

use crate::devlog::devlog;

use screen_fix_protocol::ipc::{FocusAtReceipt, InputRegistration, elevated::{VisitBinding, runtime::{HeldReadiness, IdentityClaim}}};

use crate::{
    CaptureError,
    package::metadata::{
        ElevatedSourceEvidence, SelectedSourceInterval, SelectedSourceKind, SourceBlindInterval, SourceEvidence,
        SourceRegistration, SourceTail, StandardSourceEvidence, UncertifiedInputTail,
    },
    platform::windows::preflight::ForegroundApproval,
    protocol::{AdmissionEpoch, QpcTick, SessionTimeUs, SourceFailureReason},
    session::{PauseReason, SessionPhase, SourceFailure, SourceStage, SourceStatus, fault::FaultReport},
    util::{keep_earlier, push_bounded},
};

use super::{facts::Facts, failure, lifecycle::Lifecycle, worker::WorkerSession};

const MAX_SOURCE_TRACE: usize = 4096;

#[derive(Clone)]
pub(in crate::platform::windows) struct SourcePreparation {
    pub(in crate::platform::windows) epoch: AdmissionEpoch,
    pub(in crate::platform::windows) startup: Option<ForegroundApproval>,
}

pub(in crate::platform::windows) struct SourceDispatch {
    lifecycle: Weak<Lifecycle>,
    epoch: AdmissionEpoch,
    target: IdentityClaim,
}

impl SourceDispatch {
    pub(in crate::platform::windows) fn target(&self) -> &IdentityClaim { &self.target }

    pub(in crate::platform::windows) fn epoch(&self) -> AdmissionEpoch { self.epoch }

    pub(in crate::platform::windows) fn stopped(&self) -> bool {
        self.lifecycle.upgrade().is_none_or(|lifecycle| lifecycle.closing())
    }

    pub(in crate::platform::windows) fn with_closed_phase<T>(
        &self, dispatch: impl FnOnce() -> screen_fix_elevated::Result<T>,
    ) -> screen_fix_elevated::Result<T> {
        let lifecycle = self.lifecycle.upgrade().ok_or(screen_fix_elevated::Failure::Cancelled)?;
        let facts = lifecycle.lock();
        if lifecycle.closing() || !source_preparation_current(&facts, self.epoch)
            || facts.source.startup_target.as_ref().is_some_and(|opening| opening.elevated() != Some(&self.target))
        {
            return Err(screen_fix_elevated::Failure::Cancelled);
        }
        dispatch()
    }
}

#[derive(Clone)]
pub(in crate::platform::windows) struct ActiveSourceProof {
    lifecycle: Weak<Lifecycle>,
    epoch: AdmissionEpoch,
}

impl ActiveSourceProof {
    pub(in crate::platform::windows) fn current(&self) -> bool {
        let Some(lifecycle) = self.lifecycle.upgrade() else { return false; };
        if lifecycle.closing() { return false; }
        let Ok(facts) = lifecycle.facts.try_lock() else { return false; };
        source_active(&facts, self.epoch)
    }
}

fn source_active(facts: &Facts, epoch: AdmissionEpoch) -> bool {
    source_epoch_active(facts, epoch) && facts.source.elevated_epoch == Some(epoch)
}

fn source_epoch_active(facts: &Facts, epoch: AdmissionEpoch) -> bool {
    facts.status.phase == SessionPhase::Recording && !facts.clock.failed && !facts.source.change
        && facts.pause.reasons.is_empty()
        && facts.bounds.opened.is_some_and(|opened| opened.epoch == epoch)
}

fn source_preparation_current(facts: &Facts, epoch: AdmissionEpoch) -> bool {
    matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Paused)
        && (facts.status.phase != SessionPhase::Paused || facts.bounds.closed.is_some())
        && facts.pause.reasons.iter().all(|reason| *reason == PauseReason::ElevationTransition)
        && facts.source.preparation.as_ref().is_some_and(|pending| pending.epoch == epoch)
}

pub(super) struct SelectedSource {
    pub(super) epoch: AdmissionEpoch,
    pub(super) approval: ForegroundApproval,
    focus: Option<FocusAtReceipt>,
}

impl SelectedSource {
    pub(super) fn current(&self) -> bool {
        self.approval.still_current()
            && self.focus.is_none_or(|focus| self.approval.focus_current(focus))
    }
}

pub(super) struct SelectedInterval {
    visit: Option<VisitBinding>,
    evidence: SelectedSourceInterval,
    cutoff: Option<SessionTimeUs>,
}

impl SelectedInterval {
    fn restrict(&mut self, before: SessionTimeUs) {
        keep_earlier(&mut self.cutoff, before);
        self.evidence.before_t_us = self.evidence.before_t_us.min(before);
    }
}

impl WorkerSession {
    pub(in crate::platform::windows) fn active_source(&self, epoch: AdmissionEpoch) -> ActiveSourceProof {
        ActiveSourceProof { lifecycle: Arc::downgrade(&self.lifecycle), epoch }
    }

    pub(in crate::platform::windows) fn elevated_active(&self, epoch: AdmissionEpoch) -> bool {
        let facts = self.lifecycle.lock();
        !self.lifecycle.closing() && source_active(&facts, epoch)
    }

    pub(in crate::platform::windows) fn ordinary_source_active(&self, epoch: AdmissionEpoch) -> bool {
        let facts = self.lifecycle.lock();
        !self.lifecycle.closing() && source_epoch_active(&facts, epoch) && facts.source.elevated_epoch.is_none()
    }

    pub(in crate::platform::windows) fn source_dispatch(&self, epoch: AdmissionEpoch, target: &IdentityClaim) -> SourceDispatch {
        SourceDispatch { lifecycle: Arc::downgrade(&self.lifecycle), epoch, target: target.clone() }
    }

    pub(in crate::platform::windows) fn elevated_epoch(&self, epoch: AdmissionEpoch) -> bool {
        self.lifecycle.lock().source.elevated_epoch == Some(epoch)
    }

    pub(in crate::platform::windows) fn require_source_change(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if facts.bounds.opened.is_some_and(|boundary| boundary.epoch == epoch)
            && matches!(facts.status.phase, SessionPhase::Recording | SessionPhase::Paused)
        {
            facts.source.change = true;
            self.wake();
        }
    }

    pub(in crate::platform::windows) fn source_preparation(&self) -> Option<SourcePreparation> {
        let facts = self.lifecycle.lock();
        facts.source.preparation.as_ref().filter(|pending| !self.lifecycle.stop_requested()
            && source_preparation_current(&facts, pending.epoch)).cloned()
    }

    pub(in crate::platform::windows) fn source_authorized(&self, epoch: AdmissionEpoch) -> bool {
        let facts = self.lifecycle.lock();
        !self.lifecycle.closing()
            && (source_preparation_current(&facts, epoch)
                || (facts.status.phase == SessionPhase::Recording && facts.source.elevated_epoch == Some(epoch)))
    }

    pub(in crate::platform::windows) fn abandon_source_preparation(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if facts.source.preparation.as_ref().is_some_and(|pending| pending.epoch == epoch) {
            facts.source.cancel_preparation();
            clear_source_status(&mut facts);
            self.wake();
        }
    }

    pub(in crate::platform::windows) fn source_ready(
        &self, epoch: AdmissionEpoch, approval: ForegroundApproval, focus: Option<FocusAtReceipt>,
    ) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop_requested()
            && source_preparation_current(&facts, epoch)
        {
            facts.source.ready = Some(SelectedSource { epoch, approval, focus });
            self.wake();
        }
    }

    pub(in crate::platform::windows) fn source_pending(&self, epoch: AdmissionEpoch, status: SourceStatus) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop_requested() && source_preparation_current(&facts, epoch) {
            facts.status.source = status;
        }
    }

    pub(in crate::platform::windows) fn source_enhanced(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop_requested() && source_active(&facts, epoch) {
            facts.status.source = SourceStatus::Enhanced;
        }
    }

    pub(in crate::platform::windows) fn update_standard_evidence(&self, update: impl FnOnce(&mut StandardSourceEvidence)) {
        update(self.lifecycle.lock().source.evidence.standard.get_or_insert_with(StandardSourceEvidence::default));
    }

    pub(in crate::platform::windows) fn update_elevated_evidence(&self, update: impl FnOnce(&mut ElevatedSourceEvidence)) {
        update(self.lifecycle.lock().source.evidence.elevated.get_or_insert_with(ElevatedSourceEvidence::default));
    }

    pub(in crate::platform::windows) fn elevated_registration(
        &self, epoch: AdmissionEpoch, observed_qpc: QpcTick, registration: InputRegistration, held: HeldReadiness,
    ) {
        let mut facts = self.lifecycle.lock();
        let registration = SourceRegistration { epoch, observed_qpc, registration, held: Some(held) };
        let evidence = &mut facts.source.evidence;
        let elevated = evidence.elevated.get_or_insert_with(ElevatedSourceEvidence::default);
        if !push_bounded(&mut elevated.registrations, registration, MAX_SOURCE_TRACE) {
            evidence.truncated = true;
        }
    }

    pub(in crate::platform::windows) fn record_source_coverage(
        &self, epoch: AdmissionEpoch, visit: Option<VisitBinding>, start: SessionTimeUs, before: SessionTimeUs,
    ) {
        let mut facts = self.lifecycle.lock();
        let before = facts.bounds.closed.filter(|closed| closed.epoch == epoch)
            .map_or(before, |closed| before.min(closed.time));
        if start >= before { return; }
        let source = &mut facts.source;
        if let Some(interval) = source.intervals.last_mut()
            && interval.evidence.epoch == epoch && interval.visit == visit
        {
            let before = interval.cutoff.map_or(before, |cutoff| before.min(cutoff));
            interval.evidence.before_t_us = interval.evidence.before_t_us.max(before);
        } else if !push_bounded(&mut source.intervals, SelectedInterval {
            visit, cutoff: None,
            evidence: SelectedSourceInterval {
                source: if visit.is_some() { SelectedSourceKind::Elevated } else { SelectedSourceKind::Ordinary },
                epoch, source_generation: visit.map(|binding| binding.source_generation), start_t_us: start,
                before_t_us: before,
            },
        }, MAX_SOURCE_TRACE) {
            source.evidence.truncated = true;
        }
    }

    pub(in crate::platform::windows) fn restrict_source_coverage(&self, visit: VisitBinding, before: QpcTick) {
        let mut facts = self.lifecycle.lock();
        let Some(before) = facts.bounds.origin.and_then(|clock| clock.time(before).ok()) else { return; };
        if let Some(interval) = facts.source.intervals.iter_mut().rev().find(|item| item.visit == Some(visit)) {
            interval.restrict(before);
        }
    }

    pub(in crate::platform::windows) fn last_elevated_coverage(&self, epoch: AdmissionEpoch) -> Option<SessionTimeUs> {
        self.lifecycle.lock().source.intervals.iter().rev()
            .find(|item| item.evidence.epoch == epoch && item.visit.is_some()
                && item.evidence.start_t_us < item.evidence.before_t_us)
            .map(|item| item.evidence.before_t_us)
    }

    pub(in crate::platform::windows) fn record_uncertified_tail(&self, epoch: AdmissionEpoch, tail: UncertifiedInputTail) {
        let mut facts = self.lifecycle.lock();
        let source = &mut facts.source;
        if let Some(interval) = source.intervals.last_mut()
            .filter(|item| item.evidence.epoch == epoch && item.visit.is_none())
        {
            interval.restrict(tail.first_t_us);
        }
        let evidence = &mut source.evidence;
        if !push_bounded(&mut evidence.uncertified_tails, SourceTail { epoch, tail }, MAX_SOURCE_TRACE) {
            evidence.truncated = true;
        }
    }

    pub(in crate::platform::windows) fn record_source_blind(&self, interval: SourceBlindInterval) {
        push_source_blind(&mut self.lifecycle.lock(), interval);
    }

    pub(in crate::platform::windows) fn source_resynchronizing(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop_requested() && source_active(&facts, epoch) {
            facts.status.source = SourceStatus::Activating;
        }
    }

    pub(in crate::platform::windows) fn source_evidence(&self) -> Option<SourceEvidence> {
        source_evidence(&self.lifecycle.lock())
    }

    pub(in crate::platform::windows) fn fail_classified_source(&self, failure: SourceFailure) -> bool {
        let mut facts = self.lifecycle.lock();
        self.stop_for_source_failure(&mut facts, failure)
    }

    pub(in crate::platform::windows) fn fail_source(&self, stage: SourceStage, failure: screen_fix_elevated::Failure) {
        let mut facts = self.lifecycle.lock();
        let admitted = facts.bounds.origin.is_some();
        self.stop_for_source_failure(&mut facts, SourceFailure::elevated(stage, failure, admitted));
    }

    /// Latch Stop for a source failure; `admitted` is set from the session clock.
    /// True when this failure set the Stop latch first.
    fn stop_for_source_failure(&self, facts: &mut Facts, mut failure: SourceFailure) -> bool {
        failure.admitted = facts.bounds.origin.is_some();
        devlog!("session", "source failure: {failure:?} guidance={:?}", crate::session::source_failure_guidance(failure));
        let first = self.lifecycle.latch_stop("source failure");
        if first || (failure.admitted && matches!(failure.stage, SourceStage::Activation | SourceStage::Capture | SourceStage::Cleanup)
            && !matches!(failure.reason, SourceFailureReason::Cancelled | SourceFailureReason::ScopeClosed))
        {
            set_source_failure(facts, failure);
            self.wake();
        }
        first
    }

    pub(in crate::platform::windows) fn required_elevated_hook_failed(&self, closing: bool) {
        let mut facts = self.lifecycle.lock();
        if closing || self.lifecycle.latch_stop("required elevated hook failed") {
            set_source_failure(&mut facts, SourceFailure {
                stage: SourceStage::Capture, reason: SourceFailureReason::RequiredHook, admitted: true, native: None, token_identity: None,
            });
            self.wake();
        }
    }

    pub(in crate::platform::windows) fn source_finalization_failed(&self, failure: screen_fix_elevated::Failure) {
        let mut facts = self.lifecycle.lock();
        if facts.bounds.origin.is_some() {
            let failure = SourceFailure::elevated(SourceStage::Cleanup, failure, true);
            if facts.status.source.failure().is_some_and(|primary| primary != failure) {
                facts.source.evidence.cleanup_failure.get_or_insert(failure);
            }
            set_source_failure(&mut facts, failure);
        }
    }

    pub(in crate::platform::windows) fn source_failure(&self) -> Option<SourceFailure> {
        self.lifecycle.lock().status.source.failure()
    }
}

impl SourceFailure {
    pub(in crate::platform::windows) fn elevated(stage: SourceStage, failure: screen_fix_elevated::Failure, admitted: bool) -> Self {
        use screen_fix_elevated::Failure;
        let native = match failure {
            Failure::NativeProducer(failure) => Some(failure),
            _ => None,
        };
        let token_identity = match failure {
            Failure::TokenIdentity(detail) => Some(detail),
            _ => None,
        };
        Self { stage, reason: failure.reason(), admitted, native, token_identity }
    }
}

pub(super) fn push_source_blind(facts: &mut Facts, interval: SourceBlindInterval) {
    if interval.start_t_us >= interval.before_t_us { return; }
    let evidence = &mut facts.source.evidence;
    if !push_bounded(&mut evidence.blind, interval, MAX_SOURCE_TRACE) {
        evidence.truncated = true;
    }
}

pub(super) fn source_evidence(facts: &Facts) -> Option<SourceEvidence> {
    let mut evidence = facts.source.evidence.clone();
    evidence.failure = facts.status.source.failure();
    evidence.intervals.extend(facts.source.intervals.iter().map(|interval| interval.evidence)
        .filter(|interval| interval.start_t_us < interval.before_t_us));
    evidence.blind.sort_by_key(|interval| (interval.start_t_us, interval.before_t_us));
    (evidence.standard.is_some() || evidence.elevated.is_some() || evidence.failure.is_some() || !evidence.intervals.is_empty()
        || !evidence.uncertified_tails.is_empty() || !evidence.blind.is_empty() || evidence.truncated).then_some(evidence)
}

pub(super) fn clear_source_status(facts: &mut Facts) {
    if facts.status.source.failure().is_none() {
        facts.status.source = SourceStatus::Unselected;
    }
}

pub(super) fn set_source_failure(facts: &mut Facts, failure: SourceFailure) {
    if facts.status.source.failure().is_none() {
        facts.record_fault(FaultReport::attempt(&CaptureError::elevated(failure)));
        facts.status.source = if failure.admitted {
            SourceStatus::FailedPrefix(failure)
        } else {
            SourceStatus::Unavailable(failure)
        };
    }
}

/// Hand out the next admission epoch for a source preparation, once per preparation.
pub(super) fn prepare_source(facts: &mut Facts) -> Result<(), CaptureError> {
    let source = &mut facts.source;
    if source.preparation.is_none() {
        source.epoch = source.epoch.checked_add(1)
            .ok_or_else(|| failure("Prepare a capture source", "The admission epoch overflowed."))?;
        source.preparation = Some(SourcePreparation {
            epoch: AdmissionEpoch(source.epoch), startup: source.startup_target.clone(),
        });
    }
    Ok(())
}

/// Select the source admitted at a new opening boundary with `epoch`.
pub(super) fn select_source(facts: &mut Facts, epoch: AdmissionEpoch, selected: Option<SelectedSource>) {
    let source = &mut facts.source;
    source.epoch = epoch.0;
    source.elevated_epoch = selected.as_ref().filter(|source| source.approval.elevated().is_some()).map(|source| source.epoch);
    facts.status.source = if source.elevated_epoch.is_some() {
        SourceStatus::Activating
    } else {
        SourceStatus::Ordinary
    };
    source.startup_target = None;
    source.cancel_preparation();
    source.change = false;
}
