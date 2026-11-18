mod elevated_lane;
mod gate;
mod provisional;
mod receiver;
mod resync;
pub(crate) use resync::on_input_desktop;
mod rows;
mod saves;
mod text;
mod writer;

use std::{
    collections::BTreeMap,
    ops::ControlFlow,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::json;

use crate::{
    capture::input::HeldBoundary,
    package::{diagnostics::ReportOutcome, journal::JournalIngress, metadata::{AttemptLayout, InputBlindReason, InputCapture, InputPublication, InputTailReason, SourceBlindReason, UncertifiedInputTail}, outcome::SuppressionClass},
    capture::save::{FixtureSaveError, SaveWindowContext},
    protocol::{MonitorId, QpcTick, RequestId, SessionId, SessionTimeUs, ipc::{ConfigureInput, HelperToCore, InputRegistration, elevated::runtime::CoverageReason}},
    platform::windows::controller::{Admission, ControlPrefix, PauseProducer, PausedVideo, Receipt, WorkerSession},
    session::{AdmissionBoundary, PauseReason, hotkey::ControlOutcome, labels::{input_tail_reason, pen_capture}},
};

use super::{ProbeError, RecordingReport, clock, context::{Context, FramePolicy}, display::topology::Topology, elevated::{InputStagingLoss, Selector}, input_helper::{HelperError, InputHelper, READY_WAIT}, io_error, still_rows, stills, unavailable};
use crate::package::finalize::video_failure_health;
use elevated_lane::drain_elevated_prefix;
use rows::{blind_pause_rows, discontinuity_row, pause_row, paused_slot_ranges, resume_row, staging_loss_row};
use saves::{ElevatedSaves, route_fixture};
pub(super) use writer::ReceiptPrefix;
use writer::{ContextWriter, InputTiming, InputWriter};

/// The bounded wait for producer progress past a pause cutoff (§6.3).
const PAUSE_CERTIFICATION_WAIT: Duration = Duration::from_millis(250);
/// Stop-budget share kept for helper cleanup, journal close and manifest
/// publication after the closing certification wait.
const STOP_FINALIZATION_RESERVE: Duration = Duration::from_secs(2);
/// The step of a bounded wait at a pause or stop boundary.
const BOUNDARY_WAIT_STEP: Duration = Duration::from_millis(2);

pub(super) fn with_journal<T>(
    layout: &AttemptLayout,
    frame_policy: &FramePolicy,
    input: JournalIngress,
    context: JournalIngress,
    session: &WorkerSession,
    topology: &Topology,
    still_endpoints: &BTreeMap<MonitorId, stills::Requests>,
    still_rows: &still_rows::Rows,
    reports: Option<&mpsc::SyncSender<RecordingReport>>,
    executable_versions: super::context::ExecutableVersions,
    record: impl FnOnce() -> Result<T, ProbeError>,
) -> Result<(T, InputPrefix), ProbeError> {
    thread::scope(|scope| {
        let stop_on_exit = StopOnExit(session);
        let (ready, initialized) = mpsc::sync_channel(1);
        let env = PumpEnv { layout, frame_policy, session, topology, still_endpoints, still_rows, reports };
        let worker = thread::Builder::new().name("input-receiver-context".into()).spawn_scoped(scope, move || {
            let result = pump(env, ready, input, context, executable_versions);
            if result.is_err() {
                session.request_stop();
            }
            result
        }).map_err(|error| io_error("Start the input worker", error))?;
        let video = match initialized.recv() {
            Ok(InputStartup::Ready | InputStartup::Cancelled) => Some(record()),
            Err(_) => None,
        };
        drop(stop_on_exit);
        let input = worker.join().map_err(|_| unavailable("The input worker terminated unexpectedly"))?;
        match video {
            Some(Err(error)) => Err(error),
            Some(Ok(video)) => input.map(|count| (video, count)),
            None => Err(input.err().unwrap_or_else(|| unavailable("The input worker is not ready"))),
        }
    })
}

struct StopOnExit<'a>(&'a WorkerSession);

impl Drop for StopOnExit<'_> {
    fn drop(&mut self) {
        self.0.request_stop();
    }
}

pub(super) struct InputPrefix {
    pub(super) cutoff: Option<AdmissionBoundary>,
    pub(super) input: ReceiptPrefix,
    pub(super) context: ReceiptPrefix,
    pub(super) video_health: Vec<MonitorId>,
    pub(super) capture: Option<InputCapture>,
    /// Cursor assets published by the context owner (M1-09 output inventory).
    pub(super) cursors: Option<Vec<crate::package::metadata::ImageFile>>,
    /// Capture health records already handed to the context ingress; later
    /// ones are written with the final context rows.
    pub(super) health_seen: usize,
}

impl InputPrefix {
    pub(super) fn cancelled() -> Self {
        Self {
            cutoff: None,
            input: ReceiptPrefix::default(),
            context: ReceiptPrefix::default(),
            video_health: Vec::new(),
            capture: None,
            cursors: Some(Vec::new()),
            health_seen: 0,
        }
    }
}

enum InputStartup {
    Ready,
    Cancelled,
}

fn pump(
    env: PumpEnv<'_>,
    ready: mpsc::SyncSender<InputStartup>,
    input: JournalIngress,
    context: JournalIngress,
    executable_versions: super::context::ExecutableVersions,
) -> Result<InputPrefix, ProbeError> {
    let PumpEnv { layout, frame_policy, session, topology, still_endpoints, still_rows, .. } = env;
    let session_id = SessionId::new(layout.id().as_str())
        .map_err(|_| unavailable("The input helper session identity is invalid"))?;
    let frequency = clock::frequency()?;
    let mut helper = InputHelper::start(ConfigureInput {
        session: session_id.clone(),
        attempt: layout.id().clone(),
        qpc_frequency: frequency,
        pen_hid: session.config().profile().pen_hid(),
    })?;
    session.update_standard_evidence(|evidence| evidence.launched = true);
    let Some(registration) = await_registration(&mut helper, session)? else {
        let _ = ready.try_send(InputStartup::Cancelled);
        return Ok(InputPrefix::cancelled());
    };
    let required_ready = registration.required_ready()
        && (!session.config().profile().pen_hid() || registration.pen_hid);
    session.input_ready(registration, required_ready);
    session.update_standard_evidence(|evidence| {
        evidence.registration_reported = true;
        evidence.required_registration_ready = required_ready;
    });
    if !required_ready {
        return Err(unavailable("The input helper did not confirm all required input registrations"));
    }
    let source = Selector::new(layout.id().clone(), session_id, frequency, session.config(), frame_policy, session)
        .map_err(|_| unavailable("Start foreground source selection"))?;
    let native_context = Context::start(layout, session.config(), topology, executable_versions)?;
    let input_settings = native_context.input_settings();
    let remote = remote_session();
    ready.try_send(InputStartup::Ready).map_err(|_| unavailable("The recording coordinator is unavailable"))?;
    let receiver = receiver::Receiver::new(session.hotkey_keys(), still_endpoints.clone(), still_rows.clone(),
        input_settings.known_controller_processes.clone().unwrap_or_default());
    let mut pump = Pump {
        env, elevated_saves: ElevatedSaves::default(), elevated_visit: None,
        journal_revision: session.journal_flush_revision(), receiver, health: HealthCursor::default(),
        native_context, source, helper: HelperSlot { retired: None, active: Some(helper) },
        context: ContextWriter::new(context), input: InputWriter::new(input),
    };
    let (cutoff, paused) = loop {
        let cutoff = pump.admitted()?;
        let Some(closed) = cutoff.filter(|cutoff| session.pause_pending(*cutoff)) else {
            break (cutoff, None);
        };
        if let Some(paused) = pump.pause(closed)? {
            break (Some(closed), Some(paused));
        }
    };
    pump.stop(cutoff, paused, registration, remote, input_settings)
}

