//! Controller thread and the read-only lifecycle view used by current workers.
//! Only the controller changes the phase, boundaries, and clock facts.

pub(crate) mod disk;
mod environment;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use crate::package::metadata::{AttemptClock, ClockAnchor};

use crate::{
    CaptureError,
    capture::save::{FixtureSave, FixtureSaveError, FixtureSaveResult},
    config::ResolvedCaptureConfig,
    package::{diagnostics::{DiagnosticContext, Failure, ReportOutcome}, metadata::{
        ElevatedSourceEvidence, InputBlindReason, SelectedSourceInterval, SelectedSourceKind, SourceBlindInterval, SourceBlindReason,
        SourceEvidence, SourceRegistration, SourceTail, StandardSourceEvidence, UncertifiedInputTail,
    }},
    platform::windows::{ProbeError, clock, hotkeys::ActiveHotkeys, preflight::ForegroundApproval, unavailable},
    protocol::{AdmissionEpoch, ClockOrigin, MessageTickAnchor, MonitorId, QpcFrequency, QpcTick, SaveId, SessionTimeUs},
};
use screen_fix_protocol::ipc::{FocusAtReceipt, InputRegistration, elevated::{VisitBinding, runtime::IdentityClaim}};

use super::{
    AdmissionBoundary, CommandError, PauseReason, SessionCommand, SessionHandle, SessionPhase,
    ReadyVideo, SessionResult, SessionStatus, SourceFailure, SourceFailureReason, SourceStage, SourceStatus,
    StartupStep, VideoReport, WindowCoverage,
    hotkey::{ControlLoss, ControlNotice, ControlOutcome, HotkeyActivation, RecorderAction},
};

const COUNTDOWN_SECONDS: u8 = 3;
const UTC_INTERVAL_SECONDS: i64 = 60;
const STOP_BUDGET: Duration = Duration::from_secs(10);
const SESSION_END_CONTROLLER_BUDGET: Duration = Duration::from_millis(3750);
const CONTROL_WAIT: Duration = Duration::from_millis(25);
const MAX_CONTROL_NOTICES: usize = 32;
pub(crate) const MAX_FIXTURE_SAVES: usize = 8;
const MAX_SOURCE_TRACE: usize = 4096;
pub(crate) const MAX_INPUT_FAILURES: usize = 4;

// The recording worker and all nested owners retain this lease until their
// actual exit, even when the controller has stopped waiting for a result.
static RECORDING_OWNED: AtomicBool = AtomicBool::new(false);

struct RecordingLease;

impl RecordingLease {
    fn claim() -> Result<Self, CaptureError> {
        RECORDING_OWNED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| failure("Start recording", "The previous recording still owns capture or output resources."))
    }
}

impl Drop for RecordingLease {
    fn drop(&mut self) { RECORDING_OWNED.store(false, Ordering::Release); }
}

#[derive(Default)]
enum AdmissionCheck {
    #[default]
    Idle,
    Requested,
    Running,
    Finished(Result<ForegroundApproval, CaptureError>),
}

#[derive(Clone)]
pub(crate) struct SourcePreparation {
    pub(crate) epoch: AdmissionEpoch,
    pub(crate) startup: Option<ForegroundApproval>,
}

pub(crate) struct SourceDispatch {
    lifecycle: Weak<Lifecycle>,
    epoch: AdmissionEpoch,
    target: IdentityClaim,
}

impl SourceDispatch {
    pub(crate) fn target(&self) -> &IdentityClaim { &self.target }

    pub(crate) fn epoch(&self) -> AdmissionEpoch { self.epoch }

    pub(crate) fn stopped(&self) -> bool {
        self.lifecycle.upgrade().is_none_or(|lifecycle| lifecycle.stop.load(Ordering::Acquire)
            || lifecycle.session_end_started.get().is_some())
    }

    pub(crate) fn with_closed_phase<T>(
        &self, dispatch: impl FnOnce() -> screen_fix_elevated::Result<T>,
    ) -> screen_fix_elevated::Result<T> {
        let lifecycle = self.lifecycle.upgrade().ok_or(screen_fix_elevated::Failure::Cancelled)?;
        let facts = lifecycle.lock();
        if self.stopped() || !source_preparation_current(&facts, self.epoch)
            || facts.startup_target.as_ref().is_some_and(|opening| opening.elevated() != Some(&self.target))
        {
            return Err(screen_fix_elevated::Failure::Cancelled);
        }
        dispatch()
    }
}

#[derive(Clone)]
pub(crate) struct ActiveSourceProof {
    lifecycle: Weak<Lifecycle>,
    epoch: AdmissionEpoch,
}

impl ActiveSourceProof {
    pub(crate) fn current(&self) -> bool {
        let Some(lifecycle) = self.lifecycle.upgrade() else { return false; };
        if lifecycle.stop.load(Ordering::Acquire) || lifecycle.session_end_started.get().is_some() { return false; }
        let Ok(facts) = lifecycle.facts.try_lock() else { return false; };
        source_active(&facts, self.epoch)
    }
}

fn source_active(facts: &Facts, epoch: AdmissionEpoch) -> bool {
    source_epoch_active(facts, epoch) && facts.elevated_epoch == Some(epoch)
}

fn source_epoch_active(facts: &Facts, epoch: AdmissionEpoch) -> bool {
    facts.status.phase == SessionPhase::Recording && !facts.clock_failed && !facts.source_change
        && facts.status.pause_reasons.is_empty()
        && facts.status.opened.is_some_and(|opened| opened.epoch == epoch)
}

fn source_preparation_current(facts: &Facts, epoch: AdmissionEpoch) -> bool {
    matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Paused)
        && (facts.status.phase != SessionPhase::Paused || facts.status.closed.is_some())
        && facts.status.pause_reasons.iter().all(|reason| *reason == PauseReason::ElevationTransition)
        && facts.source_preparation.as_ref().is_some_and(|pending| pending.epoch == epoch)
}

struct SelectedSource {
    epoch: AdmissionEpoch,
    approval: ForegroundApproval,
    focus: Option<FocusAtReceipt>,
}

impl SelectedSource {
    fn current(&self) -> bool {
        self.approval.still_current()
            && self.focus.is_none_or(|focus| self.approval.focus_current(focus))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CompletePublication {
    #[default]
    Awaiting,
    Expired,
    Authorized,
}

#[derive(Default)]
struct Facts {
    status: SessionStatus,
    admission_check: AdmissionCheck,
    utc_anchors: Vec<ClockAnchor>,
    tick_anchor: Option<MessageTickAnchor>,
    video_failures: Vec<VideoFailure>,
    input_failures: Vec<InputFailure>,
    hotkeys: ControlPrefix,
    fixture_saves: VecDeque<FixtureNotice>,
    next_save: u64,
    disk: Option<disk::Sample>,
    stop_deadline: Option<Instant>,
    complete_publication: CompletePublication,
    completed_prefix: Option<AdmissionBoundary>,
    segment_deadline: Option<Instant>,
    pause_deadline: Option<Instant>,
    clock_failed: bool,
    controller_failure: Option<&'static str>,
    pause_prefix: BTreeSet<PauseProducer>,
    paused_video: BTreeMap<MonitorId, PausedVideo>,
    pause_interval_reasons: BTreeSet<PauseReason>,
    last_pause: Option<(AdmissionBoundary, Vec<PauseReason>)>,
    input_prefix: Option<AdmissionBoundary>,
    source_epoch: u64,
    elevated_epoch: Option<AdmissionEpoch>,
    source_change: bool,
    source_preparation: Option<SourcePreparation>,
    source_ready: Option<SelectedSource>,
    startup_target: Option<ForegroundApproval>,
    source_evidence: SourceEvidence,
    selected_intervals: Vec<SelectedInterval>,

}

struct SelectedInterval {
    visit: Option<VisitBinding>,
    evidence: SelectedSourceInterval,
    cutoff: Option<SessionTimeUs>,
}

pub(crate) struct FixtureNotice {
    pub signal: FixtureSave,
    pub id: SaveId,
    pub receipt: Receipt,
    pub reply: SyncSender<FixtureSaveResult>,
}

#[derive(Default)]
struct Lifecycle {
    facts: Mutex<Facts>,
    changed: Condvar,
    stop: AtomicBool,
    session_end_started: Arc<OnceLock<Instant>>,
    input_environment: Arc<AtomicU32>,
    elevated_environment: Arc<AtomicU32>,
    display_revision: Arc<AtomicU32>,
    journal_flush_revision: AtomicU64,
    controller_qpc: AtomicI64,
    hotkey_keys: [u16; 4],
}

#[derive(Default)]
pub(crate) struct ControlPrefix {
    notices: VecDeque<ControlNotice>,
    stop: Option<ControlNotice>,
    pub before: Option<QpcTick>,
    pub loss: Option<ControlLoss>,
    pub failed: bool,
}

impl ControlPrefix {
    fn push(&mut self, notice: ControlNotice) {
        if self.notices.len() < MAX_CONTROL_NOTICES {
            self.notices.push_back(notice);
        } else if notice.activation.action == RecorderAction::Stop && self.stop.is_none() {
            self.stop = Some(notice);
        } else {
            ControlLoss::record(&mut self.loss, notice.activation.at);
        }
    }

    pub fn notices(&self) -> impl Iterator<Item = ControlNotice> + '_ {
        self.notices.iter().copied().chain(self.stop)
    }

    pub fn into_notices(self) -> impl Iterator<Item = ControlNotice> {
        let mut notices = self.notices.into_iter().collect::<Vec<_>>();
        notices.extend(self.stop);
        notices.sort_unstable_by_key(|notice| notice.activation.sequence);
        notices.into_iter()
    }
}

impl Lifecycle {
    fn lock(&self) -> MutexGuard<'_, Facts> {
        self.facts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn apply_session_end_deadlines(&self, facts: &mut Facts) {
        if let Some(&started) = self.session_end_started.get() {
            shorten(&mut facts.stop_deadline, started + SESSION_END_CONTROLLER_BUDGET);
            shorten(&mut facts.segment_deadline, started + environment::SUSPEND_BUDGET);
        }
    }
}

#[derive(Clone)]
pub(crate) struct Control {
    lifecycle: Arc<Lifecycle>,
    commands: SyncSender<SessionCommand>,
    wake: SyncSender<()>,
}

impl Control {
    pub(crate) fn stop_for_indicator_failure(&self) {
        request_stop(&self.lifecycle, &self.wake);
    }

