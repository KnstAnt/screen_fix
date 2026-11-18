//! `WorkerSession`, the recording workers' view of the lifecycle, and the
//! recording worker thread. The source methods are in `source.rs`, the
//! startup admission check in `startup.rs`.

use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, PoisonError,
        atomic::Ordering,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use screen_fix_native_observation::StaticLease;
use screen_fix_protocol::ipc::InputRegistration;

use crate::{
    CaptureError,
    capture::save::{FixtureSave, FixtureSaveResult},
    config::ResolvedCaptureConfig,
    package::{
        diagnostics::{Failure, FailureComponent, ReportOutcome},
        journal::HealthRecord,
        metadata::{AttemptClock, InputBlindReason, LostActivity},
        outcome::HealthKind,
    },
    platform::windows::{ProbeError, clock, unavailable},
    protocol::{ClockOrigin, MessageTickAnchor, MonitorId, QpcTick, SaveId, SessionTimeUs},
    session::{
        AdmissionBoundary, PauseReason, ReadyVideo, SessionPhase, SessionResult, StartupStep, VideoReport,
        WindowCoverage,
        fault::{FaultClass, FaultOutcome, FaultReport},
    },
};

use super::{
    MAX_INPUT_FAILURES, SuspendMarks, disk, failure,
    facts::{CompletePublication, ControlPrefix, Facts},
    lifecycle::{FramesOpening, Lifecycle, request_stop},
};

#[derive(Clone)]
pub(crate) struct WorkerSession {
    pub(super) lifecycle: Arc<Lifecycle>,
    pub(super) config: Arc<ResolvedCaptureConfig>,
    pub(super) ready: SyncSender<()>,
    pub(super) wake: SyncSender<()>,
}

#[derive(Clone, Copy)]
pub(in crate::platform::windows) struct RecordingStart {
    pub clock: ClockOrigin,
    pub boundary: AdmissionBoundary,
}

#[derive(Clone, Copy)]
pub(in crate::platform::windows) struct Receipt {
    pub qpc: QpcTick,
    pub clock: ClockOrigin,
    pub boundary: AdmissionBoundary,
    pub tick_anchor: Option<MessageTickAnchor>,
}

#[derive(Clone, Copy)]
pub(crate) struct VideoFailure {
    pub monitor: MonitorId,
    pub at: Option<SessionTimeUs>,
    pub failure: Failure,
}

#[derive(Clone, Copy)]
pub(crate) struct InputFailure {
    pub operation: &'static str,
    /// The detection receipt.
    pub at: SessionTimeUs,
    /// The earliest missing receipt: the start of the blind interval.
    pub first_missing: SessionTimeUs,
    pub failure: Failure,
    pub reason: InputBlindReason,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::platform::windows) enum PauseProducer {
    Input,
    Capture(MonitorId),
    Video(MonitorId),
    Stills,
}

#[derive(Clone, Copy, serde::Serialize)]
pub(in crate::platform::windows) struct PausedVideo {
    pub monitor: MonitorId,
    pub first_paused_slot: u64,
    pub fps: u32,
}

impl PausedVideo {
    pub(in crate::platform::windows) fn end_exclusive(self, closed: SessionTimeUs, next: SessionTimeUs) -> Option<u64> {
        let us = next.0.checked_sub(closed.0)?;
        let slots = (i128::from(us) * i128::from(self.fps) + 999_999).div_euclid(1_000_000);
        self.first_paused_slot.checked_add(u64::try_from(slots).ok()?)
    }
}

/// The session deadline that bounds a worker wait.
#[derive(Clone, Copy)]
pub(in crate::platform::windows) enum Budget {
    Stop,
    Segment,
}

impl Budget {
    fn deadline(self, facts: &Facts) -> Option<Instant> {
        match self {
            Self::Stop => facts.stop_deadline,
            Self::Segment => facts.segment_deadline,
        }
    }
}

pub(in crate::platform::windows) enum Admission {
    Starting,
    Open(Receipt),
    Closed(Option<AdmissionBoundary>),
}

pub(in crate::platform::windows) struct FixtureNotice {
    pub signal: FixtureSave,
    pub id: SaveId,
    pub receipt: Receipt,
    pub reply: SyncSender<FixtureSaveResult>,
}

impl WorkerSession {
    /// Wake the controller. A full wake slot already guarantees that it wakes up.
    pub(super) fn wake(&self) {
        let _ = self.wake.try_send(());
    }