/// A pause that lasted until Stop: its cutoff, its reasons and the paused
/// video slot ranges.
type PausedAtStop = (AdmissionBoundary, Vec<PauseReason>, Vec<PausedVideo>);

/// What every step of the input pump reads and never owns.
#[derive(Clone, Copy)]
struct PumpEnv<'a> {
    layout: &'a AttemptLayout,
    frame_policy: &'a FramePolicy,
    session: &'a WorkerSession,
    topology: &'a Topology,
    still_endpoints: &'a BTreeMap<MonitorId, stills::Requests>,
    still_rows: &'a still_rows::Rows,
    reports: Option<&'a mpsc::SyncSender<RecordingReport>>,
}

impl PumpEnv<'_> {
    /// Advance source selection; a failure fails and cancels the source. A
    /// running standard helper reports when it last attested its registrations.
    fn tick(self, source: &mut Selector<'_>, active: Option<AdmissionBoundary>, helper: &HelperSlot, receiver: &receiver::Receiver) {
        let ordinary = helper.is_live().then(|| receiver.helper_checked());
        if let Err(failure) = source.tick(self.topology, active, self.session.take_elevated_environment(), ordinary) {
            self.fail_source(source, failure);
        }
    }

    /// Fail and cancel the foreground source; its frames are no longer published.
    fn fail_source(self, source: &mut Selector<'_>, failure: screen_fix_elevated::Failure) {
        self.frame_policy.clear();
        self.session.fail_source(source.stage(), failure);
        source.cancel();
    }
}

/// The standard input helper, and the last one retired after a failure.
/// Fields drop in this order: the retired helper before the active one.
struct HelperSlot {
    retired: Option<InputHelper>,
    active: Option<InputHelper>,
}

impl HelperSlot {
    fn is_live(&self) -> bool {
        self.active.is_some()
    }

    /// Retire the active helper; it is kept until the pump ends.
    fn retire(&mut self) {
        if let Some(mut helper) = self.active.take() {
            helper.retire();
            self.retired = Some(helper);
        }
    }

    /// Hand the active helper the environment changes, then consume its
    /// records. Nothing without an active helper.
    fn poll_with_env(&mut self, session: &WorkerSession, consume: impl FnMut(HelperToCore)) -> Result<(), HelperError> {
        let Some(child) = &mut self.active else { return Ok(()) };
        child.notify(session.take_input_environment()).and_then(|()| child.poll(consume))
    }

    /// Poll the helper at a closing boundary. Records are accepted up to the
    /// cutoff while `admitted`; otherwise only the device inventory is kept.
    /// A failure retires the helper.
    fn poll_paused(
        &mut self, receiver: &mut receiver::Receiver, native: &mut Context<'_>, env: PumpEnv<'_>, poll: PausedPoll,
    ) -> Result<(), ProbeError> {
        let session = env.session;
        let mut accepted = Ok(());
        let result = self.poll_with_env(session, |record| {
            if poll.admitted {
                keep_first_error(&mut accepted, || receiver.accept(record, poll.receipt, Some(poll.cutoff), native));
            } else {
                receiver.collect_ordinary_inventory(record, poll.ordinary);
            }
        });
        accepted?;
        if let Err(error) = result {
            let at = session.session_time().unwrap_or(poll.cutoff.time);
            if poll.ordinary {
                // Once the pause prefix is published, the input stream holds no
                // runtime health row; the activity loss keeps its final row.
                note_helper_failure(session, env.reports, receiver, error, at, poll.admitted);
            } else {
                note_inactive_helper_failure(session, error, at);
            }
            self.retire();
        }
        Ok(())
    }
}

/// How the helper is polled while a closing boundary waits.
#[derive(Clone, Copy)]
struct PausedPoll {
    receipt: Receipt,
    cutoff: AdmissionBoundary,
    /// Records before the cutoff are still admitted.
    admitted: bool,
    /// The closing epoch is ordinary.
    ordinary: bool,
}

/// The runtime health already handed to the context ingress.
#[derive(Default)]
struct HealthCursor {
    /// The monitors of the video failures written as runtime health.
    video: Vec<MonitorId>,
    video_seen: usize,
    capture_seen: usize,
}

impl HealthCursor {
    /// Write the runtime health found since the last poll: each new video
    /// failure, and each new capture health record.
    fn publish(
        &mut self, session: &WorkerSession, context: &mut ContextWriter, receipt: Receipt, observed_at: QpcTick,
    ) -> Result<(), ProbeError> {
        for failed in session.video_failures_after(self.video_seen) {
            let Some(timing) = ContextWriter::timing(receipt, observed_at)? else { break };
            let detected = failed.at.unwrap_or(timing.time.recv_t_us);
            context.submit_health(video_failure_health(failed.failure, failed.monitor, detected))?;
            self.video_seen += 1;
            self.video.push(failed.monitor);
        }
        for record in session.health_after(self.capture_seen) {
            context.submit_health(record)?;
            self.capture_seen += 1;
        }
        Ok(())
    }
}

/// The input lane of an open admission interval.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// The elevated source owns the epoch.
    Elevated,
    /// The standard helper's input is admitted.
    OrdinaryLive,
    /// After the source-change latch: standard input is counted, not admitted (E1).
    Latched,
    /// Standard input is not admitted; only the device inventory is kept.
    Inactive,
}