    pub(crate) fn status(&self) -> Option<SessionStatus> {
        self.lifecycle.facts.try_lock().ok().map(|facts| {
            let mut status = facts.status.clone();
            status.final_publication_pending = status.phase == SessionPhase::Stopping
                && facts.complete_publication == CompletePublication::Authorized
                && facts.stop_deadline.is_some_and(|deadline| Instant::now() >= deadline);
            status
        })
    }

    pub(super) fn confirm_fixture_save(
        &self, signal: FixtureSave,
    ) -> Result<Receiver<FixtureSaveResult>, FixtureSaveError> {
        let mut facts = self.lifecycle.facts.try_lock().map_err(|_| FixtureSaveError::Busy)?;
        if facts.status.phase != SessionPhase::Recording || self.lifecycle.stop.load(Ordering::Acquire) {
            return Err(FixtureSaveError::AdmissionClosed);
        }
        if facts.fixture_saves.len() >= MAX_FIXTURE_SAVES {
            return Err(FixtureSaveError::Busy);
        }
        let (Some(origin), Some(boundary)) = (facts.status.clock, facts.status.opened) else {
            return Err(FixtureSaveError::ClockUnavailable);
        };
        let qpc = clock::qpc().map_err(|_| FixtureSaveError::ClockUnavailable)?;
        if qpc < boundary.qpc {
            return Err(FixtureSaveError::ClockUnavailable);
        }
        let next = facts.next_save.checked_add(1).ok_or(FixtureSaveError::ControllerUnavailable)?;
        let (reply, result) = mpsc::sync_channel(1);
        let receipt = Receipt { qpc, clock: origin, boundary, tick_anchor: facts.tick_anchor };
        facts.next_save = next;
        facts.fixture_saves.push_back(FixtureNotice { signal, id: SaveId(next), receipt, reply });
        Ok(result)
    }

