//! The lifecycle facts behind the controller's mutex, grouped by the event
//! that resets each group.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Instant,
};

use crate::{
    CaptureError,
    package::{
        diagnostics::Failure, journal::HealthRecord, metadata::{ClockAnchor, SourceEvidence}, outcome::HealthKind,
    },
    platform::windows::{ProbeError, clock, preflight::ForegroundApproval, unavailable},
    protocol::{AdmissionEpoch, ClockOrigin, MessageTickAnchor, MonitorId, QpcTick, SessionTimeUs},
    session::{
        AdmissionBoundary, PauseReason, SessionPhase, SessionStatus,
        fault::{FaultLog, FaultReport},
        hotkey::{ControlLoss, ControlNotice, RecorderAction},
    },
    util::push_bounded,
};

use super::{
    AwakeMark, disk,
    source::{SelectedInterval, SelectedSource, SourcePreparation},
    worker::{FixtureNotice, InputFailure, PauseProducer, PausedVideo, VideoFailure},
};

const MAX_CONTROL_NOTICES: usize = 32;
const MAX_HEALTH_REPORTS: usize = 256;

/// The controller's lifecycle state machine. `status.phase` is the state; the
/// groups below are its associated data. Only the controller thread changes
/// the phase, the boundaries and the clock facts.
///
/// States and transitions (`SessionPhase`):
/// - `Starting` → `Recording` in `start`: the clock origin is set, `opened`
///   holds the first admission boundary and the source is selected.
/// - `Recording` → `Paused` in `set_pause`, when the pause reasons become
///   non-empty: `closed` holds the pause cutoff and `pause.prefix` restarts
///   for it.
/// - `Paused` → `Recording` in `resume`, once every producer finished the
///   pause at `closed`: `opened` holds the new admission epoch; `closed` and
///   `pause.suspend` are cleared.
/// - `Starting | Recording | Paused` → `Stopping` in `stop`: `bounds.stop`
///   holds the stop boundary (and `closed` when stopping while recording);
///   `stop_deadline` and `segment_deadline` are bounded by the Stop budget.
/// - `Stopping` → `Finished` or `Failed` when the controller returns.
///
/// Latches:
/// - `Lifecycle::stop`: Stop was requested; one-way, set only through
///   `Lifecycle::latch_stop`: by `request_stop` (a UI Stop, an indicator
///   failure, an attempt failure in `report_fault`), by the Stop hotkey, by a
///   source failure (`stop_for_source_failure`, `required_elevated_hook_failed`,
///   `commit_startup_source_failure`) and by session end.
/// - `clock.failed`: the clock or a boundary sample failed; one-way. It ends
///   source activity and refuses Complete publication.
/// - `source.change`: the admitted source must change at the next boundary;
///   set by `require_source_change`, cleared by `select_source` on reopening.
/// - `complete_publication`: `Awaiting` → `Authorized` or `Expired`; one-way.
/// - `bounds.closed` / `bounds.opened`: the current closing and opening boundaries.
/// - `pause.suspend`, `segment_deadline`: suspend and Stop bounds on video
///   finalization (D-5 A); suspend data is read only through `suspend_marks`.
/// - `reports.faults`: the fault log; its first attempt failure decides the
///   outcome (§6.2).
#[derive(Default)]
pub(super) struct Facts {
    /// The UI status. `Facts::status_view` adds the fields derived from the
    /// other facts.
    pub(super) status: SessionStatus,
    pub(super) bounds: Boundaries,
    pub(super) clock: ClockFacts,
    pub(super) pause: PauseFacts,
    pub(super) source: SourceFacts,
    pub(super) reports: Reports,
    pub(super) admission_check: AdmissionCheck,
    /// The suspend sequence read at the previous hotkey drain.
    pub(super) hotkey_suspends: u64,
    pub(super) hotkeys: ControlPrefix,
    pub(super) fixture_saves: VecDeque<FixtureNotice>,
    pub(super) next_save: u64,
    pub(super) disk: Option<disk::Sample>,
    pub(super) stop_deadline: Option<Instant>,
    pub(super) complete_publication: CompletePublication,
    pub(super) completed_prefix: Option<AdmissionBoundary>,
    /// The Stop and session-end bound on segment finalization and
    /// verification. Suspend never sets it (D-5 A).
    pub(super) segment_deadline: Option<Instant>,
}

/// The session clock and the admission boundaries.
#[derive(Default)]
pub(super) struct Boundaries {
    pub(super) origin: Option<ClockOrigin>,
    /// The current opening boundary.
    pub(super) opened: Option<AdmissionBoundary>,
    /// The current closing boundary: the pause cutoff, or the Stop boundary
    /// when Stop arrived while recording.
    pub(super) closed: Option<AdmissionBoundary>,
    pub(super) stop: Option<AdmissionBoundary>,
}

/// Clock samples for the attempt's manifest clock.
#[derive(Default)]
pub(super) struct ClockFacts {
    pub(super) utc_anchors: Vec<ClockAnchor>,
    pub(super) tick_anchor: Option<MessageTickAnchor>,
    pub(super) failed: bool,
}

#[derive(Default)]
pub(super) struct PauseFacts {
    /// The active pause reasons: the only record of them. The status view
    /// lists them in this order.
    pub(super) reasons: BTreeSet<PauseReason>,
    /// The current pause's producer prefix; replaced when a pause begins.
    pub(super) prefix: PausePrefix,
    /// Cleared when the pause reopens or every video producer finished.
    pub(super) suspend: SuspendBound,
    /// The previous pause's cutoff and interval reasons.
    pub(super) last: Option<(AdmissionBoundary, Vec<PauseReason>)>,
}