impl Lane {
    fn select(elevated: bool, source: &Selector<'_>, session: &WorkerSession, receipt: Receipt) -> Self {
        if elevated {
            return Self::Elevated;
        }
        let epoch = receipt.boundary.epoch;
        let ordinary_live = source.ordinary_allowed() && session.ordinary_source_active(epoch);
        let latched = source.source_latched() && session.ordinary_source_active(epoch);
        if ordinary_live {
            Self::OrdinaryLive
        } else if latched {
            Self::Latched
        } else {
            Self::Inactive
        }
    }
}

/// How Stop closes the input prefix, decided once at its cutoff. Each
/// closed variant carries the foreground closing receipt of its cutoff.
enum Closing {
    /// No admitted epoch closed.
    NoAdmission,
    /// A pause lasted until Stop: the input prefix closed at the pause cutoff.
    WhilePaused { cutoff: AdmissionBoundary, receipt: Receipt, reasons: Vec<PauseReason>, ranges: Vec<PausedVideo> },
    /// Stop closed an ordinary epoch.
    Ordinary { cutoff: AdmissionBoundary, receipt: Receipt },
    /// Stop closed an elevated epoch.
    Elevated { cutoff: AdmissionBoundary, receipt: Receipt },
}

impl Closing {
    fn new(session: &WorkerSession, cutoff: Option<AdmissionBoundary>, paused: Option<PausedAtStop>) -> Result<Self, ProbeError> {
        let Some(cutoff) = cutoff else { return Ok(Self::NoAdmission) };
        // After Stop the closing facts, its tick anchor among them, no longer change.
        let receipt = session.foreground_closing_receipt(cutoff)?;
        Ok(match paused {
            Some((_, reasons, ranges)) => Self::WhilePaused { cutoff, receipt, reasons, ranges },
            None if session.elevated_epoch(cutoff.epoch) => Self::Elevated { cutoff, receipt },
            None => Self::Ordinary { cutoff, receipt },
        })
    }

    /// The closed cutoff and its receipt.
    fn closed(&self) -> Option<(AdmissionBoundary, Receipt)> {
        match self {
            Self::NoAdmission => None,
            Self::WhilePaused { cutoff, receipt, .. } | Self::Ordinary { cutoff, receipt } | Self::Elevated { cutoff, receipt } =>
                Some((*cutoff, *receipt)),
        }
    }

    /// The cutoff and receipt of an interval Stop closes while it is open.
    fn open(&self) -> Option<(AdmissionBoundary, Receipt)> {
        match self {
            Self::Ordinary { cutoff, receipt } | Self::Elevated { cutoff, receipt } => Some((*cutoff, *receipt)),
            Self::NoAdmission | Self::WhilePaused { .. } => None,
        }
    }

    /// The cutoff and receipt of an ordinary interval Stop closes; Stop then
    /// decides its prefix from the helper's own drain.
    fn ordinary(&self) -> Option<(AdmissionBoundary, Receipt)> {
        match self {
            Self::Ordinary { cutoff, receipt } => Some((*cutoff, *receipt)),
            _ => None,
        }
    }
}

/// The input pump's state across admission intervals. Fields drop in the
/// order the pump's locals did; each writer drops its pending flush outcome
/// with its ingress, after the helpers.
struct Pump<'a> {
    env: PumpEnv<'a>,
    elevated_saves: ElevatedSaves,
    /// The elevated visit the receiver and context currently apply.
    elevated_visit: Option<crate::protocol::ipc::elevated::VisitBinding>,
    journal_revision: u64,
    receiver: receiver::Receiver,
    health: HealthCursor,
    native_context: Context<'a>,
    source: Selector<'a>,
    helper: HelperSlot,
    context: ContextWriter,
    input: InputWriter,
}