    pub(super) fn submit(&self, command: SessionCommand) -> Result<(), CommandError> {
        if command == SessionCommand::Stop {
            request_stop(&self.lifecycle, &self.wake);
            return Ok(());
        }
        if !matches!(command, SessionCommand::Pause(PauseReason::User) | SessionCommand::Resume(PauseReason::User)) {
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

fn request_stop(lifecycle: &Lifecycle, wake: &SyncSender<()>) {
    lifecycle.stop.store(true, Ordering::Release);
    // A full wake slot already guarantees that the controller wakes up.
    let _ = wake.try_send(());
}

#[derive(Clone)]
pub(crate) struct WorkerSession {
    lifecycle: Arc<Lifecycle>,
    config: Arc<ResolvedCaptureConfig>,
    ready: SyncSender<()>,
    wake: SyncSender<()>,
}

#[derive(Clone, Copy)]
pub(crate) struct RecordingStart {
    pub clock: ClockOrigin,
    pub boundary: AdmissionBoundary,
}

#[derive(Clone, Copy)]
pub(crate) struct Receipt {
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
    pub at: SessionTimeUs,
    pub failure: Failure,
    pub reason: InputBlindReason,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PauseProducer {
    Input,
    Capture(MonitorId),
    Video(MonitorId),
    Stills,
}

#[derive(Clone, Copy, serde::Serialize)]
pub(crate) struct PausedVideo {
    pub monitor: MonitorId,
    pub first_paused_slot: u64,
    pub fps: u32,
}

impl PausedVideo {
    pub(crate) fn end_exclusive(self, closed: SessionTimeUs, next: SessionTimeUs) -> Option<u64> {
        let us = next.0.checked_sub(closed.0)?;
        let slots = (i128::from(us) * i128::from(self.fps) + 999_999).div_euclid(1_000_000);
        self.first_paused_slot.checked_add(u64::try_from(slots).ok()?)
    }
}

pub(crate) enum Admission {
    Starting,
    Open(Receipt),
    Closed(Option<AdmissionBoundary>),
}

impl WorkerSession {
    pub(crate) fn active_source(&self, epoch: AdmissionEpoch) -> ActiveSourceProof {
        ActiveSourceProof { lifecycle: Arc::downgrade(&self.lifecycle), epoch }
    }

    pub(crate) fn elevated_active(&self, epoch: AdmissionEpoch) -> bool {
        let facts = self.lifecycle.lock();
        !self.lifecycle.stop.load(Ordering::Acquire) && self.lifecycle.session_end_started.get().is_none()
            && source_active(&facts, epoch)
    }

    pub(crate) fn ordinary_source_active(&self, epoch: AdmissionEpoch) -> bool {
        let facts = self.lifecycle.lock();
        !self.lifecycle.stop.load(Ordering::Acquire) && self.lifecycle.session_end_started.get().is_none()
            && source_epoch_active(&facts, epoch) && facts.elevated_epoch.is_none()
    }

    pub(crate) fn source_dispatch(&self, epoch: AdmissionEpoch, target: &IdentityClaim) -> SourceDispatch {
        SourceDispatch { lifecycle: Arc::downgrade(&self.lifecycle), epoch, target: target.clone() }
    }

    pub(crate) fn controller_qpc(&self) -> QpcTick {
        QpcTick(self.lifecycle.controller_qpc.load(Ordering::Acquire))
    }

    pub(crate) fn elevated_epoch(&self, epoch: AdmissionEpoch) -> bool {
        self.lifecycle.lock().elevated_epoch == Some(epoch)
    }

    pub(crate) fn require_source_change(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if facts.status.opened.is_some_and(|boundary| boundary.epoch == epoch)
            && matches!(facts.status.phase, SessionPhase::Recording | SessionPhase::Paused)
        {
            facts.source_change = true;
            let _ = self.wake.try_send(());
        }
    }

    pub(crate) fn source_preparation(&self) -> Option<SourcePreparation> {
        let facts = self.lifecycle.lock();
        facts.source_preparation.as_ref().filter(|pending| !self.lifecycle.stop.load(Ordering::Acquire)
            && source_preparation_current(&facts, pending.epoch)).cloned()
    }

    pub(crate) fn source_authorized(&self, epoch: AdmissionEpoch) -> bool {
        let facts = self.lifecycle.lock();
        !self.lifecycle.stop.load(Ordering::Acquire)
            && self.lifecycle.session_end_started.get().is_none()
            && (source_preparation_current(&facts, epoch)
                || (facts.status.phase == SessionPhase::Recording && facts.elevated_epoch == Some(epoch)))
    }

    pub(crate) fn abandon_source_preparation(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if facts.source_preparation.as_ref().is_some_and(|pending| pending.epoch == epoch) {
            facts.source_ready = None;
            facts.source_preparation = None;
            clear_source_status(&mut facts);
            let _ = self.wake.try_send(());
        }
    }

    pub(crate) fn source_ready(
        &self, epoch: AdmissionEpoch, approval: ForegroundApproval, focus: Option<FocusAtReceipt>,
    ) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop.load(Ordering::Acquire)
            && source_preparation_current(&facts, epoch)
        {
            facts.source_ready = Some(SelectedSource { epoch, approval, focus });
            let _ = self.wake.try_send(());
        }
    }

    pub(crate) fn source_pending(&self, epoch: AdmissionEpoch, status: SourceStatus) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop.load(Ordering::Acquire) && source_preparation_current(&facts, epoch) {
            facts.status.source = status;
        }
    }

    pub(crate) fn source_enhanced(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop.load(Ordering::Acquire) && source_active(&facts, epoch) {
            facts.status.source = SourceStatus::Enhanced;
        }
    }

    pub(crate) fn update_standard_evidence(&self, update: impl FnOnce(&mut StandardSourceEvidence)) {
        update(self.lifecycle.lock().source_evidence.standard.get_or_insert_with(StandardSourceEvidence::default));
    }

    pub(crate) fn update_elevated_evidence(&self, update: impl FnOnce(&mut ElevatedSourceEvidence)) {
        update(self.lifecycle.lock().source_evidence.elevated.get_or_insert_with(ElevatedSourceEvidence::default));
    }

    pub(crate) fn elevated_registration(&self, epoch: AdmissionEpoch, observed_qpc: QpcTick, registration: InputRegistration) {
        let mut facts = self.lifecycle.lock();
        let registration = SourceRegistration { epoch, observed_qpc, registration };
        let evidence = facts.source_evidence.elevated.get_or_insert_with(ElevatedSourceEvidence::default);
        if evidence.registrations.len() < MAX_SOURCE_TRACE {
            evidence.registrations.push(registration);
        } else {
            facts.source_evidence.truncated = true;
        }
    }

    pub(crate) fn record_source_coverage(
        &self, epoch: AdmissionEpoch, visit: Option<VisitBinding>, start: SessionTimeUs, before: SessionTimeUs,
    ) {
        let mut facts = self.lifecycle.lock();
        let before = facts.status.closed.filter(|closed| closed.epoch == epoch)
            .map_or(before, |closed| before.min(closed.time));
        if start >= before { return; }
        if let Some(interval) = facts.selected_intervals.last_mut()
            && interval.evidence.epoch == epoch && interval.visit == visit
        {
            let before = interval.cutoff.map_or(before, |cutoff| before.min(cutoff));
            interval.evidence.before_t_us = interval.evidence.before_t_us.max(before);
        } else if facts.selected_intervals.len() < MAX_SOURCE_TRACE {
            facts.selected_intervals.push(SelectedInterval {
                visit, cutoff: None,
                evidence: SelectedSourceInterval {
                    source: if visit.is_some() { SelectedSourceKind::Elevated } else { SelectedSourceKind::Ordinary },
                    epoch, source_generation: visit.map(|binding| binding.source_generation), start_t_us: start,
                    before_t_us: before,
                },
            });
        } else {
            facts.source_evidence.truncated = true;
        }
    }

    pub(crate) fn restrict_source_coverage(&self, visit: VisitBinding, before: QpcTick) {
        let mut facts = self.lifecycle.lock();
        let Some(before) = facts.status.clock.and_then(|clock| clock.time(before).ok()) else { return; };
        if let Some(interval) = facts.selected_intervals.iter_mut().rev().find(|item| item.visit == Some(visit)) {
            restrict_interval(interval, before);
        }
    }

    pub(crate) fn last_elevated_coverage(&self, epoch: AdmissionEpoch) -> Option<SessionTimeUs> {
        self.lifecycle.lock().selected_intervals.iter().rev()
            .find(|item| item.evidence.epoch == epoch && item.visit.is_some()
                && item.evidence.start_t_us < item.evidence.before_t_us)
            .map(|item| item.evidence.before_t_us)
    }

    pub(crate) fn record_uncertified_tail(&self, epoch: AdmissionEpoch, tail: UncertifiedInputTail) {
        let mut facts = self.lifecycle.lock();
        if let Some(interval) = facts.selected_intervals.last_mut()
            .filter(|item| item.evidence.epoch == epoch && item.visit.is_none())
        {
            restrict_interval(interval, tail.first_t_us);
        }
        let evidence = &mut facts.source_evidence;
        if evidence.uncertified_tails.len() < MAX_SOURCE_TRACE {
            evidence.uncertified_tails.push(SourceTail { epoch, tail });
        } else {
            evidence.truncated = true;
        }
    }

    pub(crate) fn record_source_blind(&self, interval: SourceBlindInterval) {
        push_source_blind(&mut self.lifecycle.lock(), interval);
    }

    pub(crate) fn source_resynchronizing(&self, epoch: AdmissionEpoch) {
        let mut facts = self.lifecycle.lock();
        if !self.lifecycle.stop.load(Ordering::Acquire) && source_active(&facts, epoch) {
            facts.status.source = SourceStatus::Activating;
        }
    }

    pub(crate) fn source_evidence(&self) -> Option<SourceEvidence> {
        source_evidence(&self.lifecycle.lock())
    }

    pub(crate) fn fail_classified_source(&self, failure: SourceFailure) -> bool {
        let mut facts = self.lifecycle.lock();
        self.stop_for_source_failure(&mut facts, failure)
    }

    pub(crate) fn fail_source(&self, stage: SourceStage, failure: screen_fix_elevated::Failure) {
        let mut facts = self.lifecycle.lock();
        self.stop_for_source_failure(&mut facts, SourceFailure::elevated(stage, failure, false));
    }

    fn stop_for_source_failure(&self, facts: &mut Facts, mut failure: SourceFailure) -> bool {
        failure.admitted = facts.status.clock.is_some();
        let first = self.lifecycle.stop.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok();
        if first || (failure.admitted && matches!(failure.stage, SourceStage::Activation | SourceStage::Capture | SourceStage::Cleanup)
            && !matches!(failure.reason, SourceFailureReason::Cancelled | SourceFailureReason::ScopeClosed))
        {
            set_source_failure(facts, failure);
            let _ = self.wake.try_send(());
        }
        first
    }

    pub(crate) fn required_elevated_hook_failed(&self, closing: bool) {
        let mut facts = self.lifecycle.lock();
        if closing || self.lifecycle.stop.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            set_source_failure(&mut facts, SourceFailure {
                stage: SourceStage::Capture, reason: SourceFailureReason::RequiredHook, admitted: true, native: None, token_identity: None,
            });
            let _ = self.wake.try_send(());
        }
    }

    pub(crate) fn source_finalization_failed(&self, failure: screen_fix_elevated::Failure) {
        let mut facts = self.lifecycle.lock();
        if facts.status.clock.is_some() {
            let failure = SourceFailure::elevated(SourceStage::Cleanup, failure, true);
            if facts.status.source.failure().is_some_and(|primary| primary != failure) {
                facts.source_evidence.cleanup_failure.get_or_insert(failure);
            }
            set_source_failure(&mut facts, failure);
        }
    }

    pub(crate) fn source_failure(&self) -> Option<SourceFailure> {
        self.lifecycle.lock().status.source.failure()
    }

    fn disk_sample(&self, sample: disk::Sample) {
        self.lifecycle.lock().disk = Some(sample);
        let _ = self.wake.try_send(());
    }

    pub(crate) fn stop_requested(&self) -> bool {
        self.lifecycle.stop.load(Ordering::Acquire)
    }

    pub(crate) fn started(&self) -> bool {
        self.lifecycle.lock().status.clock.is_some()
    }

    pub(crate) fn controller_failure(&self) -> Option<&'static str> {
        self.lifecycle.lock().controller_failure
    }

    pub(crate) fn stop_deadline(&self) -> Option<Instant> {
        self.lifecycle.lock().stop_deadline
    }

    pub(crate) fn segment_deadline(&self) -> Option<Instant> {
        self.lifecycle.lock().segment_deadline
    }

    pub(crate) fn pause_interval_reasons(&self, cutoff: AdmissionBoundary) -> Vec<PauseReason> {
        let facts = self.lifecycle.lock();
        if let Some((closed, reasons)) = &facts.last_pause && *closed == cutoff {
            return reasons.clone();
        }
        facts.pause_interval_reasons.iter().copied().collect()
    }

    pub(crate) fn wait_for_stop(&self) -> Result<Option<AdmissionBoundary>, ProbeError> {
        let facts = self.lifecycle.changed.wait_while(self.lifecycle.lock(), |facts| {
            matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Recording | SessionPhase::Paused)
        }).unwrap_or_else(PoisonError::into_inner);
        check_clock(&facts)?;
        Ok(facts.status.closed)
    }

    pub(crate) fn mark_input_prefix(&self, cutoff: AdmissionBoundary) {
        let mut facts = self.lifecycle.lock();
        if facts.status.closed == Some(cutoff) {
            facts.input_prefix = Some(cutoff);
        }
    }

    pub(crate) fn input_prefix_closed(&self, cutoff: AdmissionBoundary) -> bool {
        self.lifecycle.lock().input_prefix == Some(cutoff)
    }

    pub(crate) fn paused_cutoff(&self) -> Option<AdmissionBoundary> {
        let facts = self.lifecycle.lock();
        (facts.status.phase == SessionPhase::Paused).then_some(facts.status.closed).flatten()
    }