    pub(in crate::platform::windows) fn controller_qpc(&self) -> QpcTick {
        QpcTick(self.lifecycle.controller_qpc.load(Ordering::Acquire))
    }

    pub(super) fn disk_sample(&self, sample: disk::Sample) {
        self.lifecycle.lock().disk = Some(sample);
        self.wake();
    }

    pub(in crate::platform::windows) fn stop_requested(&self) -> bool {
        self.lifecycle.stop_requested()
    }

    pub(in crate::platform::windows) fn started(&self) -> bool {
        self.lifecycle.lock().bounds.origin.is_some()
    }

    /// Report an attempt failure or an activity loss. An attempt failure runs the Stop path.
    pub(in crate::platform::windows) fn report_fault(&self, report: FaultReport) {
        let attempt = report.class() == FaultClass::Attempt;
        self.lifecycle.lock().record_fault(report);
        if attempt {
            self.request_stop();
        }
    }

    pub(in crate::platform::windows) fn attempt_failed(&self) -> bool {
        self.lifecycle.lock().reports.faults.attempt_failure().is_some()
    }

    pub(in crate::platform::windows) fn fault_outcome(&self) -> FaultOutcome {
        self.lifecycle.lock().reports.faults.outcome()
    }

    pub(in crate::platform::windows) fn stop_deadline(&self) -> Option<Instant> {
        self.lifecycle.lock().stop_deadline
    }

    /// The latest suspend and resume QPC marks, for excluding sleep from QPC intervals.
    pub(in crate::platform::windows) fn suspend_marks(&self) -> SuspendMarks {
        self.lifecycle.environment.power.snapshot()
    }

    pub(in crate::platform::windows) fn segment_deadline(&self) -> Option<Instant> {
        self.lifecycle.lock().segment_deadline
    }

    /// `local`, or the `budget` deadline when that is earlier.
    pub(in crate::platform::windows) fn bounded(&self, local: Instant, budget: Budget) -> Instant {
        budget.deadline(&self.lifecycle.lock()).map_or(local, |global| global.min(local))
    }

    /// Wait for the next change of the facts while `pending` holds, for at
    /// most `step` and never past `until`. Returns at once when `pending` is
    /// false. The check and the wait share one lock, and every writer a
    /// caller depends on notifies `changed`, so no change is lost; `step`
    /// bounds the wait if one is, and paces callers that also poll sources
    /// outside the facts. The caller re-reads its state after every return.
    fn wait_changed(&self, step: Duration, until: impl FnOnce(&Facts) -> Option<Instant>, pending: impl FnOnce(&Facts) -> bool) {
        let facts = self.lifecycle.lock();
        if !pending(&facts) {
            return;
        }
        let timeout = until(&facts).map_or(step, |until| until.saturating_duration_since(Instant::now()).min(step));
        if !timeout.is_zero() {
            let _ = self.lifecycle.changed.wait_timeout(facts, timeout);
        }
    }

    /// Wait at most `step` for a change while the input prefix through
    /// `cutoff` is open, and never past `local` or the `budget` deadline.
    pub(in crate::platform::windows) fn wait_input_prefix(
        &self, cutoff: AdmissionBoundary, local: Instant, budget: Budget, step: Duration,
    ) {
        self.wait_changed(step, |facts| Some(budget.deadline(facts).map_or(local, |global| global.min(local))),
            |facts| facts.pause.prefix.input != Some(cutoff));
    }

    /// Wait at most `step` for a change while the pause at `cutoff` holds and
    /// some monitor's paused video range is missing.
    pub(in crate::platform::windows) fn wait_paused_video(&self, cutoff: AdmissionBoundary, step: Duration) {
        let monitors = self.config.monitors().len();
        self.wait_changed(step, |_| None,
            |facts| paused_at(facts, cutoff) && facts.pause.prefix.video.len() != monitors);
    }

    /// Wait at most `step` for a change while the pause at `cutoff` holds.
    pub(in crate::platform::windows) fn wait_pause_end(&self, cutoff: AdmissionBoundary, step: Duration) {
        self.wait_changed(step, |_| None, |facts| paused_at(facts, cutoff));
    }