impl Pump<'_> {
    /// Pump the open admission intervals until admission closes. The
    /// closing cutoff, if any.
    fn admitted(&mut self) -> Result<Option<AdmissionBoundary>, ProbeError> {
        loop {
            if let ControlFlow::Break(cutoff) = self.admit_once()? {
                return Ok(cutoff);
            }
            if let Some(cutoff) = self.env.session.wait_for_close(Duration::from_millis(5))? {
                return Ok(Some(cutoff));
            }
        }
    }

    /// One poll of the admission state: `Break` with the closing cutoff once
    /// admission closed.
    fn admit_once(&mut self) -> Result<ControlFlow<Option<AdmissionBoundary>>, ProbeError> {
        let session = self.env.session;
        let observed_at = clock::qpc()?;
        let receipt = match session.admit()? {
            Admission::Starting => {
                self.service_starting()?;
                return Ok(ControlFlow::Continue(()));
            }
            Admission::Closed(cutoff) => return Ok(ControlFlow::Break(cutoff)),
            Admission::Open(receipt) => receipt,
        };
        if observed_at < receipt.boundary.qpc {
            return Ok(ControlFlow::Continue(()));
        }
        self.env.tick(&mut self.source, Some(receipt.boundary), &self.helper, &self.receiver);
        let elevated = session.elevated_epoch(receipt.boundary.epoch);
        // The selector can receive evidence while servicing the peer.
        // Sample after that work under the controller admission lock.
        let (receipt, observed_at) = if elevated {
            match session.admit()? {
                Admission::Open(current) if current.boundary == receipt.boundary => (current, current.qpc),
                Admission::Closed(cutoff) => return Ok(ControlFlow::Break(cutoff)),
                _ => return Err(unavailable("The elevated input admission changed while servicing the source")),
            }
        } else {
            (receipt, observed_at)
        };
        let lane = Lane::select(elevated, &self.source, session, receipt);
        if lane != Lane::OrdinaryLive {
            self.drain_inactive_helper(lane, receipt)?;
        }
        if lane == Lane::OrdinaryLive && let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
            for event in self.native_context.poll(receipt, observed_at, self.env.frame_policy)? {
                self.context.event(event, timing)?;
            }
        }
        for notice in session.take_fixture_saves() {
            if lane == Lane::Elevated {
                self.elevated_saves.fixture(notice);
            } else {
                let result = if lane == Lane::OrdinaryLive {
                    route_fixture(&notice, self.env, &mut self.native_context, receipt, None)
                } else {
                    Err(FixtureSaveError::UnapprovedContext)
                };
                let _ = notice.reply.try_send(result);
            }
        }
        let controls = session.take_hotkeys();
        let wrote_control = controls.loss.is_some() || controls.notices().next().is_some();
        self.receiver.accept_controls(&controls, receipt)?;
        match lane {
            Lane::Elevated => {
                self.service_elevated(receipt, observed_at)?;
                if session.stop_requested() {
                    return Ok(ControlFlow::Continue(()));
                }
            }
            Lane::OrdinaryLive if self.helper.is_live() => self.service_ordinary(receipt)?,
            Lane::OrdinaryLive | Lane::Latched | Lane::Inactive => {}
        }
        self.publish_tick(lane, receipt, observed_at, controls, wrote_control)?;
        Ok(ControlFlow::Continue(()))
    }

    /// Before admission opens: the countdown keeps startup modifiers, and
    /// while a source switch is prepared only the device inventory is kept.
    fn service_starting(&mut self) -> Result<(), ProbeError> {
        let session = self.env.session;
        self.native_context.discard_paused_notifications();
        self.env.tick(&mut self.source, None, &self.helper, &self.receiver);
        if !self.helper.is_live() {
            return Ok(());
        }
        if self.source.switching() || session.source_preparation().is_some() || !self.source.ordinary_allowed() {
            if let Err(error) = self.helper.poll_with_env(session, |record| self.receiver.collect_ordinary_inventory(record, false)) {
                note_inactive_helper_failure(session, error, SessionTimeUs(0));
                self.helper.retire();
            }
        } else {
            self.helper.poll_with_env(session, |record| self.receiver.countdown(record))?;
        }
        Ok(())
    }

    /// Drain the standard helper outside an ordinary interval: after the
    /// source-change latch its input is counted, otherwise only the device
    /// inventory is kept.
    fn drain_inactive_helper(&mut self, lane: Lane, receipt: Receipt) -> Result<(), ProbeError> {
        let session = self.env.session;
        let mut drained = Ok(());
        let polled = self.helper.poll_with_env(session, |record| if lane == Lane::Latched {
            keep_first_error(&mut drained, || self.receiver.drain_after_latch(record, receipt));
        } else {
            self.receiver.collect_ordinary_inventory(record, lane != Lane::Elevated);
        });
        if let Err(error) = polled {
            let at = receipt.clock.time(receipt.qpc)?;
            note_inactive_helper_failure(session, error, at);
            self.helper.retire();
        }
        drained?;
        if lane != Lane::Elevated {
            self.env.frame_policy.clear();
        }
        Ok(())
    }

    /// Admit the standard helper's records of an ordinary interval and check
    /// producer liveness; a failure retires the helper.
    fn service_ordinary(&mut self, receipt: Receipt) -> Result<(), ProbeError> {
        let session = self.env.session;
        // Read before the records so progress carries a suspend count no
        // older than its own receipt (S0 marks).
        let power = session.suspend_marks();
        self.receiver.note_suspends(power.sequence);
        let mut accepted = Ok(());
        let polled = self.helper.poll_with_env(session, |record| {
            keep_first_error(&mut accepted, || self.receiver.accept(record, receipt, None, &mut self.native_context));
        });
        accepted?;
        let at = receipt.clock.time(receipt.qpc)?;
        self.receiver.advance(at);
        for producer in self.receiver.take_hook_reports() {
            note_helper_failure(session, self.env.reports, &mut self.receiver, HelperError::SuspectedHookLoss(producer), at, true);
        }
        let stalled = self.receiver.check_stall(receipt, power)?;
        let failure = polled.err().or_else(|| stalled.map(HelperError::ProducerStalled));
        if let Some(error) = failure {
            note_helper_failure(session, self.env.reports, &mut self.receiver, error, at, true);
            self.helper.retire();
            self.receiver.interrupt(at, HeldBoundary::AdmissionClosed);
        } else {
            self.receiver.try_synchronize(receipt, at, &self.native_context)?;
        }
        Ok(())
    }

    /// The controls, deferred Saves, runtime health, ordinary input and
    /// journal flushes of one poll of an open interval.
    fn publish_tick(
        &mut self, lane: Lane, receipt: Receipt, observed_at: QpcTick, controls: ControlPrefix, wrote_control: bool,
    ) -> Result<(), ProbeError> {
        let session = self.env.session;
        let epoch = receipt.boundary.epoch;
        let certified = self.source.open_visit().filter(|opened| opened.binding.epoch == epoch
            && self.source.current_visit() && session.elevated_active(epoch))
            .and_then(|opened| self.source.certified_before().map(|before| (opened.binding, before)));
        let checkpoint = |id, at| match lane {
            Lane::Elevated => {
                let selected = certified.filter(|(_, before)| at < *before
                    && session.elevated_active(epoch) && self.source.current_visit())
                    .and_then(|(binding, _)| self.native_context.checkpoint_context(at, Some(binding)));
                if selected.is_none() {
                    self.elevated_saves.checkpoint(id, at);
                }
                selected
            }
            Lane::OrdinaryLive if session.ordinary_source_active(epoch) => self.native_context.checkpoint_context(at, None),
            _ => None,
        };
        publish_controls(controls, receipt, observed_at, &mut self.context, checkpoint)?;
        if lane == Lane::Elevated {
            let certified = certified.filter(|_| session.elevated_active(epoch) && self.source.current_visit());
            self.elevated_saves.poll(certified, receipt, observed_at, self.env, &mut self.native_context, &mut self.context, false)?;
            if let Some((_, before)) = certified {
                self.receiver.advance_stills(receipt.clock.time(before)?, true);
            }
        }
        self.health.publish(session, &mut self.context, receipt, observed_at)?;
        let watermark = receipt.clock.time(observed_at)?;
        if lane == Lane::OrdinaryLive {
            self.publish_ordinary(receipt, observed_at, watermark)?;
        }
        publish_context_health(&mut self.native_context, &mut self.context, session, receipt, observed_at)?;
        self.context.offer_gap(watermark)?;
        let revision = session.journal_flush_revision();
        let closed_segment = self.journal_revision != revision;
        self.journal_revision = revision;
        self.input.flush(receipt.boundary.time, closed_segment)?;
        self.context.flush(watermark, closed_segment || wrote_control)
    }

    /// Publish the ordinary input prefix through `watermark`, with its source
    /// coverage, pointer rest, still progress and clipboard rows.
    fn publish_ordinary(&mut self, receipt: Receipt, observed_at: QpcTick, watermark: SessionTimeUs) -> Result<(), ProbeError> {
        self.stage_confirmed_text(receipt, observed_at);
        self.receiver.publish(&mut self.native_context, &mut self.input, receipt, watermark, false)?;
        if self.helper.is_live() && let Some(synchronized) = self.receiver.synchronization_qpc()
            && let Some(before) = self.receiver.unresolved_before()
        {
            let start = receipt.clock.time(synchronized.max(receipt.boundary.qpc))?;
            self.env.session.record_source_coverage(receipt.boundary.epoch, None, start, before.min(watermark));
        }
        self.receiver.poll_pointer_rest(&mut self.native_context, receipt, observed_at)?;
        self.receiver.advance_stills(watermark, self.helper.is_live());
        if let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
            for event in self.native_context.poll_clipboard(receipt, observed_at) {
                self.context.event(event, timing)?;
            }
        }
        Ok(())
    }

    /// Stage the confirmed text commits observed through `observed_at`, and
    /// the commits lost before confirmation.
    fn stage_confirmed_text(&mut self, receipt: Receipt, observed_at: QpcTick) {
        let (texts, losses) = self.native_context.take_confirmed_text(receipt, observed_at);
        for text in texts {
            self.receiver.stage_confirmed(text, receipt);
        }
        for loss in losses {
            self.receiver.note_loss(loss);
        }
    }

    /// Advance source selection, then poll the helper, while a boundary waits.
    fn tick_paused(&mut self, poll: PausedPoll) -> Result<(), ProbeError> {
        tick_paused(self.env, &mut self.source, &mut self.helper, &mut self.receiver, &mut self.native_context, poll)
    }

    /// Advance source selection until `done`; parked between steps while `pending`.
    fn wait_source(&mut self, mut done: impl FnMut(&Self) -> bool, pending: impl Fn(&Self) -> bool) {
        while !done(self) {
            self.env.tick(&mut self.source, None, &self.helper, &self.receiver);
            if pending(self) {
                thread::park_timeout(BOUNDARY_WAIT_STEP);
            }
        }
    }

    /// Advance source selection and poll the helper with `poll` until `done`.
    fn wait_paused(&mut self, poll: PausedPoll, mut done: impl FnMut(&Self) -> bool) -> Result<(), ProbeError> {
        while !done(self) {
            self.tick_paused(poll)?;
            thread::park_timeout(BOUNDARY_WAIT_STEP);
        }
        Ok(())
    }

    /// Pause at `closed`: certify the input prefix through the cutoff, write
    /// the pause rows, wait out the pause and write the resume rows. The pause
    /// when it lasted until Stop.
    fn pause(&mut self, closed: AdmissionBoundary) -> Result<Option<PausedAtStop>, ProbeError> {
        let env = self.env;
        let session = env.session;
        env.frame_policy.clear();
        self.native_context.discard_elevated_clipboard();
        if let Err(failure) = self.source.close(closed.qpc) {
            session.fail_source(self.source.stage(), failure);
            self.source.cancel();
        }
        let ordinary = !session.elevated_epoch(closed.epoch);
        let receipt = session.foreground_closing_receipt(closed)?;
        self.wait_source(|pump| !(!ordinary && pump.source.awaiting_close() && !session.stop_requested()),
            |pump| pump.source.awaiting_close());
        let until = Instant::now() + PAUSE_CERTIFICATION_WAIT;
        self.wait_source(|pump| !(!ordinary && session.pause_pending(closed) && !session.stop_requested()
            && !pump.source.closing_received(closed.qpc) && Instant::now() < until),
            |pump| !pump.source.closing_received(closed.qpc));
        self.wait_paused(PausedPoll { receipt, cutoff: closed, admitted: true, ordinary }, |pump| !(session.pause_pending(closed)
            && !pump.receiver.certified_through(closed.time) && Instant::now() < until && pump.helper.is_live() && ordinary))?;
        let certified = if ordinary { None } else {
            Some(drain_elevated_prefix(&mut self.source, &mut self.receiver, &mut self.native_context, receipt, closed, session)?)
        };
        // One final poll of the certified prefix resolves every deferred Save (E4).
        let saves_certified = self.source.input_certificate(Some(closed.qpc)).filter(|_| !ordinary)
            .map(|certificate| (certificate.visit, certificate.before));
        self.elevated_saves.poll(saves_certified, receipt, closed.qpc, env, &mut self.native_context, &mut self.context, true)?;
        let staging_loss = self.source.take_staging_loss();
        if staging_loss.is_some() { self.receiver.note_native_gap(); }
        let tail = describe_tail(&self.receiver, &mut self.source, session, receipt, closed, certified);
        let controls = session.take_hotkeys();
        let mut resume_chord = controls.notices().any(|notice| notice.outcome == ControlOutcome::ResumeRequested);
        self.receiver.accept_controls(&controls, receipt)?;
        if let Some(certified) = certified {
            self.receiver.interrupt_certified(certified, closed.time, HeldBoundary::Pause);
        } else {
            self.receiver.interrupt(closed.time, HeldBoundary::Pause);
        }
        // An uncertified ordinary tail is an explicit blind interval before the
        // pause; with it, the prefix up to the pause is certified (§6.3). Every
        // open input blind interval closes at the cutoff and joins the input
        // prefix published here.
        if let Some(tail) = tail.filter(|tail| tail.reason == Some(InputTailReason::ProducerProgress)) {
            self.receiver.open_blind(InputBlindReason::ProducerStall, tail.first_t_us);
        }
        self.receiver.close_blinds(closed.time);
        self.receiver.publish(&mut self.native_context, &mut self.input, receipt, closed.time, true)?;
        for notice in session.take_fixture_saves() {
            let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
        }
        self.native_context.pause(closed.qpc)?;
        let drain = PausedPoll { receipt, cutoff: closed, admitted: false, ordinary };
        self.input.flush_boundary(closed.time, session, || tick_paused(env, &mut self.source, &mut self.helper,
            &mut self.receiver, &mut self.native_context, drain))?;
        // The input prefix through the cutoff, with its pending gaps, is queued.
        self.input.pause();
        self.receiver.finish_still_prefix(closed.time);
        session.mark_input_prefix(closed);
        let timing = ContextWriter::timing(receipt, closed.qpc)?
            .ok_or_else(|| unavailable("The input pause cutoff is invalid"))?;
        if let Some(loss) = staging_loss {
            publish_staging_loss(&mut self.context, &loss, timing)?;
        }
        self.context.boundaries(self.native_context.close_hung_observation(), timing)?;
        let ranges = loop {
            let ranges = session.paused_video();
            if ranges.len() == session.config().monitors().len() || !session.pause_pending(closed) { break ranges; }
            self.tick_paused(drain)?;
            thread::park_timeout(BOUNDARY_WAIT_STEP);
        };
        let reasons = session.pause_interval_reasons(closed);
        let all_monitors = ranges.len() == session.config().monitors().len();
        let pause = pause_row(&reasons, closed.time, tail, &ranges, all_monitors, self.receiver.elevated_hook_loss())
            .map_err(|_| unavailable("Encode a pause row"))?;
        self.context.boundary(pause, timing)?;
        // Pending context gaps preceded the pause row; later ones follow the resume rows.
        self.context.pause();
        publish_controls(controls, receipt, closed.qpc, &mut self.context, |_, _| None)?;
        self.context.flush_boundary(closed.time, session, || tick_paused(env, &mut self.source, &mut self.helper,
            &mut self.receiver, &mut self.native_context, drain))?;
        session.finish_pause(closed, PauseProducer::Input);
        let mut discarded_context = 0_u64;
        // Video failures counted for this pause's resume row. Their detailed
        // runtime rows keep their original times and are written right after resume.
        let mut paused_video_counted = self.health.video_seen;
        let mut paused_health_counted = self.health.capture_seen;
        while session.pause_pending(closed) {
            let at = clock::qpc()?;
            self.helper.poll_paused(&mut self.receiver, &mut self.native_context, env, drain)?;
            env.tick(&mut self.source, None, &self.helper, &self.receiver);
            let discarded = self.native_context.discard_paused_notifications();
            discarded_context = discarded_context.saturating_add(discarded);
            self.context.record_suppressed(SuppressionClass::Pause, discarded);
            // Health from the pause is counted for the resume row, not written (PRIV-2).
            for failed in session.video_failures_after(paused_video_counted) {
                if let Some(at) = failed.at {
                    self.context.submit_health(video_failure_health(failed.failure, failed.monitor, at))?;
                }
                paused_video_counted += 1;
            }
            for record in session.health_after(paused_health_counted) {
                self.context.submit_health(record)?;
                paused_health_counted += 1;
            }
            if !session.pause_pending(closed) { break; }
            let controls = session.take_hotkeys();
            let wrote_control = controls.loss.is_some() || controls.notices().next().is_some();
            resume_chord |= controls.notices().any(|notice| notice.outcome == ControlOutcome::ResumeRequested);
            publish_controls(controls, receipt, at, &mut self.context, |_, _| None)?;
            let through = receipt.clock.time(at)?;
            self.context.flush(through, wrote_control)?;
            thread::park_timeout(Duration::from_millis(5));
        }
        let Some(opened) = session.wait_for_reopen(closed)? else {
            // Health counted during this pause was handed to the context ingress.
            self.health.capture_seen = paused_health_counted;
            return Ok(Some((closed, session.pause_interval_reasons(closed), ranges)));
        };
        let reasons = session.pause_interval_reasons(closed);
        let resumed = Receipt { qpc: opened.boundary.qpc, clock: opened.clock, boundary: opened.boundary, tick_anchor: None };
        let timing = ContextWriter::timing(resumed, opened.boundary.qpc)?
            .ok_or_else(|| unavailable("The input resume boundary is invalid"))?;
        let blind = blind_pause_rows(closed.time, opened.boundary.time, &reasons)
            .map_err(|_| unavailable("Encode an input blind row"))?;
        self.input.submit_boundary(opened.boundary.time, blind)?;
        let discontinuity = discontinuity_row(closed, opened.boundary.time, &reasons)
            .map_err(|_| unavailable("Encode a discontinuity row"))?;
        self.context.boundary(discontinuity, timing)?;
        let paused_slots = paused_slot_ranges(&ranges, closed.time, opened.boundary.time);
        let mut suppressed_health = self.context.take_paused_health();
        for (kind, count) in self.input.take_paused_health() {
            let total = suppressed_health.entry(kind).or_default();
            *total = total.saturating_add(count);
        }
        let resume = resume_row(&reasons, closed.time, &paused_slots, discarded_context, &suppressed_health)
            .map_err(|_| unavailable("Encode a resume row"))?;
        self.context.boundary(resume, timing)?;
        // Losses noticed during the pause now follow the resume row.
        self.context.resume();
        self.input.resume();
        self.native_context.resume();
        self.receiver.reopen(opened.boundary.time, resume_chord);
        if !session.elevated_epoch(opened.boundary.epoch) {
            self.receiver.select_source(None);
        }
        self.elevated_visit = None;
        Ok(None)
    }

    /// Stop at `cutoff`: drain and certify the input prefix, shut the helper
    /// down and write the closing rows.
    fn stop(
        self, cutoff: Option<AdmissionBoundary>, paused: Option<PausedAtStop>, registration: InputRegistration,
        remote: bool, input_settings: crate::capture::context::InputSettings,
    ) -> Result<InputPrefix, ProbeError> {
        // Bound in the pump's field order, so they drop as the pump's locals did.
        let Self {
            env, mut elevated_saves, mut receiver, health, mut native_context, mut source, mut helper, mut context,
            mut input, ..
        } = self;
        let session = env.session;
        env.frame_policy.clear();
        native_context.discard_elevated_clipboard();
        let stopped_qpc = cutoff.map_or_else(clock::qpc, |boundary| Ok(boundary.qpc))?;
        if let Err(failure) = source.stop(stopped_qpc, session.stop_deadline()) && cutoff.is_some() {
            session.source_finalization_failed(failure);
        }
        let staging_loss = source.take_staging_loss();
        if staging_loss.is_some() { receiver.note_native_gap(); }
        for notice in session.take_fixture_saves() {
            let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
        }
        let final_controls = session.take_hotkeys();
        let closing = Closing::new(session, cutoff, paused)?;
        if let Some((_, receipt)) = closing.open() {
            receiver.accept_controls(&final_controls, receipt)?;
        }
        if let Some((cutoff, receipt)) = closing.ordinary() && helper.is_live() {
            // §6.3: wait for producer progress past the cutoff, bounded by the
            // Stop drain deadline less the finalization reserve.
            let until = session.stop_deadline().map_or_else(|| Instant::now() + PAUSE_CERTIFICATION_WAIT,
                |deadline| deadline.checked_sub(STOP_FINALIZATION_RESERVE).unwrap_or(deadline));
            let poll = PausedPoll { receipt, cutoff, admitted: true, ordinary: true };
            while helper.is_live() && !receiver.certified_through(cutoff.time) && Instant::now() < until {
                helper.poll_paused(&mut receiver, &mut native_context, env, poll)?;
                thread::park_timeout(BOUNDARY_WAIT_STEP);
            }
        }
        if let Some(mut child) = helper.active.take() {
            let ordinary = closing.ordinary();
            let mut accepted = Ok(());
            let shutdown = child.notify(session.take_input_environment()).and_then(|()| child.shutdown(session.stop_deadline(), |record| {
                if let Some((cutoff, receipt)) = ordinary {
                    keep_first_error(&mut accepted, || receiver.accept(record, receipt, Some(cutoff), &mut native_context));
                }
            }));
            accepted?;
            if shutdown.is_ok() {
                session.update_standard_evidence(|evidence| evidence.stopped = true);
                if ordinary.is_some() {
                    // The helper drained every producer and exited normally.
                    receiver.close_producers();
                }
            }
            if let Err(error) = shutdown {
                match &closing {
                    Closing::NoAdmission => return Err(error.into()),
                    Closing::Elevated { cutoff, .. } => note_inactive_helper_failure(session, error, cutoff.time),
                    Closing::Ordinary { cutoff, .. } => note_helper_failure(session, env.reports, &mut receiver, error, cutoff.time, true),
                    Closing::WhilePaused { cutoff, .. } => if session.elevated_epoch(cutoff.epoch) {
                        note_inactive_helper_failure(session, error, cutoff.time);
                    } else {
                        note_helper_failure(session, env.reports, &mut receiver, error, cutoff.time, false);
                    },
                }
            }
        }
        let mut uncertified_closing_tail = None;
        if let Some((cutoff, receipt)) = closing.open() {
            let certified = if let Closing::Elevated { .. } = closing {
                Some(drain_elevated_prefix(&mut source, &mut receiver, &mut native_context, receipt, cutoff, session)?)
            } else { None };
            let saves_certified = source.input_certificate(Some(cutoff.qpc)).filter(|_| certified.is_some())
                .map(|certificate| (certificate.visit, certificate.before));
            elevated_saves.poll(saves_certified, receipt, cutoff.qpc, env, &mut native_context, &mut context, true)?;
            uncertified_closing_tail = describe_tail(&receiver, &mut source, session, receipt, cutoff, certified);
            if let Some(certified) = certified {
                receiver.interrupt_certified(certified, cutoff.time, HeldBoundary::Stop);
            } else {
                receiver.interrupt(cutoff.time, HeldBoundary::Stop);
            }
            // Reported before the final drain, so its input health row is written.
            for producer in receiver.take_hook_reports() {
                note_helper_failure(session, env.reports, &mut receiver, HelperError::SuspectedHookLoss(producer), cutoff.time, true);
            }
            // Open input blind intervals end at the cutoff (already closed at a pause).
            receiver.close_blinds(cutoff.time);
            receiver.publish(&mut native_context, &mut input, receipt, cutoff.time, true)?;
            input.flush_boundary(cutoff.time, session, || Ok(()))?;
            receiver.finish_still_prefix(cutoff.time);
            session.mark_input_prefix(cutoff);
        }
        elevated_saves.close();
        let (input_prefix, context_prefix) = if let Some((cutoff, receipt)) = closing.closed() {
            let stopped = session.stop_boundary().unwrap_or(cutoff);
            if input.published().is_some_and(|last| last > cutoff.time)
                || context.last_receipt().is_some_and(|last| last > stopped.qpc)
            {
                return Err(unavailable("An input or context record was received after its closing boundary"));
            }
            if closing.open().is_some() && let Some(timing) = ContextWriter::timing(receipt, cutoff.qpc)? {
                context.boundaries(native_context.close_hung_observation(), timing)?;
                for event in native_context.finish_cursor(cutoff.qpc, receipt.clock) {
                    context.event(event, timing)?;
                }
            }
            if let Some(timing) = ContextWriter::timing(receipt, stopped.qpc)? {
                if let Some(loss) = staging_loss {
                    publish_staging_loss(&mut context, &loss, timing)?;
                }
                let paused_slots = if let Closing::WhilePaused { reasons, ranges, .. } = &closing {
                    // The input stream closed at the pause cutoff: the blind rows of
                    // the pause up to Stop are admitted there.
                    let blind = blind_pause_rows(cutoff.time, stopped.time, reasons)
                        .map_err(|_| unavailable("Encode an input blind row"))?;
                    input.submit_boundary(cutoff.time, blind)?;
                    Some(paused_slot_ranges(ranges, cutoff.time, stopped.time))
                } else { None };
                let mut stop = json!({
                    "kind": "recorder_control", "command": "stop_boundary",
                    "cutoff_t_us": cutoff.time, "stop_t_us": stopped.time, "paused_slots": paused_slots,
                });
                if let Some(before) = receiver.elevated_hook_loss() {
                    stop["required_hook_first_uncovered_qpc"] = json!(before);
                }
                context.boundary(stop, timing)?;
            }
            publish_controls(final_controls, receipt, stopped.qpc, &mut context, |_, _| None)?;
            (input.seal(cutoff.time)?, context.seal(stopped.time)?)
        } else if input.published().is_some() || context.last_receipt().is_some() {
            return Err(unavailable("The admitted input prefix has no closing boundary"));
        } else {
            (input.into_prefix(), context.into_prefix())
        };
        let (devices, device_inventory_truncated) = receiver.finish_inventory()?;
        let pen = pen_capture(receiver.pen_observation(), registration, remote, devices.as_slice(), device_inventory_truncated);
        let hook_loss = receiver.hook_loss();
        Ok(InputPrefix {
            cutoff, input: input_prefix, context: context_prefix, video_health: health.video,
            capture: Some(InputCapture {
                registration, pen: Some(pen), publication: InputPublication::ApprovedReceiptGate, settings: Some(input_settings),
                transport_overflow: receiver.overflowed(), devices: Some(devices), device_inventory_truncated,
                keyboard_hook_loss_suspected: hook_loss.keyboard, pointer_hook_loss_suspected: hook_loss.pointer,
                uncertified_closing_tail, blind: None, optional_unavailable: receiver.optional_unavailable(),
            }),
            cursors: native_context.cursor_inventory(),
            health_seen: health.capture_seen,
        })
    }
}