    pub(crate) fn pause_pending(&self, cutoff: AdmissionBoundary) -> bool {
        let facts = self.lifecycle.lock();
        facts.status.phase == SessionPhase::Paused && facts.status.closed == Some(cutoff)
    }

    pub(crate) fn paused_video(&self) -> Vec<PausedVideo> {
        self.lifecycle.lock().paused_video.values().copied().collect()
    }

    pub(crate) fn finish_video_pause(&self, cutoff: AdmissionBoundary, video: PausedVideo) {
        let mut facts = self.lifecycle.lock();
        if facts.status.closed == Some(cutoff) {
            facts.paused_video.insert(video.monitor, video);
            if facts.status.phase == SessionPhase::Paused {
                facts.pause_prefix.insert(PauseProducer::Video(video.monitor));
            }
            let _ = self.wake.try_send(());
        }
    }

    pub(crate) fn finish_pause(&self, cutoff: AdmissionBoundary, producer: PauseProducer) {
        let mut facts = self.lifecycle.lock();
        if facts.status.phase == SessionPhase::Paused && facts.status.closed == Some(cutoff) {
            facts.pause_prefix.insert(producer);
            let _ = self.wake.try_send(());
        }
    }

    pub(crate) fn wait_for_reopen(&self, cutoff: AdmissionBoundary) -> Result<Option<RecordingStart>, ProbeError> {
        let facts = self.lifecycle.changed.wait_while(self.lifecycle.lock(), |facts| {
            facts.status.phase == SessionPhase::Paused && facts.status.closed == Some(cutoff)
        }).unwrap_or_else(PoisonError::into_inner);
        check_clock(&facts)?;
        let status = &facts.status;
        match (status.phase, status.clock, status.opened) {
            (SessionPhase::Recording | SessionPhase::Paused | SessionPhase::Stopping | SessionPhase::Failed,
                Some(clock), Some(boundary))
                if boundary.epoch != cutoff.epoch => Ok(Some(RecordingStart { clock, boundary })),
            _ => Ok(None),
        }
    }

    pub(crate) fn stop_boundary(&self) -> Option<AdmissionBoundary> {
        self.lifecycle.lock().status.stop
    }

    pub(crate) fn take_fixture_saves(&self) -> VecDeque<FixtureNotice> {
        std::mem::take(&mut self.lifecycle.lock().fixture_saves)
    }

    pub(crate) fn window_coverage(&self, coverage: WindowCoverage) {
        self.lifecycle.lock().status.window_coverage = Some(coverage);
    }

    pub(crate) fn verify_startup(&self, check: impl FnOnce() -> Result<ForegroundApproval, CaptureError>) {
        let mut facts = self.lifecycle.lock();
        while matches!(facts.admission_check, AdmissionCheck::Idle)
            && facts.status.phase == SessionPhase::Starting
            && !self.lifecycle.stop.load(Ordering::Acquire)
        {
            facts = self.lifecycle.changed.wait(facts).unwrap_or_else(PoisonError::into_inner);
        }
        if facts.status.phase != SessionPhase::Starting
            || self.lifecycle.stop.load(Ordering::Acquire)
            || !matches!(facts.admission_check, AdmissionCheck::Requested)
        {
            return;
        }
        facts.admission_check = AdmissionCheck::Running;
        drop(facts);
        let result = check();
        self.lifecycle.lock().admission_check = AdmissionCheck::Finished(result);
        let _ = self.wake.try_send(());
    }

    pub(crate) fn startup_step(&self, step: StartupStep) {
        self.lifecycle.lock().status.startup = step;
    }

    pub(crate) fn video_ready(&self, video: ReadyVideo) {
        let mut facts = self.lifecycle.lock();
        let ready = &mut facts.status.ready_video;
        if let Some(previous) = ready.iter_mut().find(|item| item.monitor == video.monitor) {
            *previous = video;
        } else {
            ready.push(video);
            ready.sort_unstable_by_key(|item| item.monitor);
        }
    }

    pub(crate) fn input_ready(&self, registration: InputRegistration, required_ready: bool) {
        let mut facts = self.lifecycle.lock();
        facts.status.input_ready = Some(registration);
        facts.status.input_helper_ready = required_ready;
    }

    pub(crate) fn input_registration(&self) -> Option<crate::protocol::ipc::InputRegistration> {
        self.lifecycle.lock().status.input_ready
    }

    pub(crate) fn windows_evidence(&self, evidence: crate::platform::WindowsEvidence) {
        self.lifecycle.lock().status.windows = Some(evidence);
    }

    pub(crate) fn config(&self) -> &ResolvedCaptureConfig {
        &self.config
    }

    pub(crate) fn ready(&self) -> Result<(), ProbeError> {
        self.ready.try_send(())
            .map_err(|_| unavailable("The session controller did not accept readiness"))?;
        let _ = self.wake.try_send(());
        Ok(())
    }

    /// The worker reports this after its local producers and journals have
    /// finished the admitted prefix. The publication decision reads the same fact.
    pub(crate) fn complete_prefix(&self, cutoff: AdmissionBoundary) -> Result<(), ProbeError> {
        let mut facts = self.lifecycle.lock();
        if facts.status.phase != SessionPhase::Stopping || facts.status.closed != Some(cutoff) {
            return Err(unavailable("The session controller did not accept the closed producer prefix"));
        }
        facts.completed_prefix = Some(cutoff);
        Ok(())
    }

    pub(crate) fn authorize_complete_publication(&self) -> io::Result<()> {
        let mut facts = self.lifecycle.lock();
        match facts.complete_publication {
            CompletePublication::Awaiting => {}
            CompletePublication::Expired => return Err(publication_timeout()),
            CompletePublication::Authorized => return Err(io::Error::other("Complete publication was already authorized")),
        }
        if facts.clock_failed {
            return Err(io::Error::other("The session clock or lifecycle controller failed"));
        }
        if let Some(operation) = facts.controller_failure {
            return Err(io::Error::other(operation));
        }
        if facts.status.phase != SessionPhase::Stopping || facts.status.stop.is_none()
            || facts.status.closed.is_none() || facts.completed_prefix != facts.status.closed
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

    pub(crate) fn request_stop(&self) {
        request_stop(&self.lifecycle, &self.wake);
    }

    pub(crate) fn wait_for_start(&self) -> Result<Option<RecordingStart>, ProbeError> {
        let facts = self.lifecycle.changed.wait_while(self.lifecycle.lock(), |facts| {
            facts.status.phase == SessionPhase::Starting
        }).unwrap_or_else(PoisonError::into_inner);
        check_clock(&facts)?;
        match (facts.status.clock, facts.status.opened) {
            (Some(clock), Some(boundary)) => Ok(Some(RecordingStart { clock, boundary })),
            _ => Ok(None),
        }
    }

    /// Receipt and boundary publication share the same short critical section.
    pub(crate) fn admit(&self) -> Result<Admission, ProbeError> {
        let facts = self.lifecycle.lock();
        check_clock(&facts)?;
        let status = &facts.status;
        match status.phase {
            SessionPhase::Starting => Ok(Admission::Starting),
            SessionPhase::Recording => {
                let (Some(clock), Some(boundary)) = (status.clock, status.opened) else {
                    return Err(unavailable("The recording admission has no clock or opening boundary"));
                };
                let qpc = clock::qpc()?;
                if qpc < boundary.qpc {
                    return Err(unavailable("QPC moved backward before the opening boundary"));
                }
                Ok(Admission::Open(Receipt { qpc, clock, boundary, tick_anchor: facts.tick_anchor }))
            }
            _ => Ok(Admission::Closed(status.closed)),
        }
    }

    /// The input pump uses the closed epoch to convert delayed WinEvent
    /// receipts without reopening normal admission.
    pub(crate) fn foreground_closing_receipt(&self, cutoff: AdmissionBoundary) -> Result<Receipt, ProbeError> {
        let facts = self.lifecycle.lock();
        check_clock(&facts)?;
        let status = &facts.status;
        let (Some(clock), Some(boundary)) = (status.clock, status.opened) else {
            return Err(unavailable("The foreground prefix has no clock or opening boundary"));
        };
        if status.closed != Some(cutoff) || boundary.epoch != cutoff.epoch || boundary.qpc > cutoff.qpc {
            return Err(unavailable("The foreground prefix does not match the closed epoch"));
        }
        Ok(Receipt { qpc: cutoff.qpc, clock, boundary, tick_anchor: facts.tick_anchor })
    }

    /// A bounded wait between video pacing or input polling iterations.
    pub(crate) fn wait_for_close(&self, timeout: Duration) -> Result<Option<AdmissionBoundary>, ProbeError> {
        let (facts, _) = self.lifecycle.changed.wait_timeout_while(self.lifecycle.lock(), timeout, |facts| {
            matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Recording)
        }).unwrap_or_else(PoisonError::into_inner);
        check_clock(&facts)?;
        if matches!(facts.status.phase, SessionPhase::Starting | SessionPhase::Recording) {
            return Ok(None);
        }
        Ok(facts.status.closed)
    }