/// The producers' progress on one pause cutoff. It stays readable after the
/// pause reopens, until the next pause begins.
#[derive(Default)]
pub(super) struct PausePrefix {
    /// The producers that finished their prefix at the cutoff.
    pub(super) finished: BTreeSet<PauseProducer>,
    pub(super) video: BTreeMap<MonitorId, PausedVideo>,
    /// The cutoff whose input prefix the input pump closed.
    pub(super) input: Option<AdmissionBoundary>,
    /// Every reason that held during the pause interval.
    pub(super) reasons: BTreeSet<PauseReason>,
    /// The pause start and the suspend sequence read after it. The pause
    /// bound is measured with sleep excluded and never across an unknown suspend.
    pub(super) started: Option<AwakeMark>,
}

/// The suspend bounds of a pause that began with sleep (D-5 A).
#[derive(Default)]
pub(super) struct SuspendBound {
    /// The suspend sequence whose pause still waits for its video producers.
    pub(super) wait: Option<u64>,
    /// The suspend budget offered to the elevated collector's cleanup only.
    pub(super) cleanup: Option<Instant>,
}

/// Source selection and its evidence.
#[derive(Default)]
pub(super) struct SourceFacts {
    /// The latest admission epoch handed out.
    pub(super) epoch: u64,
    /// The admitted epoch whose source is the elevated collector.
    pub(super) elevated_epoch: Option<AdmissionEpoch>,
    pub(super) change: bool,
    pub(super) preparation: Option<SourcePreparation>,
    pub(super) ready: Option<SelectedSource>,
    pub(super) startup_target: Option<ForegroundApproval>,
    pub(super) evidence: SourceEvidence,
    pub(super) intervals: Vec<SelectedInterval>,
}

impl SourceFacts {
    /// Drop the pending source preparation and the source it selected.
    pub(super) fn cancel_preparation(&mut self) {
        self.preparation = None;
        self.ready = None;
    }
}

/// Failures and health conditions for the attempt's report.
#[derive(Default)]
pub(super) struct Reports {
    pub(super) video_failures: Vec<VideoFailure>,
    /// Capture and video health conditions for the context stream, in report order.
    pub(super) health: Vec<HealthRecord>,
    /// Conditions beyond `MAX_HEALTH_REPORTS`, counted in the manifest health block.
    pub(super) omitted_health: u64,
    pub(super) input_failures: Vec<InputFailure>,
    /// Attempt failures and activity losses in report order (§6.2).
    pub(super) faults: FaultLog,
}

#[derive(Default)]
pub(super) enum AdmissionCheck {
    #[default]
    Idle,
    Requested,
    Running,
    Finished(Result<ForegroundApproval, CaptureError>),
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum CompletePublication {
    #[default]
    Awaiting,
    Expired,
    Authorized,
}

#[derive(Default)]
pub(in crate::platform::windows) struct ControlPrefix {
    notices: VecDeque<ControlNotice>,
    stop: Option<ControlNotice>,
    pub before: Option<QpcTick>,
    pub loss: Option<ControlLoss>,
    pub failed: bool,
}

impl ControlPrefix {
    pub(super) fn push(&mut self, notice: ControlNotice) {
        if self.notices.len() < MAX_CONTROL_NOTICES {
            self.notices.push_back(notice);
        } else if notice.activation.action == RecorderAction::Stop && self.stop.is_none() {
            self.stop = Some(notice);
        } else {
            ControlLoss::record(&mut self.loss, notice.activation.at);
        }
    }

    /// Take the pending notices and loss; the progress mark and the failure stay.
    pub(super) fn take(&mut self) -> Self {
        let kept = Self { before: self.before, failed: self.failed, ..Self::default() };
        std::mem::replace(self, kept)
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

impl Facts {
    /// The session time now; none before the clock exists or when a sample fails.
    pub(super) fn now(&self) -> Option<SessionTimeUs> {
        self.bounds.origin.and_then(|origin| origin.time(clock::qpc().ok()?).ok())
    }

    /// The UI status with the fields derived from the other facts.
    pub(super) fn status_view(&self) -> SessionStatus {
        let mut status = self.status.clone();
        status.pause_reasons = self.pause.reasons.iter().copied().collect();
        status.final_publication_pending = status.phase == SessionPhase::Stopping
            && self.complete_publication == CompletePublication::Authorized
            && self.stop_deadline.is_some_and(|deadline| Instant::now() >= deadline);
        status
    }

    pub(super) fn check_clock(&self) -> Result<(), ProbeError> {
        if self.clock.failed {
            return Err(unavailable("The session clock or lifecycle controller failed"));
        }
        Ok(())
    }

    /// Record a fault in the controller's log. A lost activity becomes visible in the status at once.
    pub(super) fn record_fault(&mut self, report: FaultReport) {
        let now = self.now();
        if let Some(degraded) = self.reports.faults.record(report, now)
            && !self.status.degraded.contains(&degraded)
        {
            self.status.degraded.push(degraded);
        }
    }

    /// A capture or video health condition for the context stream; dropped
    /// before the session clock exists, counted beyond the bound.
    pub(super) fn push_health(&mut self, kind: HealthKind, failure: Failure, monitor: Option<MonitorId>, ongoing: bool) {
        let Some(at) = self.now() else { return };
        let health = HealthRecord { kind, failure, detected: at, first_affected: Some(at), ongoing, monitor, count: None };
        if !push_bounded(&mut self.reports.health, health, MAX_HEALTH_REPORTS) {
            self.reports.omitted_health = self.reports.omitted_health.saturating_add(1);
        }
    }
}