/// One wait step at a closing boundary: advance source selection, then poll
/// the helper.
fn tick_paused(
    env: PumpEnv<'_>, source: &mut Selector<'_>, helper: &mut HelperSlot, receiver: &mut receiver::Receiver,
    native: &mut Context<'_>, poll: PausedPoll,
) -> Result<(), ProbeError> {
    env.tick(source, None, helper, receiver);
    helper.poll_paused(receiver, native, env, poll)
}

/// Wait up to [`READY_WAIT`] for the helper's registrations. `None` when the
/// session closed first; the helper is then shut down.
fn await_registration(helper: &mut InputHelper, session: &WorkerSession) -> Result<Option<InputRegistration>, ProbeError> {
    let deadline = Instant::now() + READY_WAIT;
    loop {
        if let Some(registration) = helper.poll_status()? {
            return Ok(Some(registration));
        }
        if let Some(code) = helper.exit_code()? {
            // The child may have published a reason immediately before exit.
            helper.poll_status()?;
            return Err(HelperError::UnexpectedExit(code).into());
        }
        if matches!(session.admit()?, Admission::Closed(_)) {
            helper.notify(session.take_input_environment())?;
            helper.shutdown(session.stop_deadline(), drop)?;
            session.update_standard_evidence(|evidence| evidence.stopped = true);
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(HelperError::ReadyTimeout.into());
        }
        let _ = session.wait_for_close(Duration::from_millis(5))?;
    }
}