    /// The bound on the elevated collector's environment cleanup: the Stop or
    /// session-end deadline, or the suspend budget.
    pub(in crate::platform::windows) fn cleanup_deadline(&self) -> Option<Instant> {
        let facts = self.lifecycle.lock();
        facts.segment_deadline.into_iter().chain(facts.pause.suspend.cleanup).min()
    }

    pub(in crate::platform::windows) fn pause_interval_reasons(&self, cutoff: AdmissionBoundary) -> Vec<PauseReason> {
        let facts = self.lifecycle.lock();
        if let Some((closed, reasons)) = &facts.pause.last && *closed == cutoff {
            return reasons.clone();
        }
        facts.pause.prefix.reasons.iter().copied().collect()
    }

    pub(in crate::platform::windows) fn wait_for_stop(&self) -> Result<Option<AdmissionBoundary>, ProbeError> {
        let facts = self.lifecycle.changed.wait_while(self.lifecycle.lock(), |facts| {
            matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Recording | SessionPhase::Paused)
        }).unwrap_or_else(PoisonError::into_inner);
        facts.check_clock()?;
        Ok(facts.bounds.closed)
    }

    pub(in crate::platform::windows) fn mark_input_prefix(&self, cutoff: AdmissionBoundary) {
        let mut facts = self.lifecycle.lock();
        if facts.bounds.closed == Some(cutoff) {
            facts.pause.prefix.input = Some(cutoff);
            self.lifecycle.changed.notify_all();
        }
    }

    pub(in crate::platform::windows) fn input_prefix_closed(&self, cutoff: AdmissionBoundary) -> bool {
        self.lifecycle.lock().pause.prefix.input == Some(cutoff)
    }

    pub(in crate::platform::windows) fn paused_cutoff(&self) -> Option<AdmissionBoundary> {
        let facts = self.lifecycle.lock();
        (facts.status.phase == SessionPhase::Paused).then_some(facts.bounds.closed).flatten()
    }

    pub(in crate::platform::windows) fn pause_pending(&self, cutoff: AdmissionBoundary) -> bool {
        paused_at(&self.lifecycle.lock(), cutoff)
    }

    pub(in crate::platform::windows) fn paused_video(&self) -> Vec<PausedVideo> {
        self.lifecycle.lock().pause.prefix.video.values().copied().collect()
    }

    pub(in crate::platform::windows) fn finish_video_pause(&self, cutoff: AdmissionBoundary, video: PausedVideo) {
        let mut facts = self.lifecycle.lock();
        if facts.bounds.closed == Some(cutoff) {
            facts.pause.prefix.video.insert(video.monitor, video);
            if facts.status.phase == SessionPhase::Paused {
                facts.pause.prefix.finished.insert(PauseProducer::Video(video.monitor));
            }
            self.lifecycle.changed.notify_all();
            self.wake();
        }
    }

    pub(in crate::platform::windows) fn finish_pause(&self, cutoff: AdmissionBoundary, producer: PauseProducer) {
        let mut facts = self.lifecycle.lock();
        if facts.status.phase == SessionPhase::Paused && facts.bounds.closed == Some(cutoff) {
            facts.pause.prefix.finished.insert(producer);
            self.wake();
        }
    }

    pub(in crate::platform::windows) fn wait_for_reopen(&self, cutoff: AdmissionBoundary) -> Result<Option<RecordingStart>, ProbeError> {
        let facts = self.lifecycle.changed.wait_while(self.lifecycle.lock(), |facts| paused_at(facts, cutoff))
            .unwrap_or_else(PoisonError::into_inner);
        reopened(&facts, cutoff)
    }

    /// `wait_for_reopen` bounded by `timeout`: `None` while still paused at `cutoff`.
    pub(in crate::platform::windows) fn wait_for_reopen_timeout(
        &self, cutoff: AdmissionBoundary, timeout: Duration,
    ) -> Result<Option<Option<RecordingStart>>, ProbeError> {
        let (facts, _) = self.lifecycle.changed.wait_timeout_while(self.lifecycle.lock(), timeout, |facts| paused_at(facts, cutoff))
            .unwrap_or_else(PoisonError::into_inner);
        if paused_at(&facts, cutoff) {
            return Ok(None);
        }
        reopened(&facts, cutoff).map(Some)
    }

    pub(in crate::platform::windows) fn stop_boundary(&self) -> Option<AdmissionBoundary> {
        self.lifecycle.lock().bounds.stop
    }

    pub(in crate::platform::windows) fn take_fixture_saves(&self) -> VecDeque<FixtureNotice> {
        std::mem::take(&mut self.lifecycle.lock().fixture_saves)
    }

    pub(in crate::platform::windows) fn window_coverage(&self, coverage: WindowCoverage) {
        self.lifecycle.lock().status.window_coverage = Some(coverage);
    }

    pub(in crate::platform::windows) fn startup_step(&self, step: StartupStep) {
        self.lifecycle.lock().status.startup = step;
    }

    pub(in crate::platform::windows) fn video_ready(&self, video: ReadyVideo) {
        let mut facts = self.lifecycle.lock();
        let ready = &mut facts.status.ready_video;
        if let Some(previous) = ready.iter_mut().find(|item| item.monitor == video.monitor) {
            *previous = video;
        } else {
            ready.push(video);
            ready.sort_unstable_by_key(|item| item.monitor);
        }
    }

    pub(in crate::platform::windows) fn input_ready(&self, registration: InputRegistration, required_ready: bool) {
        let mut facts = self.lifecycle.lock();
        facts.status.input_ready = Some(registration);
        facts.status.input_helper_ready = required_ready;
    }

    pub(in crate::platform::windows) fn input_registration(&self) -> Option<InputRegistration> {
        self.lifecycle.lock().status.input_ready
    }

    pub(in crate::platform::windows) fn windows_evidence(&self, evidence: crate::platform::WindowsEvidence) {
        self.lifecycle.lock().status.windows = Some(evidence);
    }

    pub(in crate::platform::windows) fn config(&self) -> &ResolvedCaptureConfig {
        &self.config
    }

    pub(in crate::platform::windows) fn ready(&self) -> Result<(), ProbeError> {
        self.ready.try_send(())
            .map_err(|_| unavailable("The session controller did not accept readiness"))?;
        self.wake();
        Ok(())
    }

    /// The worker reports this after its local producers and journals have
    /// finished the admitted prefix. The publication decision reads the same fact.
    pub(in crate::platform::windows) fn complete_prefix(&self, cutoff: AdmissionBoundary) -> Result<(), ProbeError> {
        let mut facts = self.lifecycle.lock();
        if facts.status.phase != SessionPhase::Stopping || facts.bounds.closed != Some(cutoff) {
            return Err(unavailable("The session controller did not accept the closed producer prefix"));
        }
        facts.completed_prefix = Some(cutoff);
        Ok(())
    }

    pub(in crate::platform::windows) fn authorize_complete_publication(&self) -> io::Result<()> {
        let mut facts = self.lifecycle.lock();
        match facts.complete_publication {
            CompletePublication::Awaiting => {}
            CompletePublication::Expired => return Err(publication_timeout()),
            CompletePublication::Authorized => return Err(io::Error::other("Complete publication was already authorized")),
        }
        if facts.clock.failed {
            return Err(io::Error::other("The session clock or lifecycle controller failed"));
        }
        if let Some(fault) = facts.reports.faults.attempt_failure() {
            return Err(io::Error::other(fault.operation()));
        }
        if facts.status.phase != SessionPhase::Stopping || facts.bounds.stop.is_none()
            || facts.bounds.closed.is_none() || facts.completed_prefix != facts.bounds.closed
        {
            return Err(io::Error::other("The recording worker did not finish the admitted producer prefixes"));
        }
        // Read the notification timestamp directly at admission, even if the
        // controller has yet to observe the wake. A notification published after
        // this decision can shorten later waits, but cannot revoke authorization.
        let now = Instant::now();
        self.lifecycle.apply_session_end_deadlines(&mut facts);
        let Some(deadline) = facts.stop_deadline else {
            return Err(io::Error::other("The recording Stop deadline is missing"));
        };
        if now >= deadline {
            facts.complete_publication = CompletePublication::Expired;
            return Err(publication_timeout());
        }
        // The Windows replacement runs after this guard is released. Neither
        // the normal timeout nor a later session-end deadline revokes admission.
        facts.complete_publication = CompletePublication::Authorized;
        Ok(())
    }

    pub(in crate::platform::windows) fn request_stop(&self) {
        request_stop(&self.lifecycle, &self.wake, "worker request or attempt failure");
    }

    pub(in crate::platform::windows) fn wait_for_start(&self) -> Result<Option<RecordingStart>, ProbeError> {
        let facts = self.lifecycle.changed.wait_while(self.lifecycle.lock(), |facts| {
            facts.status.phase == SessionPhase::Starting
        }).unwrap_or_else(PoisonError::into_inner);
        facts.check_clock()?;
        match (facts.bounds.origin, facts.bounds.opened) {
            (Some(clock), Some(boundary)) => Ok(Some(RecordingStart { clock, boundary })),
            _ => Ok(None),
        }
    }

    /// Receipt and boundary publication share the same short critical section.
    pub(in crate::platform::windows) fn admit(&self) -> Result<Admission, ProbeError> {
        let facts = self.lifecycle.lock();
        facts.check_clock()?;
        match facts.status.phase {
            SessionPhase::Starting => Ok(Admission::Starting),
            SessionPhase::Recording => {
                let (Some(clock), Some(boundary)) = (facts.bounds.origin, facts.bounds.opened) else {
                    return Err(unavailable("The recording admission has no clock or opening boundary"));
                };
                let qpc = clock::qpc()?;
                if qpc < boundary.qpc {
                    return Err(unavailable("QPC moved backward before the opening boundary"));
                }
                Ok(Admission::Open(Receipt { qpc, clock, boundary, tick_anchor: facts.clock.tick_anchor }))
            }
            _ => Ok(Admission::Closed(facts.bounds.closed)),
        }
    }

    /// The input pump uses the closed epoch to convert delayed WinEvent
    /// receipts without reopening normal admission.
    pub(in crate::platform::windows) fn foreground_closing_receipt(&self, cutoff: AdmissionBoundary) -> Result<Receipt, ProbeError> {
        let facts = self.lifecycle.lock();
        facts.check_clock()?;
        let bounds = &facts.bounds;
        let (Some(clock), Some(boundary)) = (bounds.origin, bounds.opened) else {
            return Err(unavailable("The foreground prefix has no clock or opening boundary"));
        };
        if bounds.closed != Some(cutoff) || boundary.epoch != cutoff.epoch || boundary.qpc > cutoff.qpc {
            return Err(unavailable("The foreground prefix does not match the closed epoch"));
        }
        Ok(Receipt { qpc: cutoff.qpc, clock, boundary, tick_anchor: facts.clock.tick_anchor })
    }

    /// A bounded wait between video pacing or input polling iterations.
    pub(in crate::platform::windows) fn wait_for_close(&self, timeout: Duration) -> Result<Option<AdmissionBoundary>, ProbeError> {
        let (facts, _) = self.lifecycle.changed.wait_timeout_while(self.lifecycle.lock(), timeout, |facts| {
            matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Recording)
        }).unwrap_or_else(PoisonError::into_inner);
        facts.check_clock()?;
        if matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Recording) {
            return Ok(None);
        }
        Ok(facts.bounds.closed)
    }

    pub(in crate::platform::windows) fn session_time(&self) -> Option<SessionTimeUs> {
        self.lifecycle.lock().now()
    }

    pub(in crate::platform::windows) fn video_failed(&self, monitor: MonitorId, failure: Failure) -> Option<VideoFailure> {
        let mut facts = self.lifecycle.lock();
        if facts.status.video_unavailable.contains(&monitor) {
            return None;
        }
        let at = facts.now();
        let entry = VideoFailure { monitor, at, failure };
        facts.reports.video_failures.push(entry);
        facts.status.video_unavailable.push(monitor);
        facts.record_fault(FaultReport::activity(LostActivity::Required, failure, Some(monitor), at));
        Some(entry)
    }

    pub(in crate::platform::windows) fn set_video_report(&self, monitor: MonitorId, outcome: ReportOutcome) {
        let mut facts = self.lifecycle.lock();
        if facts.status.video_unavailable.contains(&monitor)
            && !facts.status.video_reports.iter().any(|report| report.monitor == monitor)
        {
            facts.status.video_reports.push(VideoReport { monitor, outcome });
        }
    }

    pub(in crate::platform::windows) fn video_reports(&self) -> Vec<VideoReport> {
        self.lifecycle.lock().status.video_reports.clone()
    }

    /// A capture or video health condition written to the context stream when
    /// it occurs (CTX-7). Before the session clock exists it stays only in the
    /// manifest. Conditions beyond the bound are counted as `omitted_health`.
    pub(in crate::platform::windows) fn report_health(
        &self, kind: HealthKind, failure: Failure, monitor: Option<MonitorId>, ongoing: bool,
    ) {
        self.lifecycle.lock().push_health(kind, failure, monitor, ongoing);
    }

    pub(in crate::platform::windows) fn omitted_health(&self) -> u64 {
        self.lifecycle.lock().reports.omitted_health
    }

    pub(in crate::platform::windows) fn health_after(&self, prefix: usize) -> Vec<HealthRecord> {
        self.lifecycle.lock().reports.health.get(prefix..).unwrap_or_default().to_vec()
    }

    pub(in crate::platform::windows) fn video_failures_after(&self, prefix: usize) -> Vec<VideoFailure> {
        self.lifecycle.lock().reports.video_failures.get(prefix..).unwrap_or_default().to_vec()
    }

    pub(in crate::platform::windows) fn input_failed(&self, failure: InputFailure) -> bool {
        let mut facts = self.lifecycle.lock();
        let failures = &facts.reports.input_failures;
        if failures.len() >= MAX_INPUT_FAILURES || failures.iter().any(|old| old.operation == failure.operation) {
            return false;
        }
        facts.reports.input_failures.push(failure);
        facts.record_fault(FaultReport::activity(LostActivity::Required, failure.failure, None, Some(failure.first_missing)));
        facts.status.input_unavailable = Some(failure.operation);
        facts.status.input_report = None;
        true
    }

    pub(in crate::platform::windows) fn input_failure(&self) -> Option<InputFailure> {
        self.lifecycle.lock().reports.input_failures.first().copied()
    }

    pub(in crate::platform::windows) fn input_failures(&self) -> Vec<InputFailure> {
        self.lifecycle.lock().reports.input_failures.clone()
    }

    pub(in crate::platform::windows) fn set_input_report_for(&self, failure: InputFailure, report: ReportOutcome) {
        let mut facts = self.lifecycle.lock();
        if facts.reports.input_failures.last()
            .is_some_and(|latest| latest.operation == failure.operation && latest.at == failure.at)
        {
            facts.status.input_report = Some(report);
        }
    }

    pub(in crate::platform::windows) fn set_input_report(&self, report: ReportOutcome) {
        self.lifecycle.lock().status.input_report = Some(report);
    }

    pub(in crate::platform::windows) fn input_report(&self) -> Option<ReportOutcome> {
        self.lifecycle.lock().status.input_report.clone()
    }

    pub(in crate::platform::windows) fn hotkey_keys(&self) -> [u16; 4] { self.lifecycle.hotkey_keys }

    pub(in crate::platform::windows) fn take_hotkeys(&self) -> ControlPrefix {
        self.lifecycle.lock().hotkeys.take()
    }

    /// Register the frames merge owner before the controller can start.
    pub(in crate::platform::windows) fn register_frames(&self, opening: FramesOpening) -> Result<(), ProbeError> {
        self.lifecycle.frames_opening.set(opening)
            .map_err(|_| unavailable("The frames merge owner was already registered"))
    }

    pub(in crate::platform::windows) fn request_journal_flush(&self) {
        self.lifecycle.journal_flush_revision.fetch_add(1, Ordering::Release);
    }

    pub(in crate::platform::windows) fn journal_flush_revision(&self) -> u64 {
        self.lifecycle.journal_flush_revision.load(Ordering::Acquire)
    }

    pub(in crate::platform::windows) fn display_revision(&self) -> u32 {
        self.lifecycle.environment.display_revision.load(Ordering::Acquire)
    }

    /// Ask the standard helper for a fresh registration check; its next
    /// environment record attests the result (E4).
    pub(in crate::platform::windows) fn request_input_check(&self) {
        self.lifecycle.environment.input
            .fetch_or(crate::protocol::ipc::EnvironmentChange::Settings.bit(), Ordering::AcqRel);
    }

    pub(in crate::platform::windows) fn take_input_environment(&self) -> u32 {
        self.lifecycle.environment.input.swap(0, Ordering::AcqRel)
    }

    pub(in crate::platform::windows) fn take_elevated_environment(&self) -> u32 {
        self.lifecycle.environment.elevated.swap(0, Ordering::AcqRel)
    }

    pub(in crate::platform::windows) fn attempt_clock(&self) -> Option<AttemptClock> {
        let facts = self.lifecycle.lock();
        Some(AttemptClock {
            origin: facts.bounds.origin?,
            utc_anchors: facts.clock.utc_anchors.clone(),
            stop: facts.bounds.stop,
            last_admitted_boundary: facts.bounds.closed,
        })
    }
}

