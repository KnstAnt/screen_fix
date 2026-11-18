//! The lifecycle transitions: start, pause, resume and stop, the pause prefix
//! that a resume waits for, and the clock anchors sampled while admitting.

use std::{sync::mpsc, time::{Duration, Instant}};

use crate::{
    CaptureError,
    devlog::devlog,
    package::{
        diagnostics::{Failure, FailureComponent},
        metadata::{ClockAnchor, SourceBlindInterval},
        outcome::HealthKind,
    },
    platform::windows::clock,
    protocol::{AdmissionEpoch, ClockOrigin, ClockRangeError, QpcTick},
    session::{AdmissionBoundary, PauseReason, SessionPhase, fault::FaultReport, labels::pause_blind_reason},
    util::keep_earlier,
};

use super::{
    AwakeElapsed, AwakeMark, Controller, controller_error, controller_qpc, controller_utc_anchor, failure,
    facts::{PausePrefix, SuspendBound},
    lifecycle::Lifecycle,
    source::{SelectedSource, clear_source_status, prepare_source, push_source_blind, select_source},
    worker::PauseProducer,
};

const UTC_INTERVAL_SECONDS: i64 = 60;
const STOP_BUDGET: Duration = Duration::from_secs(10);
/// The frames merge owner acknowledges an opening between two merge steps,
/// which never wait for IO; a missing acknowledgement is its failure (§6.5).
const FRAMES_OPENING_ACK: Duration = Duration::from_secs(1);

/// The pause producers: input, stills, and capture and video for each monitor.
const fn pause_producer_count(monitors: usize) -> usize {
    monitors * 2 + 2
}