fn remote_session() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    // SAFETY: GetSystemMetrics takes a metric index by value and has no other preconditions.
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

/// Run `step` unless an earlier step of the same batch failed; the batch
/// keeps its first error.
fn keep_first_error(first: &mut Result<(), ProbeError>, step: impl FnOnce() -> Result<(), ProbeError>) {
    if first.is_ok() {
        *first = step();
    }
}

fn describe_tail(
    receiver: &receiver::Receiver, source: &mut Selector<'_>, session: &WorkerSession,
    receipt: Receipt, cutoff: AdmissionBoundary, certified: Option<SessionTimeUs>,
) -> Option<UncertifiedInputTail> {
    if certified.is_some() {
        if receiver.synchronization_qpc().is_none() {
            source.note_blind(receipt.boundary.epoch, receipt.boundary.qpc, SourceBlindReason::ElevatedOpening(CoverageReason::Opening));
        }
        source.finish_blind(receipt.boundary, receipt.clock, cutoff.qpc);
    } else if let Some(synchronized) = receiver.synchronization_qpc()
        && let (Ok(start), Some(before)) = (receipt.clock.time(synchronized.max(receipt.boundary.qpc)), receiver.unresolved_before())
    {
        session.record_source_coverage(cutoff.epoch, None, start, before.min(cutoff.time));
    }
    let mut tail = receiver.uncertified_tail(receipt.boundary.time, cutoff.time, certified)?;
    if certified.is_some() && let Some(before) = session.last_elevated_coverage(cutoff.epoch) {
        tail.first_t_us = tail.first_t_us.max(before);
        if tail.first_t_us >= cutoff.time { return None; }
    }
    let failure = session.source_failure();
    tail.reason = Some(input_tail_reason(
        receiver.elevated_hook_loss().is_some(), failure, certified.is_some(), receiver.synchronization_qpc().is_some(),
    ));
    session.record_uncertified_tail(cutoff.epoch, tail);
    Some(tail)
}

