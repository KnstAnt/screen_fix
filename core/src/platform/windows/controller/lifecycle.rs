//! The lifecycle shared by the controller, the recording workers and the UI
//! handle, and the UI's `Control` over it.

use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};

use crate::{
    capture::save::{FixtureSave, FixtureSaveError, FixtureSaveResult},
    devlog::devlog,
    platform::windows::clock,
    protocol::SaveId,
    session::{AdmissionBoundary, CommandError, PauseReason, SessionCommand, SessionPhase, SessionStatus},
    util::keep_earlier,
};

use super::{
    MAX_FIXTURE_SAVES,
    environment::{EnvironmentSignals, SUSPEND_BUDGET},
    facts::Facts,
    worker::{FixtureNotice, Receipt},
};

const SESSION_END_CONTROLLER_BUDGET: Duration = Duration::from_millis(3750);

/// The frames merge owner's opening endpoint (§6.5). The controller sends
/// each opening boundary with an acknowledgement sender and publishes the
/// boundary to the workers only after the acknowledgement.
pub(in crate::platform::windows) type FramesOpening = SyncSender<(AdmissionBoundary, SyncSender<()>)>;

#[derive(Default)]
pub(super) struct Lifecycle {
    pub(super) facts: Mutex<Facts>,
    pub(super) changed: Condvar,
    stop: AtomicBool,
    pub(super) environment: Arc<EnvironmentSignals>,
    pub(super) journal_flush_revision: AtomicU64,
    pub(super) controller_qpc: AtomicI64,
    pub(super) hotkey_keys: [u16; 4],
    pub(super) frames_opening: OnceLock<FramesOpening>,
}

impl Lifecycle {
    pub(super) fn new(hotkey_keys: [u16; 4]) -> Self {
        Self { hotkey_keys, ..Self::default() }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Facts> {
        self.facts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// The Windows session is ending.
    pub(super) fn session_ending(&self) -> bool {
        self.environment.session_end.get().is_some()
    }

    /// Stop was requested or the Windows session is ending.
    pub(super) fn closing(&self) -> bool {
        self.stop_requested() || self.session_ending()
    }

    /// Latch Stop. True when this call set the latch first.
    pub(super) fn latch_stop(&self, reason: &'static str) -> bool {
        let first = !self.stop.swap(true, Ordering::AcqRel);
        if first {
            devlog!("session", "stop requested: {reason}");
        }
        first
    }

    pub(super) fn apply_session_end_deadlines(&self, facts: &mut Facts) {
        if let Some(&started) = self.environment.session_end.get() {
            let before = (facts.stop_deadline, facts.segment_deadline);
            keep_earlier(&mut facts.stop_deadline, started + SESSION_END_CONTROLLER_BUDGET);
            keep_earlier(&mut facts.segment_deadline, started + SUSPEND_BUDGET);
            // Workers waiting on `changed` re-read their deadline bound.
            if (facts.stop_deadline, facts.segment_deadline) != before {
                self.changed.notify_all();
            }
        }
    }
}

pub(super) fn request_stop(lifecycle: &Lifecycle, wake: &SyncSender<()>, reason: &'static str) {
    lifecycle.latch_stop(reason);
    // A full wake slot already guarantees that the controller wakes up.
    let _ = wake.try_send(());
}

/// A user pause applies only after Start and before Stop, so it never holds the countdown.
pub(super) fn user_pause_available(phase: SessionPhase) -> bool {
    matches!(phase, SessionPhase::Recording | SessionPhase::Paused)
}

#[derive(Clone)]
pub(crate) struct Control {
    pub(super) lifecycle: Arc<Lifecycle>,
    pub(super) commands: SyncSender<SessionCommand>,
    pub(super) wake: SyncSender<()>,
}

impl Control {
    pub(in crate::platform::windows) fn stop_for_indicator_failure(&self) {
        request_stop(&self.lifecycle, &self.wake, "indicator failure");
    }

    pub(crate) fn status(&self) -> Option<SessionStatus> {
        self.lifecycle.facts.try_lock().ok().map(|facts| facts.status_view())
    }

    pub(crate) fn confirm_fixture_save(
        &self, signal: FixtureSave,
    ) -> Result<Receiver<FixtureSaveResult>, FixtureSaveError> {
        let mut facts = self.lifecycle.facts.try_lock().map_err(|_| FixtureSaveError::Busy)?;
        if facts.status.phase != SessionPhase::Recording || self.lifecycle.stop_requested() {
            return Err(FixtureSaveError::AdmissionClosed);
        }
        if facts.fixture_saves.len() >= MAX_FIXTURE_SAVES {
            return Err(FixtureSaveError::Busy);
        }
        let (Some(origin), Some(boundary)) = (facts.bounds.origin, facts.bounds.opened) else {
            return Err(FixtureSaveError::ClockUnavailable);
        };
        let qpc = clock::qpc().map_err(|_| FixtureSaveError::ClockUnavailable)?;
        if qpc < boundary.qpc {
            return Err(FixtureSaveError::ClockUnavailable);
        }
        let next = facts.next_save.checked_add(1).ok_or(FixtureSaveError::ControllerUnavailable)?;
        let (reply, result) = mpsc::sync_channel(1);
        let receipt = Receipt { qpc, clock: origin, boundary, tick_anchor: facts.clock.tick_anchor };
        facts.next_save = next;
        facts.fixture_saves.push_back(FixtureNotice { signal, id: SaveId(next), receipt, reply });
        Ok(result)
    }

    pub(crate) fn submit(&self, command: SessionCommand) -> Result<(), CommandError> {
        if command == SessionCommand::Stop {
            request_stop(&self.lifecycle, &self.wake, "user stop command");
            return Ok(());
        }
        if !matches!(command, SessionCommand::Pause(PauseReason::User) | SessionCommand::Resume(PauseReason::User)) {
            return Err(CommandError::PauseUnavailable);
        }
        // The caller is the UI thread: a contended lock forwards the command,
        // and the controller checks the phase again (`Controller::user_command`).
        if command == SessionCommand::Pause(PauseReason::User)
            && let Ok(facts) = self.lifecycle.facts.try_lock()
            && !user_pause_available(facts.status.phase)
        {
            return Err(CommandError::PauseUnavailable);
        }
        self.commands.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => CommandError::Busy,
            TrySendError::Disconnected(_) => CommandError::ControllerUnavailable,
        })?;
        let _ = self.wake.try_send(());
        Ok(())
    }
}