impl Controller<'_> {
    /// Assert or clear one pause reason. The first reason pauses a recording
    /// at a new cutoff; a reason that is not the elevation transition also
    /// cancels the source preparation.
    pub(super) fn set_pause(&self, reason: PauseReason, active: bool) -> Result<(), CaptureError> {
        let lifecycle = self.lifecycle;
        let mut facts = lifecycle.lock();
        let changed = if active { facts.pause.reasons.insert(reason) } else { facts.pause.reasons.remove(&reason) };
        if !changed {
            return Ok(());
        }
        if active && reason != PauseReason::ElevationTransition {
            facts.source.cancel_preparation();
            if facts.status.phase != SessionPhase::Recording { clear_source_status(&mut facts); }
        }
        if facts.status.phase == SessionPhase::Paused {
            let pause = &mut facts.pause;
            pause.prefix.reasons.extend(pause.reasons.iter().copied());
        } else if facts.status.phase == SessionPhase::Recording && !facts.pause.reasons.is_empty()
            && let (Some(origin), Some(opened)) = (facts.bounds.origin, facts.bounds.opened)
        {
            let closed = boundary(origin, controller_qpc()?, opened.epoch)?;
            let anchor = controller_utc_anchor()?;
            facts.bounds.closed = Some(closed);
            facts.clock.utc_anchors.push(ClockAnchor { admission_epoch: closed.epoch, sample: anchor });
            facts.status.phase = SessionPhase::Paused;
            clear_source_status(&mut facts);
            facts.pause.prefix = PausePrefix { reasons: facts.pause.reasons.clone(), ..PausePrefix::default() };
            let started = controller_qpc()?;
            facts.pause.prefix.started = Some(AwakeMark { qpc: started, suspends: lifecycle.environment.power.snapshot().sequence });
            devlog!("session", "phase Recording -> Paused closed={closed:?} reasons={:?}", facts.pause.reasons);
            lifecycle.changed.notify_all();
        }
        Ok(())
    }

    /// Follow the producers' pause prefixes. A suspend wait ends once every video
    /// producer has finished, or once the system has resumed, with one health row
    /// for each monitor whose video prefix is still open. `pause_finished`
    /// receives the pause cutoff once every producer has finished.
    pub(super) fn track_pause_prefix(&mut self) {
        let session = self.session;
        let monitors = session.config().monitors();
        let mut facts = self.lifecycle.lock();
        if facts.status.phase != SessionPhase::Paused {
            return;
        }
        let finished = &facts.pause.prefix.finished;
        if finished.iter().filter(|producer| matches!(producer, PauseProducer::Video(_))).count() == monitors.len() {
            facts.pause.suspend = SuspendBound::default();
        } else if let Some(sequence) = facts.pause.suspend.wait {
            // The suspend window cannot be measured while the suspend is
            // open. A video prefix still open when the system has
            // resumed missed it: one G row per late monitor.
            let marks = session.suspend_marks();
            if marks.sequence != sequence || marks.resume.is_some() {
                facts.pause.suspend.wait = None;
                let late: Vec<_> = monitors.iter().map(|monitor| monitor.id)
                    .filter(|id| !facts.pause.prefix.finished.contains(&PauseProducer::Video(*id))).collect();
                for monitor in late {
                    facts.push_health(HealthKind::SuspendFinalizeLate,
                        Failure::new(FailureComponent::Video, "Finish the suspended video segment", None), Some(monitor), false);
                }
            }
        }
        if facts.pause.prefix.finished.len() == pause_producer_count(monitors.len()) {
            self.pause_finished = facts.bounds.closed;
            facts.pause.prefix.started = None;
        }
    }

    /// The pause prefix has stayed open for the Stop budget, sleep excluded.
    pub(super) fn pause_overdue(&self) -> Result<bool, CaptureError> {
        let facts = self.lifecycle.lock();
        let now = controller_qpc()?;
        Ok(facts.pause.prefix.started.is_some_and(|started| matches!(
            self.session.suspend_marks().since(started, now, self.frequency),
            AwakeElapsed::Known(elapsed) if elapsed >= STOP_BUDGET)))
    }

    /// Reopen admission after a finished pause: at once when no reason remains, or
    /// with the newly selected source when only the elevation transition holds the
    /// pause. True when the pause was handed to `resume`.
    pub(super) fn reopen_finished_pause(&self, handoff: bool, finished: AdmissionBoundary) -> Result<bool, CaptureError> {
        if !handoff {
            if !self.lifecycle.lock().pause.reasons.is_empty() {
                return Ok(false);
            }
            self.resume(finished, None)?;
            return Ok(true);
        }
        let selected = {
            let mut facts = self.lifecycle.lock();
            if !facts.pause.reasons.iter().all(|reason| *reason == PauseReason::ElevationTransition) {
                return Ok(false);
            }
            prepare_source(&mut facts)?;
            facts.source.ready.take()
        };
        let Some(selected) = selected.filter(SelectedSource::current) else {
            return Ok(false);
        };
        self.set_pause(PauseReason::ElevationTransition, false)?;
        self.resume(finished, Some(selected))?;
        Ok(true)
    }

    /// `Starting` → `Recording`: sample the clock origin and open the first epoch.
    pub(super) fn start(&self, selected: Option<SelectedSource>) -> Result<(), CaptureError> {
        let lifecycle = self.lifecycle;
        if lifecycle.stop_requested() {
            return Ok(());
        }
        let anchor = controller_utc_anchor()?;
        let origin = ClockOrigin { origin_qpc: anchor.qpc, qpc_frequency: self.frequency };
        let epoch = selected.as_ref().map_or(AdmissionEpoch(1), |source| source.epoch);
        let boundary = boundary(origin, anchor.qpc, epoch)?;
        let tick_anchor = clock::message_tick_anchor().map_err(controller_error)?;
        open_frames(lifecycle, boundary)?;
        let mut facts = lifecycle.lock();
        if lifecycle.closing() {
            return Ok(());
        }
        facts.clock.tick_anchor = Some(tick_anchor);
        facts.clock.utc_anchors.push(ClockAnchor { admission_epoch: boundary.epoch, sample: anchor });
        facts.bounds.origin = Some(origin);
        facts.bounds.opened = Some(boundary);
        select_source(&mut facts, epoch, selected);
        facts.status.phase = SessionPhase::Recording;
        facts.status.countdown_seconds = None;
        devlog!("session", "phase Starting -> Recording opened={boundary:?} qpc_frequency={}", self.frequency.0);
        lifecycle.changed.notify_all();
        Ok(())
    }

    /// `Paused` → `Recording` at a new epoch, once every producer finished the
    /// pause at `finished`.
    fn resume(&self, finished: AdmissionBoundary, selected: Option<SelectedSource>) -> Result<(), CaptureError> {
        let lifecycle = self.lifecycle;
        let facts = lifecycle.lock();
        if lifecycle.stop_requested() || facts.status.phase != SessionPhase::Paused || facts.bounds.closed != Some(finished) {
            return Ok(());
        }
        let origin = facts.bounds.origin
            .ok_or_else(|| failure("Resume recording", "The session clock is missing."))?;
        let epoch = match selected.as_ref() {
            Some(source) => source.epoch,
            None => facts.source.epoch.checked_add(1).map(AdmissionEpoch)
                .ok_or_else(|| failure("Resume recording", "The admission epoch overflowed."))?,
        };
        let resume_anchor = controller_utc_anchor()?;
        let tick_anchor = clock::message_tick_anchor().map_err(controller_error)?;
        let reopened = boundary(origin, resume_anchor.qpc, epoch)?;
        drop(facts);
        open_frames(lifecycle, reopened)?;
        let mut facts = lifecycle.lock();
        if lifecycle.stop_requested() || facts.status.phase != SessionPhase::Paused || facts.bounds.closed != Some(finished) {
            return Ok(());
        }
        if facts.pause.prefix.reasons.contains(&PauseReason::ElevationTransition) {
            let reason = pause_blind_reason(&facts.pause.prefix.reasons);
            push_source_blind(&mut facts, SourceBlindInterval {
                epoch: finished.epoch, start_t_us: finished.time, before_t_us: reopened.time, reason,
            });
        }
        facts.bounds.opened = Some(reopened);
        select_source(&mut facts, epoch, selected);
        facts.bounds.closed = None;
        facts.status.phase = SessionPhase::Recording;
        facts.pause.suspend = SuspendBound::default();
        facts.pause.last = Some((finished, facts.pause.prefix.reasons.iter().copied().collect()));
        facts.clock.utc_anchors.push(ClockAnchor { admission_epoch: epoch, sample: resume_anchor });
        facts.clock.tick_anchor = Some(tick_anchor);
        devlog!("session", "phase Paused -> Recording finished={finished:?} opened={reopened:?}");
        lifecycle.changed.notify_all();
        Ok(())
    }
}