/// The context `input_source_loss` row of a Recorder staging overflow (E3).
fn publish_staging_loss(context: &mut ContextWriter, loss: &InputStagingLoss, timing: InputTiming) -> Result<(), ProbeError> {
    let row = staging_loss_row(loss).map_err(|_| unavailable("Encode an input source loss row"))?;
    context.boundary(row, timing)
}

fn note_inactive_helper_failure(session: &WorkerSession, error: HelperError, at: SessionTimeUs) {
    // An inactive helper has no admitted interval to mark blind.
    let failure = error.runtime_failure(at, at);
    session.update_standard_evidence(|evidence| {
        evidence.inactive_transport_failure.get_or_insert(failure.reason);
    });
}

/// An input activity loss (§6.2 rows 10–12). Its blind interval starts at the
/// earliest missing receipt; `health` also writes the input health row.
fn note_helper_failure(
    session: &WorkerSession, reports: Option<&mpsc::SyncSender<RecordingReport>>, receiver: &mut receiver::Receiver,
    error: HelperError, at: SessionTimeUs, health: bool,
) {
    let first_missing = match &error {
        HelperError::SuspectedHookLoss(producer) => receiver.hook_lost_at(*producer).unwrap_or(at),
        _ => receiver.earliest_missing(at),
    };
    let failure = error.runtime_failure(at, first_missing);
    if !session.input_failed(failure) {
        return;
    }
    if health {
        receiver.note_failure_health(&failure);
    }
    if reports.is_none_or(|reports| reports.try_send(RecordingReport::Input(failure)).is_err()) {
        session.set_input_report_for(failure, ReportOutcome::Unavailable { native_code: None });
    }
}

