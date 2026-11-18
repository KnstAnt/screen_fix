//! The startup countdown and the admission check that precedes Start.

use std::{sync::PoisonError, time::Duration};

use crate::{
    CaptureError,
    package::diagnostics::FailureComponent,
    platform::windows::preflight::ForegroundApproval,
    protocol::SourceFailureReason,
    session::{PauseReason, SessionPhase, SourceFailure, SourceStage, StartupStep},
};

use super::{
    CONTROL_WAIT, Controller, controller_qpc, disk, failure,
    facts::{AdmissionCheck, Facts},
    lifecycle::Lifecycle,
    source::{SelectedSource, prepare_source, set_source_failure},
    worker::WorkerSession,
};

const COUNTDOWN_SECONDS: u8 = 3;

/// The result of one attempt to start after the countdown.
enum StartStep {
    /// Revisit the controller loop at once.
    Revisit,
    /// Wait for the admission check or the source selection.
    Pending,
    /// Start ran; the countdown is over.
    Started,
}

impl WorkerSession {
    pub(in crate::platform::windows) fn verify_startup(&self, check: impl FnOnce() -> Result<ForegroundApproval, CaptureError>) {
        let mut facts = self.lifecycle.lock();
        while matches!(facts.admission_check, AdmissionCheck::Idle)
            && facts.status.phase == SessionPhase::Starting
            && !self.lifecycle.stop_requested()
        {
            facts = self.lifecycle.changed.wait(facts).unwrap_or_else(PoisonError::into_inner);
        }
        if facts.status.phase != SessionPhase::Starting
            || self.lifecycle.stop_requested()
            || !matches!(facts.admission_check, AdmissionCheck::Requested)
        {
            return;
        }
        facts.admission_check = AdmissionCheck::Running;
        drop(facts);
        let result = check();
        self.lifecycle.lock().admission_check = AdmissionCheck::Finished(result);
        self.wake();
    }
}

impl Controller<'_> {
    /// Advance the startup countdown. `None` revisits the controller loop at once;
    /// otherwise the bounded wait before the next step.
    pub(super) fn advance_countdown(&mut self) -> Result<Option<Duration>, CaptureError> {
        let Some(begin) = self.countdown else {
            return Ok(Some(CONTROL_WAIT));
        };
        let (held, low_disk, paused) = {
            let facts = self.lifecycle.lock();
            let reasons = &facts.pause.reasons;
            (reasons.iter().any(|reason| matches!(reason,
                PauseReason::LockScreen | PauseReason::SecureDesktop | PauseReason::Sleep)),
                reasons.contains(&PauseReason::LowDisk), !reasons.is_empty())
        };
        // A lock, secure desktop or suspend holds the countdown at its full
        // length: the approved application must be in front when it ends.
        if held {
            self.countdown = Some(controller_qpc()?);
            self.lifecycle.lock().status.countdown_seconds = None;
            return Ok(Some(CONTROL_WAIT));
        }
        let elapsed = u64::try_from(controller_qpc()?.ticks_since(begin))
            .map_err(|_| failure("Count down to recording", "QPC moved backward during the countdown."))?;
        let remaining = Duration::from_secs(u64::from(COUNTDOWN_SECONDS)).saturating_sub(self.frequency.duration(elapsed));
        if !remaining.is_zero() {
            let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
            self.lifecycle.lock().status.countdown_seconds = Some(u8::try_from(seconds).unwrap_or(COUNTDOWN_SECONDS));
            // Use a bounded wait and let the next QPC read decide when Start occurs.
            return Ok(Some(CONTROL_WAIT.min(remaining)));
        }
        if low_disk {
            return Err(startup_disk_error());
        }
        if paused {
            self.lifecycle.lock().status.countdown_seconds = None;
            return Ok(Some(CONTROL_WAIT));
        }
        Ok(match self.start_when_admitted()? {
            StartStep::Revisit => None,
            StartStep::Pending => Some(CONTROL_WAIT),
            StartStep::Started => {
                self.countdown = None;
                Some(CONTROL_WAIT)
            }
        })
    }

    /// Start once the countdown has ended: after the startup admission check, or,
    /// for an approved elevated target, once its source is ready.
    fn start_when_admitted(&self) -> Result<StartStep, CaptureError> {
        let lifecycle = self.lifecycle;
        let (checked, selected) = {
            let mut facts = lifecycle.lock();
            if facts.source.startup_target.is_some() {
                prepare_source(&mut facts)?;
                (None, facts.source.ready.take())
            } else {
                (poll_admission_check(lifecycle, &mut facts), None)
            }
        };
        match (checked, selected) {
            (Some(checked), _) => self.start_checked(checked),
            (None, Some(selected)) => self.start_selected(selected),
            (None, None) => Ok(StartStep::Pending),
        }
    }

    /// Act on the startup admission check: an ordinary target starts at once, an
    /// elevated target first prepares its source.
    fn start_checked(&self, checked: Result<ForegroundApproval, CaptureError>) -> Result<StartStep, CaptureError> {
        let lifecycle = self.lifecycle;
        if lifecycle.stop_requested() {
            return Ok(StartStep::Revisit);
        }
        let approved = match checked {
            Ok(approved) => approved,
            Err(error) => {
                let mut facts = lifecycle.lock();
                if lifecycle.stop_requested() {
                    return Ok(StartStep::Revisit);
                }
                if let Some(failure) = error.source_failure()
                    && !commit_startup_source_failure(lifecycle, &mut facts, failure)
                {
                    return Ok(StartStep::Revisit);
                }
                return Err(error);
            }
        };
        if !start_disk_available(&lifecycle.lock()) {
            return Err(startup_disk_error());
        }
        if !approved.still_current() {
            if approved.elevated().is_some() {
                let mut facts = lifecycle.lock();
                return startup_target_changed(lifecycle, &mut facts, SourceStage::Foreground);
            }
            return Err(failure("Verify approved foreground",
                "The approved foreground window changed before recording started. Retry and keep the application in front."));
        }
        if approved.elevated().is_some() {
            let mut facts = lifecycle.lock();
            facts.source.startup_target = Some(approved);
            prepare_source(&mut facts)?;
            return Ok(StartStep::Pending);
        }
        self.start(None)?;
        Ok(StartStep::Started)
    }

    /// Start with the elevated source selected for the approved startup target.
    fn start_selected(&self, selected: SelectedSource) -> Result<StartStep, CaptureError> {
        let lifecycle = self.lifecycle;
        let live = {
            let facts = lifecycle.lock();
            facts.source.startup_target.as_ref().and_then(ForegroundApproval::elevated) == selected.approval.elevated()
                && start_disk_available(&facts)
        };
        if !live || !selected.current() {
            let mut facts = lifecycle.lock();
            if lifecycle.stop_requested() {
                return Ok(StartStep::Revisit);
            }
            if !start_disk_available(&facts) {
                return Err(startup_disk_error());
            }
            return startup_target_changed(lifecycle, &mut facts, SourceStage::Readiness);
        }
        self.start(Some(selected))?;
        Ok(StartStep::Started)
    }
}