/// The session is paused at `cutoff`.
fn paused_at(facts: &Facts, cutoff: AdmissionBoundary) -> bool {
    facts.status.phase == SessionPhase::Paused && facts.bounds.closed == Some(cutoff)
}

/// The opening that ended the pause at `cutoff`, if the session reopened.
fn reopened(facts: &Facts, cutoff: AdmissionBoundary) -> Result<Option<RecordingStart>, ProbeError> {
    facts.check_clock()?;
    match (facts.status.phase, facts.bounds.origin, facts.bounds.opened) {
        (SessionPhase::Recording | SessionPhase::Paused | SessionPhase::Stopping | SessionPhase::Failed,
            Some(clock), Some(boundary))
            if boundary.epoch != cutoff.epoch => Ok(Some(RecordingStart { clock, boundary })),
        _ => Ok(None),
    }
}

fn publication_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "The recording deadline expired before Complete publication")
}

/// The recording worker's finish time and result.
pub(super) type WorkerOutcome = (Instant, Result<SessionResult, CaptureError>);

/// The recording worker as the controller last observed it.
enum WorkerState {
    Running,
    /// The worker sent this outcome, or exited without one (`None`).
    Exited(Option<WorkerOutcome>),
}

/// The recording worker thread. It sends its outcome before it wakes the
/// controller, so the wake that follows its exit always finds the outcome.
pub(super) struct RecordingWorker {
    thread: thread::JoinHandle<()>,
    outcome: Receiver<WorkerOutcome>,
    state: WorkerState,
}

