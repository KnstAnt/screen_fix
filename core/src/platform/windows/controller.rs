//! Controller thread and the lifecycle state it shares with the recording
//! workers. Only the controller changes the phase, boundaries, and clock facts;
//! workers read them and report readiness, pause prefixes, source facts and
//! faults through `WorkerSession`.
//!
//! - `facts`: the lifecycle facts behind the lifecycle mutex;
//! - `lifecycle`: the lifecycle shared by all threads, and the UI's `Control`;
//! - `worker`: `WorkerSession` and the recording worker thread;
//! - `source`: capture source selection and its evidence;
//! - `startup`: the countdown and the startup admission check;
//! - `transitions`: start, pause, resume and stop;
//! - this module: the controller thread and its loop.

pub(super) mod disk;
mod environment;
mod facts;
mod lifecycle;
mod source;
mod startup;
mod transitions;
mod worker;

pub(super) use environment::{AwakeElapsed, AwakeMark, SuspendMarks, SuspendWatch};
pub(super) use facts::ControlPrefix;
pub(crate) use lifecycle::Control;
pub(super) use source::{ActiveSourceProof, SourceDispatch, SourcePreparation};
pub(super) use worker::{
    Admission, Budget, FixtureNotice, InputFailure, PauseProducer, PausedVideo, Receipt, VideoFailure, WorkerSession,
};

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use screen_fix_native_observation::StaticLease;

use crate::{
    CaptureError,
    config::ResolvedCaptureConfig,
    devlog::devlog,
    package::diagnostics::{DiagnosticContext, FailureComponent},
    protocol::{QpcFrequency, QpcTick, UtcAnchor},
    session::{
        AdmissionBoundary, PauseReason, SessionCommand, SessionHandle, SessionPhase, SessionResult, StartupStep,
        fault::{FaultOutcome, FaultReport},
        hotkey::{ControlLoss, ControlNotice, ControlOutcome, HotkeyActivation, RecorderAction},
    },
    util::keep_earlier,
};

use super::{ProbeError, clock, hotkeys::ActiveHotkeys};

use facts::CompletePublication;
use lifecycle::{Lifecycle, user_pause_available};
use worker::RecordingWorker;

const CONTROL_WAIT: Duration = Duration::from_millis(25);
pub(super) const MAX_FIXTURE_SAVES: usize = 8;
pub(super) const MAX_INPUT_FAILURES: usize = 4;
/// The hotkey progress gap that records a control loss (S1).
const HOTKEY_PROGRESS_GAP: Duration = Duration::from_millis(250);

// The recording worker and all nested owners retain this lease until their
// actual exit, even when the controller has stopped waiting for a result.
static RECORDING_OWNED: AtomicBool = AtomicBool::new(false);

pub(crate) fn spawn(
    config: ResolvedCaptureConfig,
    hotkeys: ActiveHotkeys,
    trigger: Option<HotkeyActivation>,
    record: fn(&WorkerSession) -> Result<SessionResult, CaptureError>,
) -> Result<SessionHandle, CaptureError> {
    let lease = StaticLease::claim(&RECORDING_OWNED)
        .ok_or_else(|| failure("Start recording", "The previous recording still owns capture or output resources."))?;
    let lifecycle = Arc::new(Lifecycle::new(hotkeys.virtual_keys()));
    if let Some(activation) = trigger {
        lifecycle.lock().hotkeys.push(ControlNotice { activation, outcome: ControlOutcome::StartRequested });
    }
    let (commands, incoming) = mpsc::sync_channel(8);
    let (wake, woken) = mpsc::sync_channel(1);
    let (complete, result) = mpsc::sync_channel(1);
    let (ready, initialized) = mpsc::sync_channel(1);
    let endpoint = WorkerSession {
        lifecycle: Arc::clone(&lifecycle),
        config: Arc::new(config),
        ready,
        wake: wake.clone(),
    };
    let controller_lifecycle = Arc::clone(&lifecycle);
    thread::Builder::new().name("session-controller".into()).spawn(move || {
        let config = endpoint.config.clone();
        let inbox = Inbox { commands: incoming, wake: woken, ready: initialized };
        let result = run(&controller_lifecycle, inbox, endpoint, hotkeys, record, lease)
            .map_err(|error| {
                let facts = controller_lifecycle.lock();
                let mut context = DiagnosticContext::for_config(&config);
                context.input_registration = facts.status.input_ready;
                context.source_evidence = source::source_evidence(&facts);
                error.with_context(context)
            });
        let mut facts = controller_lifecycle.lock();
        facts.status.outcome = result.as_ref().ok().map(|result| result.outcome);
        facts.status.phase = if result.is_ok() {
            SessionPhase::Finished
        } else {
            SessionPhase::Failed
        };
        devlog!("session", "phase -> {:?} outcome={:?} error_operation={:?}", facts.status.phase,
            facts.status.outcome, result.as_ref().err().map(CaptureError::operation));
        crate::devlog::close();
        drop(facts);
        controller_lifecycle.changed.notify_all();
        let _ = complete.try_send(result);
    }).map_err(|error| CaptureError::owned(FailureComponent::Controller, "Start the session controller", None, error.to_string()))?;
    Ok(SessionHandle::new(result, Control { lifecycle, commands, wake }))
}