    pub(crate) fn session_time(&self) -> Option<crate::protocol::SessionTimeUs> {
        let origin = self.lifecycle.lock().status.clock?;
        origin.time(clock::qpc().ok()?).ok()
    }

    pub(crate) fn video_failed(&self, monitor: MonitorId, failure: Failure) -> Option<VideoFailure> {
        let mut facts = self.lifecycle.lock();
        if facts.status.video_unavailable.contains(&monitor) {
            return None;
        }
        let at = facts.status.clock.and_then(|origin| origin.time(clock::qpc().ok()?).ok());
        let entry = VideoFailure { monitor, at, failure };
        facts.video_failures.push(entry);
        facts.status.video_unavailable.push(monitor);
        Some(entry)
    }

    pub(crate) fn set_video_report(&self, monitor: MonitorId, outcome: ReportOutcome) {
        let mut facts = self.lifecycle.lock();
        if facts.status.video_unavailable.contains(&monitor)
            && !facts.status.video_reports.iter().any(|report| report.monitor == monitor)
        {
            facts.status.video_reports.push(VideoReport { monitor, outcome });
        }
    }

    pub(crate) fn video_reports(&self) -> Vec<VideoReport> {
        self.lifecycle.lock().status.video_reports.clone()
    }

    pub(crate) fn video_failures_after(&self, prefix: usize) -> Vec<VideoFailure> {
        self.lifecycle.lock().video_failures.get(prefix..).unwrap_or_default().to_vec()
    }

    pub(crate) fn input_failed(&self, failure: InputFailure) -> bool {
        let mut facts = self.lifecycle.lock();
        if facts.input_failures.len() >= MAX_INPUT_FAILURES
            || facts.input_failures.iter().any(|old| old.operation == failure.operation)
        {
            return false;
        }
        facts.input_failures.push(failure);
        facts.status.input_unavailable = Some(failure.operation);
        facts.status.input_report = None;
        true
    }

    pub(crate) fn input_failure(&self) -> Option<InputFailure> {
        self.lifecycle.lock().input_failures.first().copied()
    }

    pub(crate) fn input_failures(&self) -> Vec<InputFailure> {
        self.lifecycle.lock().input_failures.clone()
    }

    pub(crate) fn set_input_report_for(&self, failure: InputFailure, report: ReportOutcome) {
        let mut facts = self.lifecycle.lock();
        if facts.input_failures.last().is_some_and(|latest| latest.operation == failure.operation && latest.at == failure.at) {
            facts.status.input_report = Some(report);
        }
    }

    pub(crate) fn set_input_report(&self, report: ReportOutcome) {
        self.lifecycle.lock().status.input_report = Some(report);
    }

    pub(crate) fn input_report(&self) -> Option<ReportOutcome> {
        self.lifecycle.lock().status.input_report.clone()
    }

    pub(crate) fn hotkey_keys(&self) -> [u16; 4] { self.lifecycle.hotkey_keys }

    pub(crate) fn take_hotkeys(&self) -> ControlPrefix {
        let mut facts = self.lifecycle.lock();
        let pending = &mut facts.hotkeys;
        ControlPrefix {
            notices: std::mem::take(&mut pending.notices), stop: pending.stop.take(),
            before: pending.before, loss: pending.loss.take(), failed: pending.failed,
        }
    }

    pub(crate) fn request_journal_flush(&self) {
        self.lifecycle.journal_flush_revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn journal_flush_revision(&self) -> u64 {
        self.lifecycle.journal_flush_revision.load(Ordering::Acquire)
    }

    pub(crate) fn display_revision(&self) -> u32 {
        self.lifecycle.display_revision.load(Ordering::Acquire)
    }

    pub(crate) fn take_input_environment(&self) -> u32 {
        self.lifecycle.input_environment.swap(0, Ordering::AcqRel)
    }

    pub(crate) fn take_elevated_environment(&self) -> u32 {
        self.lifecycle.elevated_environment.swap(0, Ordering::AcqRel)
    }

    pub(crate) fn attempt_clock(&self) -> Option<AttemptClock> {
        let facts = self.lifecycle.lock();
        Some(AttemptClock {
            origin: facts.status.clock?,
            utc_anchors: facts.utc_anchors.clone(),
            stop: facts.status.stop,
            last_admitted_boundary: facts.status.closed,
        })
    }

}

fn check_clock(facts: &Facts) -> Result<(), ProbeError> {
    if facts.clock_failed {
        return Err(unavailable("The session clock or lifecycle controller failed"));
    }
    Ok(())
}

pub(crate) fn spawn(
    config: ResolvedCaptureConfig,
    hotkeys: ActiveHotkeys,
    trigger: Option<HotkeyActivation>,
    record: fn(&WorkerSession) -> Result<SessionResult, CaptureError>,
) -> Result<SessionHandle, CaptureError> {
    let lease = RecordingLease::claim()?;
    let lifecycle = Arc::new(Lifecycle { hotkey_keys: hotkeys.virtual_keys(), ..Lifecycle::default() });
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
        let result = run(&controller_lifecycle, incoming, woken, initialized, endpoint, hotkeys, record, lease)
            .map_err(|error| {
                let facts = controller_lifecycle.lock();
                let mut context = DiagnosticContext::for_config(&config);
                context.input_registration = facts.status.input_ready;
                context.source_evidence = source_evidence(&facts);
                error.with_context(context)
            });
        let mut facts = controller_lifecycle.lock();
        facts.status.outcome = result.as_ref().ok().map(|result| result.outcome);
        facts.status.phase = if result.is_ok() {
            SessionPhase::Finished
        } else {
            SessionPhase::Failed
        };
        drop(facts);
        controller_lifecycle.changed.notify_all();
        let _ = complete.try_send(result);
    }).map_err(|error| CaptureError::backend("Start the session controller", None, error.to_string()))?;
    Ok(SessionHandle { result, control: Control { lifecycle, commands, wake } })
}

fn run(
    lifecycle: &Lifecycle,
    commands: Receiver<SessionCommand>,
    wake: Receiver<()>,
    ready: Receiver<()>,
    endpoint: WorkerSession,
    hotkeys: ActiveHotkeys,
    record: fn(&WorkerSession) -> Result<SessionResult, CaptureError>,
    lease: RecordingLease,
) -> Result<SessionResult, CaptureError> {
    let frequency = clock::frequency()?;
    let (environment, notifications_ready) = environment::Notifications::spawn(
        Arc::clone(&lifecycle.input_environment), Arc::clone(&lifecycle.elevated_environment), Arc::clone(&lifecycle.display_revision),
        Arc::clone(&lifecycle.session_end_started), endpoint.wake.clone(),
    )?;
    let pause_producer_count = endpoint.config().monitors().len() * 2 + 2;
    let worker = thread::Builder::new().name("video-recording".into())
        .spawn(move || {
            let _lease = lease;
            let result = record(&endpoint);
            (Instant::now(), result)
        })
        .map_err(|error| CaptureError::backend("Start the recording worker", None, error.to_string()))?;
    let result = coordinate(lifecycle, &commands, &wake, &ready, frequency, &worker, &environment, notifications_ready, &hotkeys, pause_producer_count);
    if let Err(error) = &result {
        lifecycle.lock().controller_failure = Some(error.operation);
        // A failed controller still closes admission and releases startup
        // waiters. A failed QPC sample may prevent a certified prefix.
        let _ = stop(lifecycle);
    }
    while !worker.is_finished() {
        let (remaining, release_session_end_wait) = {
            let mut facts = lifecycle.lock();
            lifecycle.apply_session_end_deadlines(&mut facts);
            let deadline = facts.stop_deadline.unwrap_or_else(Instant::now);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() && facts.complete_publication != CompletePublication::Authorized {
                if worker.is_finished() {
                    break;
                }
                facts.complete_publication = CompletePublication::Expired;
                return Err(facts.status.source.failure().map(CaptureError::elevated)
                    .or_else(|| result.err()).unwrap_or_else(|| failure("Finish recording",
                        "The recording could not finish before its deadline. An active owner may still hold capture or output resources.")));
            }
            (remaining, remaining.is_zero() && lifecycle.session_end_started.get().is_some())
        };
        if release_session_end_wait {
            environment.finished();
        }
        let wait = if remaining.is_zero() { CONTROL_WAIT } else { remaining.min(CONTROL_WAIT) };
        let _ = wake.recv_timeout(wait);
    }
    let worker_result = worker.join()
        .map_err(|_| failure("Record the display", "The recording worker terminated unexpectedly."));
    let mut facts = lifecycle.lock();
    if facts.complete_publication == CompletePublication::Authorized {
        drop(facts);
        return worker_result?.1;
    }
    lifecycle.apply_session_end_deadlines(&mut facts);
    let (completed, worker_result) = worker_result?;
    let acknowledged = match result {
        Ok(acknowledged) => acknowledged,
        Err(error) => {
            if let Err(worker_error) = worker_result
                && let Some(source) = worker_error.source_failure()
            {
                if error.source_failure() == Some(source) {
                    return Err(worker_error);
                }
                if error.source_failure().is_none() {
                    return Err(worker_error.with_additional_failure(error.failure()));
                }
            }
            return Err(error);
        }
    };
    if facts.stop_deadline.is_some_and(|deadline| completed >= deadline) {
        if let Some(source) = facts.status.source.failure() {
            let error = worker_result.err().filter(|error| error.source_failure() == Some(source));
            return Err(error.unwrap_or_else(|| CaptureError::elevated(source)));
        }
        return Err(failure("Finish recording", "The recording owners finished after the finalization deadline."));
    }
    let message = worker_result?;
    if let Some(cutoff) = facts.status.closed
        && acknowledged != Some(cutoff) && facts.completed_prefix != Some(cutoff)
    {
        return Err(failure("Finish recording", "The recording worker did not finish the admitted producer prefixes."));
    }
    Ok(message)
}