/// `Starting | Recording | Paused` → `Stopping`: close admission at the Stop
/// boundary and bound finalization by the Stop budget.
pub(super) fn stop(lifecycle: &Lifecycle) -> Result<(), CaptureError> {
    let mut facts = lifecycle.lock();
    lifecycle.apply_session_end_deadlines(&mut facts);
    if facts.status.phase == SessionPhase::Stopping {
        return Ok(());
    }
    let was_recording = facts.status.phase == SessionPhase::Recording;
    let now = Instant::now();
    keep_earlier(&mut facts.stop_deadline, now + STOP_BUDGET);
    keep_earlier(&mut facts.segment_deadline, now + STOP_BUDGET);
    facts.status.phase = SessionPhase::Stopping;
    if facts.bounds.origin.is_none() { clear_source_status(&mut facts); }
    facts.source.cancel_preparation();
    facts.status.countdown_seconds = None;
    let sampled = (|| {
        if let (Some(origin), Some(opened)) = (facts.bounds.origin, facts.bounds.opened) {
            let stopped = boundary(origin, controller_qpc()?, opened.epoch)?;
            facts.bounds.stop = Some(stopped);
            if was_recording {
                facts.bounds.closed = Some(stopped);
                facts.pause.prefix.input = None;
            } else if let Some(closed) = facts.bounds.closed
                && facts.pause.prefix.reasons.contains(&PauseReason::ElevationTransition)
            {
                let reason = pause_blind_reason(&facts.pause.prefix.reasons);
                push_source_blind(&mut facts, SourceBlindInterval {
                    epoch: closed.epoch, start_t_us: closed.time, before_t_us: stopped.time, reason,
                });
            }
            facts.clock.utc_anchors.push(ClockAnchor { admission_epoch: stopped.epoch, sample: controller_utc_anchor()? });
        }
        Ok::<(), CaptureError>(())
    })();
    if let Err(error) = &sampled {
        facts.clock.failed = true;
        facts.record_fault(FaultReport::attempt(error));
    }
    devlog!("session", "phase -> Stopping was_recording={was_recording} stop={:?} closed={:?} sampled={}",
        facts.bounds.stop, facts.bounds.closed, sampled.is_ok());
    lifecycle.changed.notify_all();
    sampled
}

/// Opening(e, R) (§6.5): the frames merge owner sets every monitor's floor at
/// R before any video worker can observe epoch e. The controller does not
/// hold the lifecycle lock while it waits.
fn open_frames(lifecycle: &Lifecycle, opening: AdmissionBoundary) -> Result<(), CaptureError> {
    let lost = || CaptureError::owned(FailureComponent::Journal, "Open the frames journal epoch", None,
        "The frames merge owner did not acknowledge the opening boundary.".into());
    let owner = lifecycle.frames_opening.get().ok_or_else(lost)?;
    let (ack, acknowledged) = mpsc::sync_channel(1);
    owner.try_send((opening, ack)).map_err(|_| lost())?;
    acknowledged.recv_timeout(FRAMES_OPENING_ACK).map_err(|_| lost())
}

/// Sample a message tick anchor every second and a UTC anchor every minute
/// while admission is open or paused.
pub(super) fn sample_anchors(lifecycle: &Lifecycle) -> Result<(), CaptureError> {
    let mut facts = lifecycle.lock();
    if !matches!(facts.status.phase, SessionPhase::Recording | SessionPhase::Paused) {
        return Ok(());
    }
    let (Some(origin), Some(opened)) = (facts.bounds.origin, facts.bounds.opened) else {
        return Ok(());
    };
    let now = controller_qpc()?;
    let ticks_per_second = i128::from(origin.qpc_frequency.0.get());
    if facts.clock.tick_anchor.is_none_or(|anchor| {
        now.ticks_since(anchor.qpc) >= ticks_per_second
    }) {
        facts.clock.tick_anchor = Some(clock::message_tick_anchor().map_err(controller_error)?);
    }
    if facts.clock.utc_anchors.last().is_none_or(|anchor| {
        now.ticks_since(anchor.sample.qpc) >= ticks_per_second * i128::from(UTC_INTERVAL_SECONDS)
    }) {
        facts.clock.utc_anchors.push(ClockAnchor { admission_epoch: opened.epoch, sample: controller_utc_anchor()? });
    }
    Ok(())
}

fn boundary(origin: ClockOrigin, qpc: QpcTick, epoch: AdmissionEpoch) -> Result<AdmissionBoundary, CaptureError> {
    Ok(AdmissionBoundary { epoch, qpc, time: origin.time(qpc).map_err(timing_error)? })
}

fn timing_error(error: ClockRangeError) -> CaptureError {
    CaptureError::owned(FailureComponent::Controller, "Convert the session timestamp", None, error.to_string())
}