/// Sends the worker's outcome and wakes the controller when the worker thread
/// exits, including by a panic, which leaves no outcome.
struct WorkerExit {
    outcome: Option<SyncSender<WorkerOutcome>>,
    wake: SyncSender<()>,
}

impl WorkerExit {
    fn finish(mut self, outcome: WorkerOutcome) {
        if let Some(sender) = self.outcome.take() {
            let _ = sender.send(outcome);
        }
    }
}

impl Drop for WorkerExit {
    fn drop(&mut self) {
        // Disconnect before the wake, so a panicked worker reads as finished.
        drop(self.outcome.take());
        // A full wake slot already guarantees that the controller wakes up.
        let _ = self.wake.try_send(());
    }
}

impl RecordingWorker {
    pub(super) fn spawn(
        endpoint: WorkerSession, record: fn(&WorkerSession) -> Result<SessionResult, CaptureError>, lease: StaticLease,
    ) -> Result<Self, CaptureError> {
        let (sender, outcome) = mpsc::sync_channel(1);
        let thread = thread::Builder::new().name("video-recording".into())
            .spawn(move || {
                // The worker and all nested owners hold the lease until the thread exits.
                let _lease = lease;
                let exit = WorkerExit { outcome: Some(sender), wake: endpoint.wake.clone() };
                let result = record(&endpoint);
                exit.finish((Instant::now(), result));
            })
            .map_err(|error| CaptureError::owned(FailureComponent::Controller, "Start the recording worker", None, error.to_string()))?;
        Ok(Self { thread, outcome, state: WorkerState::Running })
    }

    /// The worker has sent its outcome or exited without one.
    pub(super) fn finished(&mut self) -> bool {
        if matches!(self.state, WorkerState::Running) {
            match self.outcome.try_recv() {
                Ok(outcome) => self.state = WorkerState::Exited(Some(outcome)),
                Err(mpsc::TryRecvError::Disconnected) => self.state = WorkerState::Exited(None),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        matches!(self.state, WorkerState::Exited(_))
    }

    /// Wait for the worker thread to exit, which releases the recording lease,
    /// and take its outcome.
    pub(super) fn join(self) -> Result<WorkerOutcome, CaptureError> {
        let Self { thread, outcome, state } = self;
        let exited = thread.join().is_ok();
        let received = match state {
            WorkerState::Exited(received) => received,
            WorkerState::Running => outcome.try_recv().ok(),
        };
        received.filter(|_| exited)
            .ok_or_else(|| failure("Record the display", "The recording worker terminated unexpectedly."))
    }
}