fn publication_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "The recording deadline expired before Complete publication")
}

fn coordinate(
    lifecycle: &Lifecycle,
    commands: &Receiver<SessionCommand>,
    wake: &Receiver<()>,
    ready: &Receiver<()>,
    frequency: QpcFrequency,
    worker: &thread::JoinHandle<(Instant, Result<SessionResult, CaptureError>)>,
    environment: &environment::Notifications,
    notifications_ready: Receiver<Result<(), CaptureError>>,
    hotkeys: &ActiveHotkeys,
    pause_producer_count: usize,
) -> Result<Option<AdmissionBoundary>, CaptureError> {
    let mut countdown = None;
    let mut reasons = BTreeSet::new();
    let mut pause_finished = None;
    let mut disk_resume = None;
    let mut subscribed = false;
    loop {
        if lifecycle.session_end_started.get().is_some() {
            lifecycle.stop.store(true, Ordering::Release);
        }
        route_hotkeys(lifecycle, &mut reasons, &mut disk_resume, hotkeys, frequency)?;
        if lifecycle.stop.load(Ordering::Acquire) || worker.is_finished() {
            stop(lifecycle)?;
            if let Some(failure) = lifecycle.lock().status.source.failure() {
                return Err(CaptureError::elevated(failure));
            }
            return Ok(pause_finished);
        }
        lifecycle.controller_qpc.store(clock::qpc()?.0, Ordering::Release);
        if !subscribed {
            match notifications_ready.try_recv() {
                Ok(result) => { result?; subscribed = true; }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(failure("Monitor recording environment", "The Windows notification owner failed to start."));
                }
            }
        } else if !environment.running() {
            return Err(failure("Monitor recording environment", "The Windows notification owner exited unexpectedly."));
        }
        let (active, asserted) = environment.reasons();
        for (bit, reason) in [(environment::SLEEP, PauseReason::Sleep), (environment::LOCK_SCREEN, PauseReason::LockScreen),
            (environment::SECURE_DESKTOP, PauseReason::SecureDesktop)]
        {
            if (active | asserted) & bit != 0 {
                if reason == PauseReason::Sleep && !reasons.contains(&reason) {
                    let mut facts = lifecycle.lock();
                    if matches!(facts.status.phase, SessionPhase::Recording | SessionPhase::Paused) {
                        let deadline = environment.suspend_started().unwrap_or_else(Instant::now) + environment::SUSPEND_BUDGET;
                        shorten(&mut facts.segment_deadline, deadline);
                    }
                }
                pause_command(lifecycle, &mut reasons, SessionCommand::Pause(reason))?;
            }
            if active & bit == 0 {
                pause_command(lifecycle, &mut reasons, SessionCommand::Resume(reason))?;
            }
        }
        let startup_hotkeys_lost = {
            let facts = lifecycle.lock();
            facts.status.hotkeys_unavailable && facts.status.phase == SessionPhase::Starting
        };
        if startup_hotkeys_lost {
            return Err(failure("Verify recorder hotkey availability", "The registered recorder hotkeys became unavailable before recording started."));
        }
        if subscribed && countdown.is_none() && ready.try_recv().is_ok() {
            lifecycle.lock().status.startup = StartupStep::Ready;
            countdown = Some(clock::qpc()?);
        }
        // Bound processing per wake and revisit the independent Stop latch first.
        for command in commands.try_iter().take(8) {
            if lifecycle.stop.load(Ordering::Acquire) {
                break;
            }
            user_command(lifecycle, &mut reasons, &mut disk_resume, command)?;
        }
        if lifecycle.stop.load(Ordering::Acquire) {
            continue;
        }
        update_disk(lifecycle, &mut reasons, &mut disk_resume)?;
        let handoff = {
            let facts = lifecycle.lock();
            facts.source_change || (facts.status.phase == SessionPhase::Paused && facts.elevated_epoch.is_some())
        };
        if handoff {
            pause_command(lifecycle, &mut reasons, SessionCommand::Pause(PauseReason::ElevationTransition))?;
        }
        {
            let mut facts = lifecycle.lock();
            if facts.status.phase == SessionPhase::Paused {
                if facts.pause_prefix.iter().filter(|producer| matches!(producer, PauseProducer::Video(_))).count()
                    == (pause_producer_count - 2) / 2
                {
                    facts.segment_deadline = None;
                }
                if facts.pause_prefix.len() == pause_producer_count {
                    pause_finished = facts.status.closed;
                    facts.pause_deadline = None;
                }
            }
        }
        if pause_finished.is_none() && lifecycle.lock().segment_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(failure("Finish the suspended video segment", "Video did not finish within the suspend budget."));
        }
        if lifecycle.lock().pause_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(failure("Finish the paused recording prefix", "A recording owner did not finish its pause prefix."));
        }
        if let Some(finished) = pause_finished {
            if !handoff && reasons.is_empty() {
                resume(lifecycle, finished, None)?;
                pause_finished = None;
            } else if handoff && reasons.iter().all(|reason| *reason == PauseReason::ElevationTransition) {
                let selected = {
                    let mut facts = lifecycle.lock();
                    prepare_source(&mut facts)?;
                    facts.source_ready.take()
                };
                if let Some(selected) = selected.filter(SelectedSource::current) {
                    pause_command(lifecycle, &mut reasons, SessionCommand::Resume(PauseReason::ElevationTransition))?;
                    resume(lifecycle, finished, Some(selected))?;
                    pause_finished = None;
                }
            }
        }
        let mut wait = CONTROL_WAIT;
        if let Some(begin) = countdown {
            let ticks_per_second = i128::from(frequency.0.get());
            let elapsed = i128::from(clock::qpc()?.0) - i128::from(begin.0);
            if elapsed < 0 {
                return Err(failure("Count down to recording", "QPC moved backward during the countdown."));
            }
            let remaining = (i128::from(COUNTDOWN_SECONDS) * ticks_per_second - elapsed).max(0);
            if remaining == 0 && reasons.contains(&PauseReason::LowDisk) {
                return Err(startup_disk_error());
            }
            if remaining == 0 && reasons.is_empty() {
                let (checked, selected) = {
                    let mut facts = lifecycle.lock();
                    if facts.startup_target.is_some() {
                        prepare_source(&mut facts)?;
                        (None, facts.source_ready.take())
                    } else {
                        let checked = match std::mem::take(&mut facts.admission_check) {
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
                        };
                        (checked, None)
                    }
                };
                if let Some(checked) = checked {
                    if lifecycle.stop.load(Ordering::Acquire) {
                        continue;
                    }
                    let approved = match checked {
                        Ok(approved) => approved,
                        Err(error) => {
                            let mut facts = lifecycle.lock();
                            if lifecycle.stop.load(Ordering::Acquire) { continue; }
                            if let Some(failure) = error.source_failure()
                                && !commit_startup_source_failure(lifecycle, &mut facts, failure)
                            {
                                continue;
                            }
                            return Err(error);
                        }
                    };
                    if !lifecycle.lock().status.disk_available_bytes.is_some_and(|bytes| bytes >= disk::START_MINIMUM) {
                        return Err(startup_disk_error());
                    }
                    if !approved.still_current() {
                        if approved.elevated().is_some() {
                            let mut facts = lifecycle.lock();
                            let failure = SourceFailure {
                                stage: SourceStage::Foreground, reason: SourceFailureReason::TargetChanged,
                                admitted: false, native: None, token_identity: None,
                            };
                            if !commit_startup_source_failure(lifecycle, &mut facts, failure) { continue; }
                            return Err(CaptureError::elevated(failure));
                        }
                        return Err(failure("Verify approved foreground",
                            "The approved foreground window changed before recording started. Retry and keep the application in front."));
                    }
                    if approved.elevated().is_some() {
                        let mut facts = lifecycle.lock();
                        facts.startup_target = Some(approved);
                        prepare_source(&mut facts)?;
                    } else {
                        start(lifecycle, frequency, None)?;
                        countdown = None;
                    }
                } else if let Some(selected) = selected {
                    let live = {
                        let facts = lifecycle.lock();
                        facts.startup_target.as_ref().and_then(ForegroundApproval::elevated) == selected.approval.elevated()
                            && facts.status.disk_available_bytes.is_some_and(|bytes| bytes >= disk::START_MINIMUM)
                    };
                    if !live || !selected.current() {
                        let mut facts = lifecycle.lock();
                        if lifecycle.stop.load(Ordering::Acquire) { continue; }
                        if !facts.status.disk_available_bytes.is_some_and(|bytes| bytes >= disk::START_MINIMUM) {
                            return Err(startup_disk_error());
                        }
                        let failure = SourceFailure {
                            stage: SourceStage::Readiness, reason: SourceFailureReason::TargetChanged,
                            admitted: false, native: None, token_identity: None,
                        };
                        if !commit_startup_source_failure(lifecycle, &mut facts, failure) { continue; }
                        return Err(CaptureError::elevated(failure));
                    }
                    start(lifecycle, frequency, Some(selected))?;
                    countdown = None;
                }
            } else {
                let seconds = ((remaining + ticks_per_second - 1) / ticks_per_second) as u8;
                lifecycle.lock().status.countdown_seconds = (remaining != 0).then_some(seconds);
                if remaining != 0 {
                    // Use a bounded wait and let the next QPC read decide when Start occurs.
                    let nanos = (remaining * 1_000_000_000 + ticks_per_second - 1) / ticks_per_second;
                    wait = wait.min(Duration::from_nanos(nanos as u64));
                }
            }
        }
        sample_anchors(lifecycle)?;
        let _ = wake.recv_timeout(wait);
    }
}