/// The controller thread's receiving ends.
struct Inbox {
    commands: Receiver<SessionCommand>,
    wake: Receiver<()>,
    /// The recording worker's readiness.
    ready: Receiver<()>,
}

fn run(
    lifecycle: &Lifecycle,
    inbox: Inbox,
    endpoint: WorkerSession,
    hotkeys: ActiveHotkeys,
    record: fn(&WorkerSession) -> Result<SessionResult, CaptureError>,
    lease: StaticLease,
) -> Result<SessionResult, CaptureError> {
    let frequency = clock::frequency().map_err(controller_error)?;
    let (environment, notifications_ready) =
        environment::Notifications::spawn(Arc::clone(&lifecycle.environment), endpoint.wake.clone())?;
    // The controller reads the suspend marks through the same accessor as the owners.
    let session = endpoint.clone();
    let mut worker = RecordingWorker::spawn(endpoint, record, lease)?;
    let result = Controller {
        lifecycle, session: &session, hotkeys: &hotkeys, environment: &environment, notifications_ready, frequency,
        countdown: None, pause_finished: None, disk_resume: None, subscribed: false,
    }.coordinate(&inbox, &mut worker);
    if let Err(error) = &result {
        devlog!("session", "controller failed: operation={:?} message={:?}", error.operation(), error.to_string());
        lifecycle.lock().record_fault(FaultReport::attempt(error));
        // A failed controller still closes admission and releases startup
        // waiters. A failed QPC sample may prevent a certified prefix.
        let _ = transitions::stop(lifecycle);
    }
    while !worker.finished() {
        let (remaining, release_session_end_wait) = {
            let mut facts = lifecycle.lock();
            lifecycle.apply_session_end_deadlines(&mut facts);
            let deadline = facts.stop_deadline.unwrap_or_else(Instant::now);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() && facts.complete_publication != CompletePublication::Authorized {
                if worker.finished() {
                    break;
                }
                facts.complete_publication = CompletePublication::Expired;
                // The retained primary cause is never replaced by the deadline (§6.2 rule 2).
                let timeout = failure("Finish recording",
                    "The recording could not finish before its deadline. An active owner may still hold capture or output resources.");
                facts.record_fault(FaultReport::attempt(&timeout));
                return Err(retained_cause(facts.reports.faults.primary_outcome(), timeout));
            }
            (remaining, remaining.is_zero() && lifecycle.session_ending())
        };
        if release_session_end_wait {
            environment.finished();
        }
        // The worker wakes the controller when it exits. Not every writer of
        // the stop deadline wakes it, so every wait stays bounded.
        let wait = if remaining.is_zero() { CONTROL_WAIT } else { remaining.min(CONTROL_WAIT) };
        let _ = inbox.wake.recv_timeout(wait);
    }
    let worker_result = worker.join();
    let mut facts = lifecycle.lock();
    if facts.complete_publication == CompletePublication::Authorized {
        drop(facts);
        return worker_result?.1;
    }
    lifecycle.apply_session_end_deadlines(&mut facts);
    let (completed, worker_result) = worker_result?;
    let acknowledged = match result {
        Ok(acknowledged) => acknowledged,
        // The worker composes its error from the same fault log, which already
        // holds this controller failure, so its first attempt failure is retained.
        Err(error) => return Err(worker_result.err().unwrap_or(error)),
    };
    if facts.stop_deadline.is_some_and(|deadline| completed >= deadline) {
        let late = failure("Finish recording", "The recording owners finished after the finalization deadline.");
        facts.record_fault(FaultReport::attempt(&late));
        return Err(match worker_result {
            Err(error) => error.with_additional_failure(late.failure()),
            Ok(_) => retained_cause(facts.reports.faults.primary_outcome(), late),
        });
    }
    let message = worker_result?;
    if let Some(cutoff) = facts.bounds.closed
        && acknowledged != Some(cutoff) && facts.completed_prefix != Some(cutoff)
    {
        return Err(failure("Finish recording", "The recording worker did not finish the admitted producer prefixes."));
    }
    Ok(message)
}