fn publish_controls(
    controls: ControlPrefix,
    receipt: Receipt,
    observed: QpcTick,
    context: &mut ContextWriter,
    mut checkpoint: impl FnMut(RequestId, QpcTick) -> Option<SaveWindowContext>,
) -> Result<(), ProbeError> {
    let Some(timing) = ContextWriter::timing(receipt, observed)? else { return Ok(()) };
    if let Some(loss) = controls.loss {
        let event = json!({
            "kind": "recorder_control_gap",
            "index": context.next_index(),
            "admitted_100ns": timing.admitted_100ns,
            "dropped": loss.count,
            "first_control_receipt_t_us": receipt.clock.time(loss.first)?,
            "last_control_receipt_t_us": receipt.clock.time(loss.last)?,
        });
        context.boundary(event, timing)?;
    }
    for notice in controls.into_notices() {
        let activation = notice.activation;
        let request = matches!(notice.outcome, ControlOutcome::CheckpointRequested)
            .then(|| activation.checkpoint().id);
        let save_context = request.and_then(|id| checkpoint(id, activation.at));
        let event = json!({
            "kind": "recorder_control",
            "index": context.next_index(),
            "admitted_100ns": timing.admitted_100ns,
            "command": activation.action,
            "outcome": notice.outcome,
            "request_id": request,
            "save_context": save_context,
            "control_receipt_t_us": receipt.clock.time(activation.at)?,
        });
        context.boundary(event, timing)?;
    }
    Ok(())
}

/// Context losses as typed gap entries of the context stream, and context
/// health rows; an auxiliary activity loss is also an L-auxiliary report (C3).
fn publish_context_health(
    native: &mut Context<'_>, context: &mut ContextWriter, session: &WorkerSession, receipt: Receipt, observed: QpcTick,
) -> Result<(), ProbeError> {
    use crate::{package::{journal::HealthRecord, metadata::LostActivity}, session::fault::FaultReport};
    let Some(timing) = ContextWriter::timing(receipt, observed)? else { return Ok(()) };
    for loss in native.take_losses(receipt, observed) {
        context.record_loss(loss, timing.time.recv_t_us)?;
    }
    for health in native.take_health() {
        let at = receipt.clock.time(health.at.max(receipt.boundary.qpc))?;
        context.submit_health(HealthRecord {
            kind: health.kind, failure: health.failure, detected: at, first_affected: Some(at),
            ongoing: health.activity_lost, monitor: None, count: health.count,
        })?;
        if health.activity_lost {
            session.report_fault(FaultReport::activity(LostActivity::Auxiliary, health.failure, None, Some(at)));
        }
    }
    Ok(())
}