fn commit_startup_source_failure(lifecycle: &Lifecycle, facts: &mut Facts, failure: SourceFailure) -> bool {
    if lifecycle.session_end_started.get().is_some()
        || lifecycle.stop.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err()
    {
        return false;
    }
    set_source_failure(facts, failure);
    true
}

fn startup_disk_error() -> CaptureError {
    failure("Verify startup disk space", "The recording output volume no longer reports at least 20 GB free. Free space or pick another folder, then retry.")
}

fn user_command(
    lifecycle: &Lifecycle, reasons: &mut BTreeSet<PauseReason>, disk_resume: &mut Option<Instant>,
    command: SessionCommand,
) -> Result<(), CaptureError> {
    if command == SessionCommand::Resume(PauseReason::User) && reasons.contains(&PauseReason::LowDisk) {
        *disk_resume = Some(Instant::now());
    }
    pause_command(lifecycle, reasons, command)
}

fn update_disk(
    lifecycle: &Lifecycle, reasons: &mut BTreeSet<PauseReason>, disk_resume: &mut Option<Instant>,
) -> Result<(), CaptureError> {
    let sample = {
        let mut facts = lifecycle.lock();
        let sample = facts.disk.filter(|sample| sample.started.elapsed() <= disk::MAX_SAMPLE_AGE);
        facts.status.disk_available_bytes = sample.and_then(|sample| sample.available_bytes);
        if facts.disk.is_none() {
            return Ok(());
        }
        sample
    };
    if sample.and_then(|sample| sample.available_bytes).is_none_or(|bytes| bytes < disk::PAUSE_BELOW) {
        pause_command(lifecycle, reasons, SessionCommand::Pause(PauseReason::LowDisk))?;
    }
    if let (Some(request), Some(sample)) = (*disk_resume, sample) && sample.started >= request {
        *disk_resume = None;
        if sample.available_bytes.is_some_and(|bytes| bytes > disk::RESUME_ABOVE) {
            pause_command(lifecycle, reasons, SessionCommand::Resume(PauseReason::LowDisk))?;
        }
    }
    Ok(())
}