/// The primary cause with every other retained cause attached for diagnostics.
fn retained_cause(outcome: FaultOutcome, fallback: CaptureError) -> CaptureError {
    let FaultOutcome { error, additional, .. } = outcome;
    additional.into_iter().fold(error.unwrap_or(fallback), CaptureError::with_additional_failure)
}

/// The controller loop and the state its steps share. The pause reasons are
/// lifecycle facts (`PauseFacts::reasons`); only `set_pause` changes them.
struct Controller<'a> {
    lifecycle: &'a Lifecycle,
    session: &'a WorkerSession,
    hotkeys: &'a ActiveHotkeys,
    environment: &'a environment::Notifications,
    /// The Windows notification owner's subscription result.
    notifications_ready: Receiver<Result<(), CaptureError>>,
    frequency: QpcFrequency,
    /// The countdown start, from readiness until Start.
    countdown: Option<QpcTick>,
    /// The cutoff of a pause whose producers have all finished their prefix.
    pause_finished: Option<AdmissionBoundary>,
    /// When the user asked to resume a low-disk pause; the next disk sample
    /// taken after it decides.
    disk_resume: Option<Instant>,
    /// The Windows notification owner has subscribed.
    subscribed: bool,
}

impl Controller<'_> {
    fn coordinate(
        &mut self, inbox: &Inbox, worker: &mut RecordingWorker,
    ) -> Result<Option<AdmissionBoundary>, CaptureError> {
        let lifecycle = self.lifecycle;
        loop {
            if lifecycle.session_ending() {
                lifecycle.latch_stop("session end");
            }
            self.route_hotkeys()?;
            if lifecycle.stop_requested() || worker.finished() {
                devlog!("session", "stopping: stop_requested={}", lifecycle.stop_requested());
                transitions::stop(lifecycle)?;
                if let Some(failure) = lifecycle.lock().status.source.failure() {
                    return Err(CaptureError::elevated(failure));
                }
                return Ok(self.pause_finished);
            }
            lifecycle.controller_qpc.store(controller_qpc()?.0, Ordering::Release);
            self.watch_notifications()?;
            self.track_environment()?;
            let startup_hotkeys_lost = {
                let facts = lifecycle.lock();
                facts.status.hotkeys_unavailable && facts.status.phase == SessionPhase::Starting
            };
            if startup_hotkeys_lost {
                return Err(failure("Verify recorder hotkey availability", "The registered recorder hotkeys became unavailable before recording started."));
            }
            if self.subscribed && self.countdown.is_none() && inbox.ready.try_recv().is_ok() {
                lifecycle.lock().status.startup = StartupStep::Ready;
                self.countdown = Some(controller_qpc()?);
            }
            // Bound processing per wake and revisit the independent Stop latch first.
            for command in inbox.commands.try_iter().take(8) {
                if lifecycle.stop_requested() {
                    break;
                }
                self.user_command(command)?;
            }
            if lifecycle.stop_requested() {
                continue;
            }
            self.update_disk()?;
            let handoff = {
                let facts = lifecycle.lock();
                facts.source.change || (facts.status.phase == SessionPhase::Paused && facts.source.elevated_epoch.is_some())
            };
            if handoff {
                self.set_pause(PauseReason::ElevationTransition, true)?;
            }
            self.track_pause_prefix();
            if self.pause_overdue()? {
                return Err(failure("Finish the paused recording prefix", "A recording owner did not finish its pause prefix."));
            }
            if let Some(finished) = self.pause_finished
                && self.reopen_finished_pause(handoff, finished)?
            {
                self.pause_finished = None;
            }
            let Some(wait) = self.advance_countdown()? else {
                continue;
            };
            transitions::sample_anchors(lifecycle)?;
            let _ = inbox.wake.recv_timeout(wait);
        }
    }

    /// Follow the Windows notification owner's subscription. An owner that
    /// failed to start or exited afterwards fails the attempt.
    fn watch_notifications(&mut self) -> Result<(), CaptureError> {
        if self.subscribed {
            if !self.environment.running() {
                return Err(failure("Monitor recording environment", "The Windows notification owner exited unexpectedly."));
            }
            return Ok(());
        }
        match self.notifications_ready.try_recv() {
            Ok(result) => {
                result?;
                self.subscribed = true;
                Ok(())
            }
            Err(mpsc::TryRecvError::Empty) => Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(failure("Monitor recording environment", "The Windows notification owner failed to start."))
            }
        }
    }

    /// Pause for every asserted environment reason and resume each one that is no
    /// longer active. A new sleep pause also bounds video finalization and the
    /// elevated collector's cleanup by the suspend budget.
    fn track_environment(&mut self) -> Result<(), CaptureError> {
        for state in self.environment.reasons() {
            if state.asserted || state.active {
                if state.reason == PauseReason::Sleep {
                    let mut facts = self.lifecycle.lock();
                    if !facts.pause.reasons.contains(&PauseReason::Sleep)
                        && matches!(facts.status.phase, SessionPhase::Recording | SessionPhase::Paused)
                    {
                        // Video finalization may continue after wake; a miss is
                        // a health row, never a fault (D-5 A).
                        facts.pause.suspend.wait = Some(self.session.suspend_marks().sequence);
                        keep_earlier(&mut facts.pause.suspend.cleanup, Instant::now() + environment::SUSPEND_BUDGET);
                    }
                }
                self.set_pause(state.reason, true)?;
            }
            if !state.active {
                self.set_pause(state.reason, false)?;
            }
        }
        Ok(())
    }

    /// `Control::submit` forwards only the user's Pause and Resume.
    fn user_command(&mut self, command: SessionCommand) -> Result<(), CaptureError> {
        match command {
            SessionCommand::Pause(PauseReason::User) => {
                // The phase may have changed since `Control::submit` accepted the pause.
                if !user_pause_available(self.lifecycle.lock().status.phase) {
                    return Ok(());
                }
                self.set_pause(PauseReason::User, true)
            }
            SessionCommand::Resume(PauseReason::User) => self.user_resume(),
            _ => Ok(()),
        }
    }

    /// A user Resume clears User and requests a new disk-space check.
    fn user_resume(&mut self) -> Result<(), CaptureError> {
        if self.lifecycle.lock().pause.reasons.contains(&PauseReason::LowDisk) {
            self.disk_resume = Some(Instant::now());
        }
        self.set_pause(PauseReason::User, false)
    }

    fn update_disk(&mut self) -> Result<(), CaptureError> {
        let sample = {
            let mut facts = self.lifecycle.lock();
            let sample = facts.disk.filter(|sample| sample.started.elapsed() <= disk::MAX_SAMPLE_AGE);
            facts.status.disk_available_bytes = sample.and_then(|sample| sample.available_bytes);
            if facts.disk.is_none() {
                return Ok(());
            }
            sample
        };
        if sample.and_then(|sample| sample.available_bytes).is_none_or(|bytes| bytes < disk::PAUSE_BELOW) {
            self.set_pause(PauseReason::LowDisk, true)?;
        }
        if let (Some(request), Some(sample)) = (self.disk_resume, sample) && sample.started >= request {
            self.disk_resume = None;
            if sample.available_bytes.is_some_and(|bytes| bytes > disk::RESUME_ABOVE) {
                self.set_pause(PauseReason::LowDisk, false)?;
            }
        }
        Ok(())
    }

    fn route_hotkeys(&mut self) -> Result<(), CaptureError> {
        let (lifecycle, hotkeys) = (self.lifecycle, self.hotkeys);
        let batch = hotkeys.drain();
        let before = batch.before;
        let loss = batch.loss;
        let detected_at = controller_qpc()?;
        // A progress gap over 250 ms, sleep excluded, is a recorded control loss;
        // later activations are processed normally. The suspend sequence was read
        // at the previous drain, before the progress mark, so a missed suspend
        // makes the gap unknown rather than overdue.
        let marks = lifecycle.environment.power.snapshot();
        let since = std::mem::replace(&mut lifecycle.lock().hotkey_suspends, marks.sequence);
        let overdue = before.is_some_and(|last| matches!(
            marks.since(AwakeMark { qpc: last, suspends: since }, detected_at, self.frequency),
            AwakeElapsed::Known(gap) if gap > HOTKEY_PROGRESS_GAP));
        // Only a stopped hotkey thread makes the hotkeys unavailable.
        let failed = batch.failed;
        for activation in batch.into_events() {
            let outcome = if lifecycle.stop_requested() {
                ControlOutcome::AlreadyStopping
            } else {
                match activation.action {
                    RecorderAction::Start => ControlOutcome::AlreadyStartingOrRecording,
                    RecorderAction::TogglePause => {
                        let (available, resumable) = {
                            let facts = lifecycle.lock();
                            let reasons = &facts.pause.reasons;
                            (user_pause_available(facts.status.phase),
                                reasons.contains(&PauseReason::User) || reasons.contains(&PauseReason::LowDisk))
                        };
                        if !available {
                            ControlOutcome::PauseUnavailable
                        } else if resumable {
                            self.user_resume()?;
                            ControlOutcome::ResumeRequested
                        } else {
                            self.set_pause(PauseReason::User, true)?;
                            ControlOutcome::PauseRequested
                        }
                    }
                    RecorderAction::Checkpoint => {
                        if lifecycle.lock().status.phase == SessionPhase::Recording {
                            ControlOutcome::CheckpointRequested
                        } else {
                            ControlOutcome::CheckpointUnavailable
                        }
                    }
                    RecorderAction::Stop => {
                        lifecycle.latch_stop("stop hotkey");
                        ControlOutcome::StopRequested
                    }
                }
            };
            let mut facts = lifecycle.lock();
            facts.hotkeys.push(ControlNotice { activation, outcome });
            facts.status.last_hotkey = Some(outcome);
        }
        let mut facts = lifecycle.lock();
        facts.hotkeys.before = before;
        if let Some(loss) = loss {
            ControlLoss::merge(&mut facts.hotkeys.loss, loss);
        }
        if failed || overdue {
            ControlLoss::merge(&mut facts.hotkeys.loss, ControlLoss {
                first: before.unwrap_or(detected_at), last: detected_at, count: None,
            });
        }
        if failed {
            facts.hotkeys.failed = true;
            facts.status.hotkeys_unavailable = true;
        }
        Ok(())
    }
}

/// Clock and lifecycle errors on the controller path belong to the controller.
fn controller_error(error: ProbeError) -> CaptureError {
    error.to_error(FailureComponent::Controller)
}

fn controller_qpc() -> Result<QpcTick, CaptureError> {
    clock::qpc().map_err(controller_error)
}

fn controller_utc_anchor() -> Result<UtcAnchor, CaptureError> {
    clock::utc_anchor().map_err(controller_error)
}

fn failure(operation: &'static str, message: &'static str) -> CaptureError {
    CaptureError::owned(FailureComponent::Controller, operation, None, message.into())
}