/// Request the startup admission check once, and take its result when it has finished.
fn poll_admission_check(lifecycle: &Lifecycle, facts: &mut Facts) -> Option<Result<ForegroundApproval, CaptureError>> {
    match std::mem::take(&mut facts.admission_check) {
        AdmissionCheck::Idle => {
            facts.admission_check = AdmissionCheck::Requested;
            facts.status.startup = StartupStep::VerifyingAdmission;
            facts.status.countdown_seconds = None;
            lifecycle.changed.notify_all();
            None
        }
        AdmissionCheck::Finished(result) => Some(result),
        pending => {
            facts.admission_check = pending;
            facts.status.countdown_seconds = None;
            None
        }
    }
}

/// The elevated startup target changed before Start. The failure is reported
/// only when it latches Stop first; otherwise the controller loop revisits.
fn startup_target_changed(lifecycle: &Lifecycle, facts: &mut Facts, stage: SourceStage) -> Result<StartStep, CaptureError> {
    let failure = SourceFailure {
        stage, reason: SourceFailureReason::TargetChanged, admitted: false, native: None, token_identity: None,
    };
    if !commit_startup_source_failure(lifecycle, facts, failure) {
        return Ok(StartStep::Revisit);
    }
    Err(CaptureError::elevated(failure))
}

fn commit_startup_source_failure(lifecycle: &Lifecycle, facts: &mut Facts, failure: SourceFailure) -> bool {
    if lifecycle.session_ending() || !lifecycle.latch_stop("startup source failure") {
        return false;
    }
    set_source_failure(facts, failure);
    true
}

fn startup_disk_error() -> CaptureError {
    CaptureError::owned(FailureComponent::Controller, "Verify startup disk space", None, format!(
        "The recording output volume no longer reports at least {} GB free. Free space or pick another folder, then retry.",
        disk::START_MINIMUM_GB,
    ))
}

/// The startup volume still has the free space that Start requires.
fn start_disk_available(facts: &Facts) -> bool {
    facts.status.disk_available_bytes.is_some_and(|bytes| bytes >= disk::START_MINIMUM)
}