fn route_hotkeys(
    lifecycle: &Lifecycle, reasons: &mut BTreeSet<PauseReason>, disk_resume: &mut Option<Instant>,
    hotkeys: &ActiveHotkeys, frequency: QpcFrequency,
) -> Result<(), CaptureError> {
    let batch = hotkeys.drain();
    let before = batch.before;
    let loss = batch.loss;
    let detected_at = clock::qpc()?;
    let overdue = before.is_some_and(|last| {
        i128::from(detected_at.0).saturating_sub(i128::from(last.0)) * 4 > i128::from(frequency.0.get())
    });
    let failed = batch.failed || overdue;
    let already_failed = lifecycle.lock().hotkeys.failed;
    for activation in batch.into_events().take_while(|_| !already_failed) {
        let outcome = if lifecycle.stop.load(Ordering::Acquire) {
            ControlOutcome::AlreadyStopping
        } else {
            match activation.action {
                RecorderAction::Start => ControlOutcome::AlreadyStartingOrRecording,
                RecorderAction::TogglePause => {
                    let can_pause = matches!(lifecycle.lock().status.phase, SessionPhase::Recording | SessionPhase::Paused);
                    if !can_pause {
                        ControlOutcome::PauseUnavailable
                    } else if reasons.contains(&PauseReason::User) || reasons.contains(&PauseReason::LowDisk) {
                        user_command(lifecycle, reasons, disk_resume, SessionCommand::Resume(PauseReason::User))?;
                        ControlOutcome::ResumeRequested
                    } else {
                        pause_command(lifecycle, reasons, SessionCommand::Pause(PauseReason::User))?;
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
                    lifecycle.stop.store(true, Ordering::Release);
                    ControlOutcome::StopRequested
                }
            }
        };
        let mut facts = lifecycle.lock();
        facts.hotkeys.push(ControlNotice { activation, outcome });
        facts.status.last_hotkey = Some(outcome);
    }
    let mut facts = lifecycle.lock();
    if !already_failed {
        facts.hotkeys.before = before;
        if let Some(loss) = loss {
            ControlLoss::merge(&mut facts.hotkeys.loss, loss);
        }
        if failed {
            ControlLoss::merge(&mut facts.hotkeys.loss, ControlLoss {
                first: before.unwrap_or(detected_at), last: detected_at, count: None,
            });
            facts.hotkeys.failed = true;
            facts.status.hotkeys_unavailable = true;
        }
    }
    Ok(())
}

fn restrict_interval(interval: &mut SelectedInterval, before: SessionTimeUs) {
    interval.cutoff = Some(interval.cutoff.map_or(before, |cutoff| cutoff.min(before)));
    interval.evidence.before_t_us = interval.evidence.before_t_us.min(before);
}

fn push_source_blind(facts: &mut Facts, interval: SourceBlindInterval) {
    if interval.start_t_us >= interval.before_t_us { return; }
    let evidence = &mut facts.source_evidence;
    if evidence.blind.len() < MAX_SOURCE_TRACE {
        evidence.blind.push(interval);
    } else {
        evidence.truncated = true;
    }
}

fn source_evidence(facts: &Facts) -> Option<SourceEvidence> {
    let mut evidence = facts.source_evidence.clone();
    evidence.failure = facts.status.source.failure();
    evidence.intervals.extend(facts.selected_intervals.iter().map(|interval| interval.evidence)
        .filter(|interval| interval.start_t_us < interval.before_t_us));
    evidence.blind.sort_by_key(|interval| (interval.start_t_us, interval.before_t_us));
    (evidence.standard.is_some() || evidence.elevated.is_some() || evidence.failure.is_some() || !evidence.intervals.is_empty()
        || !evidence.uncertified_tails.is_empty() || !evidence.blind.is_empty() || evidence.truncated).then_some(evidence)
}

fn clear_source_status(facts: &mut Facts) {
    if facts.status.source.failure().is_none() {
        facts.status.source = SourceStatus::Unselected;
    }
}

fn set_source_failure(facts: &mut Facts, failure: SourceFailure) {
    if facts.status.source.failure().is_none() {
        facts.controller_failure = Some(CaptureError::elevated(failure).operation);
        facts.status.source = if failure.admitted {
            SourceStatus::FailedPrefix(failure)
        } else {
            SourceStatus::Unavailable(failure)
        };
    }
}

fn prepare_source(facts: &mut Facts) -> Result<(), CaptureError> {
    if facts.source_preparation.is_none() {
        facts.source_epoch = facts.source_epoch.checked_add(1)
            .ok_or_else(|| failure("Prepare a capture source", "The admission epoch overflowed."))?;
        facts.source_preparation = Some(SourcePreparation {
            epoch: AdmissionEpoch(facts.source_epoch), startup: facts.startup_target.clone(),
        });
    }
    Ok(())
}

fn select_source(facts: &mut Facts, epoch: AdmissionEpoch, selected: Option<SelectedSource>) {
    facts.source_epoch = epoch.0;
    facts.elevated_epoch = selected.as_ref().filter(|source| source.approval.elevated().is_some()).map(|source| source.epoch);
    facts.status.source = if facts.elevated_epoch.is_some() {
        SourceStatus::Activating
    } else {
        SourceStatus::Ordinary
    };
    facts.startup_target = None;
    facts.source_preparation = None;
    facts.source_ready = None;
    facts.source_change = false;
}

fn start(lifecycle: &Lifecycle, frequency: QpcFrequency, selected: Option<SelectedSource>) -> Result<(), CaptureError> {
    let mut facts = lifecycle.lock();
    if lifecycle.stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let anchor = clock::utc_anchor()?;
    let origin = ClockOrigin { origin_qpc: anchor.qpc, qpc_frequency: frequency };
    let epoch = selected.as_ref().map_or(AdmissionEpoch(1), |source| source.epoch);
    let boundary = boundary(origin, anchor.qpc, epoch)?;
    let tick_anchor = clock::message_tick_anchor()?;
    if lifecycle.stop.load(Ordering::Acquire) || lifecycle.session_end_started.get().is_some() {
        return Ok(());
    }
    facts.tick_anchor = Some(tick_anchor);
    facts.utc_anchors.push(ClockAnchor { admission_epoch: boundary.epoch, sample: anchor });
    facts.status.clock = Some(origin);
    facts.status.opened = Some(boundary);
    select_source(&mut facts, epoch, selected);
    facts.status.phase = SessionPhase::Recording;
    facts.status.countdown_seconds = None;
    lifecycle.changed.notify_all();
    Ok(())
}

fn shorten(deadline: &mut Option<Instant>, candidate: Instant) {
    *deadline = Some(deadline.map_or(candidate, |old| old.min(candidate)));
}

fn stop(lifecycle: &Lifecycle) -> Result<(), CaptureError> {
    let mut facts = lifecycle.lock();
    lifecycle.apply_session_end_deadlines(&mut facts);
    if facts.status.phase == SessionPhase::Stopping {
        return Ok(());
    }
    let was_recording = facts.status.phase == SessionPhase::Recording;
    let now = Instant::now();
    shorten(&mut facts.stop_deadline, now + STOP_BUDGET);
    shorten(&mut facts.segment_deadline, now + STOP_BUDGET);
    facts.status.phase = SessionPhase::Stopping;
    if facts.status.clock.is_none() { clear_source_status(&mut facts); }
    facts.source_preparation = None;
    facts.source_ready = None;
    facts.status.countdown_seconds = None;
    let sampled = (|| {
        if let (Some(origin), Some(opened)) = (facts.status.clock, facts.status.opened) {
            let stopped = boundary(origin, clock::qpc()?, opened.epoch)?;
            facts.status.stop = Some(stopped);
            if was_recording {
                facts.status.closed = Some(stopped);
                facts.input_prefix = None;
            } else if let Some(closed) = facts.status.closed
                && facts.pause_interval_reasons.contains(&PauseReason::ElevationTransition)
            {
                push_source_blind(&mut facts, SourceBlindInterval {
                    epoch: closed.epoch, start_t_us: closed.time, before_t_us: stopped.time,
                    reason: SourceBlindReason::ElevationTransition,
                });
            }
            facts.utc_anchors.push(ClockAnchor { admission_epoch: stopped.epoch, sample: clock::utc_anchor()? });
        }
        Ok::<(), CaptureError>(())
    })();
    if sampled.is_err() {
        facts.clock_failed = true;
    }
    lifecycle.changed.notify_all();
    sampled
}

fn pause_command(
    lifecycle: &Lifecycle,
    reasons: &mut BTreeSet<PauseReason>,
    command: SessionCommand,
) -> Result<(), CaptureError> {
    let changed = match command {
        SessionCommand::Pause(reason) => reasons.insert(reason),
        SessionCommand::Resume(reason) => reasons.remove(&reason),
        SessionCommand::Stop => false,
    };
    if !changed {
        return Ok(());
    }
    let mut facts = lifecycle.lock();
    facts.status.pause_reasons = reasons.iter().copied().collect();
    if matches!(command, SessionCommand::Pause(reason) if reason != PauseReason::ElevationTransition) {
        facts.source_preparation = None;
        facts.source_ready = None;
        if facts.status.phase != SessionPhase::Recording { clear_source_status(&mut facts); }
    }
    if facts.status.phase == SessionPhase::Paused {
        facts.pause_interval_reasons.extend(reasons.iter().copied());
    } else if facts.status.phase == SessionPhase::Recording && !reasons.is_empty()
        && let (Some(origin), Some(opened)) = (facts.status.clock, facts.status.opened)
    {
        let closed = boundary(origin, clock::qpc()?, opened.epoch)?;
        let anchor = clock::utc_anchor()?;
        facts.status.closed = Some(closed);
        facts.utc_anchors.push(ClockAnchor { admission_epoch: closed.epoch, sample: anchor });
        facts.status.phase = SessionPhase::Paused;
        clear_source_status(&mut facts);
        facts.pause_prefix.clear();
        facts.paused_video.clear();
        facts.input_prefix = None;
        facts.pause_interval_reasons.clone_from(reasons);
        facts.pause_deadline = Some(Instant::now() + STOP_BUDGET);
        lifecycle.changed.notify_all();
    }
    Ok(())
}

fn resume(lifecycle: &Lifecycle, finished: AdmissionBoundary, selected: Option<SelectedSource>) -> Result<(), CaptureError> {
    let mut facts = lifecycle.lock();
    if lifecycle.stop.load(Ordering::Acquire) || facts.status.phase != SessionPhase::Paused || facts.status.closed != Some(finished) {
        return Ok(());
    }
    let origin = facts.status.clock
        .ok_or_else(|| failure("Resume recording", "The session clock is missing."))?;
    let epoch = match selected.as_ref() {
        Some(source) => source.epoch,
        None => facts.source_epoch.checked_add(1).map(AdmissionEpoch)
            .ok_or_else(|| failure("Resume recording", "The admission epoch overflowed."))?,
    };
    let resume_anchor = clock::utc_anchor()?;
    let tick_anchor = clock::message_tick_anchor()?;
    let reopened = boundary(origin, resume_anchor.qpc, epoch)?;
    if facts.pause_interval_reasons.contains(&PauseReason::ElevationTransition) {
        push_source_blind(&mut facts, SourceBlindInterval {
            epoch: finished.epoch, start_t_us: finished.time, before_t_us: reopened.time,
            reason: SourceBlindReason::ElevationTransition,
        });
    }
    facts.status.opened = Some(reopened);
    select_source(&mut facts, epoch, selected);
    facts.status.closed = None;
    facts.status.phase = SessionPhase::Recording;
    facts.segment_deadline = None;
    facts.last_pause = Some((finished, facts.pause_interval_reasons.iter().copied().collect()));
    facts.utc_anchors.push(ClockAnchor { admission_epoch: epoch, sample: resume_anchor });
    facts.tick_anchor = Some(tick_anchor);
    lifecycle.changed.notify_all();
    Ok(())
}

fn sample_anchors(lifecycle: &Lifecycle) -> Result<(), CaptureError> {
    let mut facts = lifecycle.lock();
    if !matches!(facts.status.phase, SessionPhase::Recording | SessionPhase::Paused) {
        return Ok(());
    }
    let (Some(origin), Some(opened)) = (facts.status.clock, facts.status.opened) else {
        return Ok(());
    };
    let now = clock::qpc()?;
    let ticks_per_second = i128::from(origin.qpc_frequency.0.get());
    if facts.tick_anchor.is_none_or(|anchor| {
        i128::from(now.0) - i128::from(anchor.qpc.0) >= ticks_per_second
    }) {
        facts.tick_anchor = Some(clock::message_tick_anchor()?);
    }
    if facts.utc_anchors.last().is_none_or(|anchor| {
        i128::from(now.0) - i128::from(anchor.sample.qpc.0) >= ticks_per_second * i128::from(UTC_INTERVAL_SECONDS)
    }) {
        facts.utc_anchors.push(ClockAnchor { admission_epoch: opened.epoch, sample: clock::utc_anchor()? });
    }
    Ok(())
}

fn boundary(origin: ClockOrigin, qpc: QpcTick, epoch: AdmissionEpoch) -> Result<AdmissionBoundary, CaptureError> {
    Ok(AdmissionBoundary { epoch, qpc, time: origin.time(qpc).map_err(timing_error)? })
}

fn timing_error(error: crate::protocol::ClockRangeError) -> CaptureError {
    CaptureError::backend("Convert the session timestamp", None, error.to_string())
}

fn failure(operation: &'static str, message: &'static str) -> CaptureError {
    CaptureError::backend(operation, None, message.into())
}
