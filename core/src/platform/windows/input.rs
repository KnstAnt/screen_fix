mod gate;
mod receiver;
mod resync;
pub(crate) use resync::on_input_desktop;
mod text;

use std::{
    collections::{BTreeMap, VecDeque},
    ops::ControlFlow,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use screen_fix_native_observation::LossSpan;

use crate::{
    capture::input::HeldBoundary,
    capture::save::{ConfirmationSource, ConfirmedSave, FixtureSaveError, FixtureSaveResult, MonitorRoutingBasis,
        SaveEvidence, SaveRouting, SaveWindowContext},
    package::{diagnostics::ReportOutcome, journal::{Flush, FlushRequest, JournalIngress, Submit}, metadata::{AttemptLayout, InputBlindReason, InputCapture, InputPublication, InputTailReason, SourceBlindReason, UncertifiedInputTail}, outcome::{HealthKind, LossEvidence, LossReason, SuppressionClass}, row::{BlindRow, RowEnvelope, RowKind}},
    protocol::{AdmissionEpoch, CoarseTime, MonitorId, MessageTickMs, QpcTick, RecordTime, RequestId, SessionId, SessionTimeUs,
        ipc::{ConfigureInput, InputRegistration, ProducerGap, elevated::{VisitBinding, runtime::CoverageReason}}},
    platform::windows::controller::{Admission, ControlPrefix, FixtureNotice, MAX_FIXTURE_SAVES, PauseProducer, PausedVideo, Receipt, WorkerSession},
    session::{AdmissionBoundary, PauseReason, hotkey::ControlOutcome, labels::{input_tail_reason, pause_blind_labels, pen_capture}},
};

use super::{ProbeError, RecordingReport, clock, context::{Context, FramePolicy}, elevated::{InputCertificate, InputStagingLoss, Selector}, input_helper::{HelperError, InputHelper, READY_WAIT}, io_error, still_rows, stills, unavailable};
use crate::package::finalize::video_failure_health;

/// The bounded wait for producer progress past a pause cutoff (§6.3).
const PAUSE_CERTIFICATION_WAIT: Duration = Duration::from_millis(250);
/// Stop-budget share kept for helper cleanup, journal close and manifest
/// publication after the closing certification wait.
const STOP_FINALIZATION_RESERVE: Duration = Duration::from_secs(2);
/// The bound of a lifecycle journal flush at a pause or stop boundary.
const BOUNDARY_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// An open elevated visit whose certified input lags its observation by this
/// much fails the source (witness).
const MAX_ELEVATED_CERTIFICATION_LAG_US: i64 = 250_000;
/// A deferred elevated Save not certified within this time is answered
/// without a selected context.
const DEFERRED_SAVE_TIMEOUT_US: i64 = 250_000;

pub(super) fn with_journal<T>(
    layout: &AttemptLayout,
    frame_policy: &FramePolicy,
    input: JournalIngress,
    context: JournalIngress,
    session: &WorkerSession,
    topology: &super::display::topology::Topology,
    still_endpoints: &BTreeMap<MonitorId, stills::Requests>,
    still_rows: &still_rows::Rows,
    reports: Option<&mpsc::SyncSender<RecordingReport>>,
    executable_versions: super::context::ExecutableVersions,
    record: impl FnOnce() -> Result<T, ProbeError>,
) -> Result<(T, InputPrefix), ProbeError> {
    thread::scope(|scope| {
        let stop_on_exit = StopOnExit(session);
        let (ready, initialized) = mpsc::sync_channel(1);
        let worker = thread::Builder::new().name("input-receiver-context".into()).spawn_scoped(scope, move || {
            let result = pump(layout, frame_policy, session, ready, input, context, topology, still_endpoints, still_rows, reports, executable_versions);
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

#[derive(Default)]
pub(super) struct ReceiptPrefix {
    pub(super) queued: u64,
    pub(super) last_queued: Option<SessionTimeUs>,
    last_receipt: Option<QpcTick>,
    last_time: Option<SessionTimeUs>,
}

impl ReceiptPrefix {
    fn advance(&mut self, received: QpcTick, admitted: SessionTimeUs, disposition: Submit) -> Result<(), ProbeError> {
        if self.last_receipt.is_some_and(|previous| previous > received) {
            return Err(unavailable("Input receipts are out of order"));
        }
        self.last_receipt = Some(received);
        self.advance_time(admitted, disposition)
    }

    /// A ledger admission raised the stream's receipt without adding a row.
    fn admit(&mut self, admitted: SessionTimeUs) -> Result<(), ProbeError> {
        if self.last_time.is_some_and(|previous| previous > admitted) {
            return Err(unavailable("Input receipts are out of order"));
        }
        self.last_time = Some(admitted);
        Ok(())
    }

    /// `at` raised to the published prefix.
    fn raised(&self, at: SessionTimeUs) -> SessionTimeUs {
        self.last_time.map_or(at, |last| last.max(at))
    }

    /// Submit lifecycle rows at `at`; each is queued and advances the prefix.
    fn submit_boundary(&mut self, output: &mut JournalIngress, at: SessionTimeUs, rows: Vec<serde_json::Value>) -> Result<(), ProbeError> {
        let count = rows.len();
        output.submit_boundary(at, rows)?;
        (0..count).try_for_each(|_| self.advance_time(at, Submit::Queued))
    }

    fn advance_time(&mut self, admitted: SessionTimeUs, disposition: Submit) -> Result<(), ProbeError> {
        self.admit(admitted)?;
        if disposition == Submit::Queued {
            self.queued = self.queued.checked_add(1)
                .ok_or_else(|| unavailable("The input record counter overflowed"))?;
            self.last_queued = Some(admitted);
        }
        Ok(())
    }
}

enum InputStartup {
    Ready,
    Cancelled,
}

fn pump(
    layout: &AttemptLayout,
    frame_policy: &FramePolicy,
    session: &WorkerSession,
    ready: mpsc::SyncSender<InputStartup>,
    input: JournalIngress,
    context: JournalIngress,
    topology: &super::display::topology::Topology,
    still_endpoints: &BTreeMap<MonitorId, stills::Requests>,
    still_rows: &still_rows::Rows,
    reports: Option<&mpsc::SyncSender<RecordingReport>>,
    executable_versions: super::context::ExecutableVersions,
) -> Result<InputPrefix, ProbeError> {
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
        layout, frame_policy, session, still_endpoints, still_rows, reports,
        sources: SourceService { session, topology, frame_policy },
        elevated_saves: ElevatedSaves::default(), elevated_visit: None,
        journal_revision: session.journal_flush_revision(),
        input_flush: DeferredFlush::default(), context_flush: DeferredFlush::default(),
        receiver, video_health: Vec::new(), video_health_seen: 0, capture_health_seen: 0,
        input_prefix: ReceiptPrefix::default(), context_prefix: ReceiptPrefix::default(), next_context_index: 0,
        native_context, source, retired_helper: None, helper: Some(helper), context, input,
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

/// The input pump's state across admission intervals. Fields drop in the
/// order the pump's locals did.
struct Pump<'a> {
    layout: &'a AttemptLayout,
    frame_policy: &'a FramePolicy,
    session: &'a WorkerSession,
    still_endpoints: &'a BTreeMap<MonitorId, stills::Requests>,
    still_rows: &'a still_rows::Rows,
    reports: Option<&'a mpsc::SyncSender<RecordingReport>>,
    sources: SourceService<'a>,
    elevated_saves: ElevatedSaves,
    elevated_visit: Option<VisitBinding>,
    journal_revision: u64,
    input_flush: DeferredFlush,
    context_flush: DeferredFlush,
    receiver: receiver::Receiver,
    video_health: Vec<MonitorId>,
    video_health_seen: usize,
    capture_health_seen: usize,
    input_prefix: ReceiptPrefix,
    context_prefix: ReceiptPrefix,
    next_context_index: u64,
    native_context: Context<'a>,
    source: Selector<'a>,
    retired_helper: Option<InputHelper>,
    helper: Option<InputHelper>,
    context: JournalIngress,
    input: JournalIngress,
}

impl<'a> Pump<'a> {
    /// Pump the open admission intervals until admission closes. The
    /// closing cutoff, if any.
    fn admitted(&mut self) -> Result<Option<AdmissionBoundary>, ProbeError> {
        let Self {
            layout, frame_policy, session, still_endpoints, still_rows, reports, sources,
            ref mut elevated_saves, ref mut elevated_visit, ref mut journal_revision, ref mut input_flush,
            ref mut context_flush, ref mut receiver, ref mut video_health, ref mut video_health_seen,
            ref mut capture_health_seen, ref mut input_prefix, ref mut context_prefix, ref mut next_context_index,
            ref mut native_context, ref mut source, ref mut retired_helper, ref mut helper, ref mut context,
            ref mut input,
        } = *self;
        loop {

            let observed_at = clock::qpc();
            let keep_pumping = (|| {
                let observed_at = observed_at?;
                let receipt = match session.admit()? {
                    Admission::Starting => {
                        native_context.discard_paused_notifications();
                        sources.tick(source, None, helper.as_ref(), &receiver);
                        if let Some(child) = helper {
                            if source.switching() || session.source_preparation().is_some() || !source.ordinary_allowed() {
                                if let Err(error) = child.notify(session.take_input_environment()).and_then(|()|
                                    child.poll(|record| receiver.collect_ordinary_inventory(record, false)))
                                {
                                    note_inactive_helper_failure(session, error, SessionTimeUs(0));
                                    retire_helper(helper, retired_helper);
                                }
                            } else {
                                child.notify(session.take_input_environment())?;
                                child.poll(|record| receiver.countdown(record))?;
                            }
                        }
                        return Ok(ControlFlow::Continue(()));
                    }
                    Admission::Closed(cutoff) => return Ok(ControlFlow::Break(cutoff)),
                    Admission::Open(receipt) => receipt,
                };
                if observed_at < receipt.boundary.qpc {
                    return Ok(ControlFlow::Continue(()));
                }
                sources.tick(source, Some(receipt.boundary), helper.as_ref(), &receiver);
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
                let ordinary_live = !elevated && source.ordinary_allowed()
                    && session.ordinary_source_active(receipt.boundary.epoch);
                // E1: after the source-change latch the standard records are counted, not discarded.
                let latched = !elevated && source.source_latched() && session.ordinary_source_active(receipt.boundary.epoch);
                if !ordinary_live {
                    let mut drained = Ok(());
                    if let Some(child) = helper
                        && let Err(error) = child.notify(session.take_input_environment()).and_then(|()|
                            child.poll(|record| if latched {
                                keep_first_error(&mut drained, || receiver.drain_after_latch(record, receipt));
                            } else {
                                receiver.collect_ordinary_inventory(record, !elevated);
                            }))
                    {
                        let at = receipt.clock.time(receipt.qpc)?;
                        note_inactive_helper_failure(session, error, at);
                        retire_helper(helper, retired_helper);
                    }
                    drained?;
                    if !elevated { frame_policy.clear(); }
                }
                if ordinary_live && let Some(timing) = input_timing(receipt, observed_at, None)? {
                    for event in native_context.poll(receipt, observed_at, frame_policy)? {
                        send_event(context, event, timing, context_prefix, next_context_index)?;
                    }
                }
                for notice in session.take_fixture_saves() {
                    if elevated {
                        elevated_saves.fixture(notice);
                    } else {
                        let result = if ordinary_live {
                            route_fixture(&notice, session, layout, native_context, still_endpoints, still_rows, receipt, None)
                        } else {
                            Err(FixtureSaveError::UnapprovedContext)
                        };
                        let _ = notice.reply.try_send(result);
                    }
                }
                let controls = session.take_hotkeys();
                let wrote_control = controls.loss.is_some() || controls.notices().next().is_some();
                receiver.accept_controls(&controls, receipt)?;
                if elevated {
                    poll_elevated(source, elevated_visit, receiver, native_context,
                        input, input_prefix, context, context_prefix, next_context_index,
                        frame_policy, session, receipt, observed_at)?;
                    if session.stop_requested() {
                        return Ok(ControlFlow::Continue(()));
                    }
                } else if ordinary_live && let Some(child) = helper {
                    // Read before the records so progress carries a suspend count no
                    // older than its own receipt (S0 marks).
                    let power = session.suspend_marks();
                    receiver.note_suspends(power.sequence);
                    let mut accepted = Ok(());
                    let polled = child.notify(session.take_input_environment()).and_then(|()| child.poll(|record| {
                        keep_first_error(&mut accepted, || receiver.accept(record, receipt, None, native_context));
                    }));
                    accepted?;
                    let at = receipt.clock.time(receipt.qpc)?;
                    receiver.advance(at);
                    for producer in receiver.take_hook_reports().into_iter().flatten() {
                        note_helper_failure(session, reports, receiver, HelperError::SuspectedHookLoss(producer), at, true);
                    }
                    let stalled = receiver.check_stall(receipt, power)?;
                    let failure = polled.err().or_else(|| stalled.map(HelperError::ProducerStalled));
                    if let Some(error) = failure {
                        note_helper_failure(session, reports, receiver, error, at, true);
                        retire_helper(helper, retired_helper);
                        receiver.interrupt(at, HeldBoundary::AdmissionClosed);
                    } else {
                        receiver.try_synchronize(receipt, at, &native_context)?;
                    }
                }
                let certified = source.open_visit().filter(|opened| opened.binding.epoch == receipt.boundary.epoch
                    && source.current_visit() && session.elevated_active(receipt.boundary.epoch))
                    .and_then(|opened| source.certified_before().map(|before| (opened.binding, before)));
                let checkpoint = |id, at| {
                    if elevated {
                        let selected = certified.filter(|(_, before)| at < *before
                            && session.elevated_active(receipt.boundary.epoch) && source.current_visit())
                            .and_then(|(binding, _)| native_context.checkpoint_context(at, Some(binding)));
                        if selected.is_none() {
                            elevated_saves.checkpoint(id, at);
                        }
                        selected
                    } else if ordinary_live && session.ordinary_source_active(receipt.boundary.epoch) {
                        native_context.checkpoint_context(at, None)
                    } else {
                        None
                    }
                };
                publish_controls(controls, receipt, observed_at,
                    context, context_prefix, next_context_index, checkpoint)?;
                if elevated {
                    let certified = certified.filter(|_| session.elevated_active(receipt.boundary.epoch) && source.current_visit());
                    elevated_saves.poll(certified, receipt, observed_at, session, layout, native_context,
                        still_endpoints, still_rows, context, context_prefix, next_context_index, false)?;
                    if let Some((_, before)) = certified {
                        receiver.advance_stills(receipt.clock.time(before)?, true);
                    }
                }
                publish_runtime_health(session, context, receipt, observed_at,
                    video_health, video_health_seen, capture_health_seen)?;
                let watermark = receipt.clock.time(observed_at)?;
                if ordinary_live {
                    let (texts, losses) = native_context.take_confirmed_text(receipt, observed_at);
                    for text in texts {
                        receiver.stage_confirmed(text, receipt);
                    }
                    for loss in losses {
                        receiver.note_loss(loss);
                    }
                    receiver.publish(native_context, input, input_prefix, receipt, watermark, false)?;
                    if helper.is_some() && let Some(synchronized) = receiver.synchronization_qpc()
                        && let Some(before) = receiver.unresolved_before()
                    {
                        let start = receipt.clock.time(synchronized.max(receipt.boundary.qpc))?;
                        session.record_source_coverage(receipt.boundary.epoch, None, start, before.min(watermark));
                    }
                    receiver.poll_pointer_rest(native_context, receipt, observed_at)?;
                    receiver.advance_stills(watermark, helper.is_some());
                    if let Some(timing) = input_timing(receipt, observed_at, None)? {
                        for event in native_context.poll_clipboard(receipt, observed_at) {
                            send_event(context, event, timing, context_prefix, next_context_index)?;
                        }
                    }
                }
                publish_context_health(native_context, context, &context_prefix, session, receipt, observed_at)?;
                context.offer_gap(watermark)?;
                let revision = session.journal_flush_revision();
                let closed_segment = *journal_revision != revision;
                *journal_revision = revision;
                // Pending input losses are admitted at the published prefix, never
                // ahead of records that the matcher or gate still holds.
                let input_flush_at = input_prefix.last_time.unwrap_or(receipt.boundary.time);
                input_flush.poll(input, input_flush_at, closed_segment)?;
                context_flush.poll(context, watermark, closed_segment || wrote_control)?;
                Ok::<_, ProbeError>(ControlFlow::Continue(()))
            })();

            if let ControlFlow::Break(cutoff) = keep_pumping? {
                return Ok(cutoff);
            }
            if let Some(cutoff) = session.wait_for_close(Duration::from_millis(5))? {
                return Ok(Some(cutoff));
            }
        }
    }

    /// Pause at `closed`: certify the input prefix through the cutoff, write
    /// the pause rows, wait out the pause and write the resume rows. The pause
    /// when it lasted until Stop.
    fn pause(&mut self, closed: AdmissionBoundary) -> Result<Option<PausedAtStop>, ProbeError> {
        let Self {
            layout, frame_policy, session, still_endpoints, still_rows, reports, sources,
            ref mut elevated_saves, ref mut elevated_visit, ref mut context_flush, ref mut receiver,
            ref mut video_health_seen, ref mut capture_health_seen, ref mut input_prefix, ref mut context_prefix,
            ref mut next_context_index, ref mut native_context, ref mut source, ref mut retired_helper,
            ref mut helper, ref mut context, ref mut input, ..
        } = *self;
        frame_policy.clear();
        native_context.discard_elevated_clipboard();
        if let Err(failure) = source.close(closed.qpc) {
            session.fail_source(source.stage(), failure);
            source.cancel();
        }
        let ordinary = !session.elevated_epoch(closed.epoch);
        let receipt = session.foreground_closing_receipt(closed)?;
        while !ordinary && source.awaiting_close() && !session.stop_requested() {
            sources.tick(source, None, helper.as_ref(), &receiver);
            if source.awaiting_close() {
                thread::park_timeout(Duration::from_millis(2));
            }
        }
        let until = Instant::now() + PAUSE_CERTIFICATION_WAIT;
        while !ordinary && session.pause_pending(closed) && !session.stop_requested()
            && !source.closing_received(closed.qpc) && Instant::now() < until
        {
            sources.tick(source, None, helper.as_ref(), &receiver);
            if !source.closing_received(closed.qpc) {
                thread::park_timeout(Duration::from_millis(2));
            }
        }
        while session.pause_pending(closed) && !receiver.certified_through(closed.time)
            && Instant::now() < until && helper.is_some() && ordinary
        {
            sources.tick(source, None, helper.as_ref(), &receiver);
            poll_paused_helper(helper, retired_helper, receiver, native_context,
                session, reports, receipt, closed, true, ordinary)?;
            thread::park_timeout(Duration::from_millis(2));
        }
        let certified = if ordinary { None } else {
            Some(drain_elevated_prefix(source, receiver, native_context, receipt, closed, session)?)
        };
        // One final poll of the certified prefix resolves every deferred Save (E4).
        let saves_certified = source.input_certificate(Some(closed.qpc)).filter(|_| !ordinary)
            .map(|certificate| (certificate.visit, certificate.before));
        elevated_saves.poll(saves_certified, receipt, closed.qpc, session, layout, native_context,
            still_endpoints, still_rows, context, context_prefix, next_context_index, true)?;
        let staging_loss = source.take_staging_loss();
        if staging_loss.is_some() { receiver.note_native_gap(); }
        let tail = describe_tail(&receiver, source, session, receipt, closed, certified);
        let controls = session.take_hotkeys();
        let mut resume_chord = controls.notices().any(|notice| notice.outcome == ControlOutcome::ResumeRequested);
        receiver.accept_controls(&controls, receipt)?;
        if let Some(certified) = certified {
            receiver.interrupt_certified(certified, closed.time, HeldBoundary::Pause);
        } else {
            receiver.interrupt(closed.time, HeldBoundary::Pause);
        }
        // An uncertified ordinary tail is an explicit blind interval before the
        // pause; with it, the prefix up to the pause is certified (§6.3). Every
        // open input blind interval closes at the cutoff and joins the input
        // prefix published here.
        if let Some(tail) = tail.filter(|tail| tail.reason == Some(InputTailReason::ProducerProgress)) {
            receiver.open_blind(InputBlindReason::ProducerStall, tail.first_t_us);
        }
        receiver.close_blinds(closed.time);
        receiver.publish(native_context, input, input_prefix, receipt, closed.time, true)?;
        for notice in session.take_fixture_saves() {
            let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
        }
        native_context.pause(closed.qpc)?;
        flush_boundary(input, closed.time, session, || {
            sources.tick(source, None, helper.as_ref(), &receiver);
            poll_paused_helper(helper, retired_helper, receiver, native_context,
                session, reports, receipt, closed, false, ordinary)
        })?;
        // The input prefix through the cutoff, with its pending gaps, is queued.
        input.pause();
        receiver.finish_still_prefix(closed.time);
        session.mark_input_prefix(closed);
        let timing = input_timing(receipt, closed.qpc, None)?
            .ok_or_else(|| unavailable("The input pause cutoff is invalid"))?;
        if let Some(loss) = staging_loss {
            publish_input_staging_loss(context, loss, timing, context_prefix, next_context_index)?;
        }
        send_boundary_events(context, native_context.close_hung_observation(), timing,
            context_prefix, next_context_index)?;
        let ranges = loop {
            let ranges = session.paused_video();
            if ranges.len() == session.config().monitors().len() || !session.pause_pending(closed) { break ranges; }
            sources.tick(source, None, helper.as_ref(), &receiver);
            poll_paused_helper(helper, retired_helper, receiver, native_context,
                session, reports, receipt, closed, false, ordinary)?;
            thread::park_timeout(Duration::from_millis(2));
        };
        let reasons = session.pause_interval_reasons(closed);
        let all_monitors = ranges.len() == session.config().monitors().len();
        let pause = pause_row(&reasons, closed.time, tail, &ranges, all_monitors, receiver.elevated_hook_loss())
            .map_err(|_| unavailable("Encode a pause row"))?;
        send_boundary_event(context, pause, timing, context_prefix, next_context_index)?;
        // Pending context gaps preceded the pause row; later ones follow the resume rows.
        context.pause();
        publish_controls(controls, receipt, closed.qpc, context, context_prefix,
            next_context_index, |_, _| None)?;
        flush_boundary(context, closed.time, session, || {
            sources.tick(source, None, helper.as_ref(), &receiver);
            poll_paused_helper(helper, retired_helper, receiver, native_context,
                session, reports, receipt, closed, false, ordinary)
        })?;
        session.finish_pause(closed, PauseProducer::Input);
        let mut discarded_context = 0_u64;
        // Video failures counted for this pause's resume row. Their detailed
        // runtime rows keep their original times and are written right after resume.
        let mut paused_video_counted = *video_health_seen;
        let mut paused_health_counted = *capture_health_seen;
        while session.pause_pending(closed) {
            let at = clock::qpc()?;
            poll_paused_helper(helper, retired_helper, receiver, native_context,
                session, reports, receipt, closed, false, ordinary)?;
            sources.tick(source, None, helper.as_ref(), &receiver);
            let discarded = native_context.discard_paused_notifications();
            discarded_context = discarded_context.saturating_add(discarded);
            context.record_suppressed(SuppressionClass::Pause, discarded);
            // Health from the pause is counted for the resume row, not written (PRIV-2).
            for failed in session.video_failures_after(paused_video_counted) {
                if let Some(at) = failed.at {
                    context.submit_health(video_failure_health(failed.failure, failed.monitor, at))
                        ?;
                }
                paused_video_counted += 1;
            }
            for record in session.health_after(paused_health_counted) {
                context.submit_health(record)?;
                paused_health_counted += 1;
            }
            if !session.pause_pending(closed) { break; }
            let controls = session.take_hotkeys();
            let wrote_control = controls.loss.is_some() || controls.notices().next().is_some();
            resume_chord |= controls.notices().any(|notice| notice.outcome == ControlOutcome::ResumeRequested);
            publish_controls(controls, receipt, at, context, context_prefix,
                next_context_index, |_, _| None)?;
            let through = receipt.clock.time(at)?;
            context_flush.poll(context, through, wrote_control)?;
            thread::park_timeout(Duration::from_millis(5));
        }
        let Some(opened) = session.wait_for_reopen(closed)? else {
            // Health counted during this pause was handed to the context ingress.
            *capture_health_seen = paused_health_counted;
            return Ok(Some((closed, session.pause_interval_reasons(closed), ranges)));
        };
        let reasons = session.pause_interval_reasons(closed);
        let resumed = Receipt { qpc: opened.boundary.qpc, clock: opened.clock, boundary: opened.boundary, tick_anchor: None };
        let timing = input_timing(resumed, opened.boundary.qpc, None)?
            .ok_or_else(|| unavailable("The input resume boundary is invalid"))?;
        publish_blind_pause(input, input_prefix, closed.time, opened.boundary.time, opened.boundary.time, &reasons)?;
        let discontinuity = discontinuity_row(closed, opened.boundary.time, &reasons)
            .map_err(|_| unavailable("Encode a discontinuity row"))?;
        send_boundary_event(context, discontinuity, timing, context_prefix, next_context_index)?;
        let paused_slots = paused_slot_ranges(&ranges, closed.time, opened.boundary.time);
        let mut suppressed_health = context.take_paused_health();
        for (kind, count) in input.take_paused_health() {
            let total = suppressed_health.entry(kind).or_default();
            *total = total.saturating_add(count);
        }
        let resume = resume_row(&reasons, closed.time, &paused_slots, discarded_context, &suppressed_health)
            .map_err(|_| unavailable("Encode a resume row"))?;
        send_boundary_event(context, resume, timing, context_prefix, next_context_index)?;
        // Losses noticed during the pause now follow the resume row.
        context.resume();
        input.resume();
        native_context.resume();
        receiver.reopen(opened.boundary.time, resume_chord);
        if !session.elevated_epoch(opened.boundary.epoch) {
            receiver.select_source(None);
        }
        *elevated_visit = None;
        Ok(None)
    }

    /// Stop at `cutoff`: drain and certify the input prefix, shut the helper
    /// down and write the closing rows.
    fn stop(
        self, cutoff: Option<AdmissionBoundary>, paused: Option<PausedAtStop>, registration: InputRegistration,
        remote: bool, input_settings: crate::capture::context::InputSettings,
    ) -> Result<InputPrefix, ProbeError> {
        let Self {
            layout, frame_policy, session, still_endpoints, still_rows, reports, mut elevated_saves, mut receiver,
            video_health, capture_health_seen, mut input_prefix, mut context_prefix, mut next_context_index,
            mut native_context, mut source, mut retired_helper, mut helper, mut context, mut input, ..
        } = self;
        frame_policy.clear();
        native_context.discard_elevated_clipboard();
        let stopped_qpc = cutoff.map_or_else(clock::qpc, |boundary| Ok(boundary.qpc))?;
        if let Err(failure) = source.stop(stopped_qpc, session.stop_deadline()) {
            if cutoff.is_some() {
                session.source_finalization_failed(failure);
            }
        }
        let staging_loss = source.take_staging_loss();
        if staging_loss.is_some() { receiver.note_native_gap(); }
        for notice in session.take_fixture_saves() {
            let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
        }
        let final_controls = session.take_hotkeys();
        if let Some(cutoff) = cutoff.filter(|_| paused.is_none()) {
            receiver.accept_controls(&final_controls, session.foreground_closing_receipt(cutoff)?)?;
        }
        let decides_stop = paused.is_none() && cutoff.is_some_and(|cutoff| !session.elevated_epoch(cutoff.epoch));
        if decides_stop && helper.is_some() && let Some(cutoff) = cutoff {
            // §6.3: wait for producer progress past the cutoff, bounded by the
            // Stop drain deadline less the finalization reserve.
            let receipt = session.foreground_closing_receipt(cutoff)?;
            let until = session.stop_deadline().map_or_else(|| Instant::now() + PAUSE_CERTIFICATION_WAIT,
                |deadline| deadline.checked_sub(STOP_FINALIZATION_RESERVE).unwrap_or(deadline));
            while helper.is_some() && !receiver.certified_through(cutoff.time) && Instant::now() < until {
                poll_paused_helper(&mut helper, &mut retired_helper, &mut receiver, &mut native_context,
                    session, reports, receipt, cutoff, true, true)?;
                thread::park_timeout(Duration::from_millis(2));
            }
        }
        if let Some(mut child) = helper {
            let closed = cutoff.map(|boundary| session.foreground_closing_receipt(boundary)).transpose()?;
            let mut accepted = Ok(());
            let shutdown = child.notify(session.take_input_environment()).and_then(|()| child.shutdown(session.stop_deadline(), |record| {
                if paused.is_none() && let (Some(receipt), Some(cutoff)) = (closed, cutoff)
                    && !session.elevated_epoch(cutoff.epoch)
                {
                    keep_first_error(&mut accepted, || receiver.accept(record, receipt, Some(cutoff), &mut native_context));
                }
            }));
            accepted?;
            if shutdown.is_ok() {
                session.update_standard_evidence(|evidence| evidence.stopped = true);
                if decides_stop {
                    // The helper drained every producer and exited normally.
                    receiver.close_producers();
                }
            }
            if let Err(error) = shutdown {
                if let Some(cutoff) = cutoff {
                    if session.elevated_epoch(cutoff.epoch) {
                        note_inactive_helper_failure(session, error, cutoff.time);
                    } else {
                        note_helper_failure(session, reports, &mut receiver, error, cutoff.time, paused.is_none());
                    }
                } else {
                    return Err(error.into());
                }
            }
        }
        let mut uncertified_closing_tail = None;
        if let Some(cutoff) = cutoff.filter(|_| paused.is_none()) {
            let receipt = session.foreground_closing_receipt(cutoff)?;
            let certified = if session.elevated_epoch(cutoff.epoch) {
                Some(drain_elevated_prefix(&mut source, &mut receiver, &mut native_context, receipt, cutoff, session)?)
            } else { None };
            let saves_certified = source.input_certificate(Some(cutoff.qpc)).filter(|_| certified.is_some())
                .map(|certificate| (certificate.visit, certificate.before));
            elevated_saves.poll(saves_certified, receipt, cutoff.qpc, session, layout, &mut native_context,
                still_endpoints, still_rows, &mut context, &mut context_prefix, &mut next_context_index, true)?;
            uncertified_closing_tail = describe_tail(&receiver, &mut source, session, receipt, cutoff, certified);
            if let Some(certified) = certified {
                receiver.interrupt_certified(certified, cutoff.time, HeldBoundary::Stop);
            } else {
                receiver.interrupt(cutoff.time, HeldBoundary::Stop);
            }
            // Reported before the final drain, so its input health row is written.
            for producer in receiver.take_hook_reports().into_iter().flatten() {
                note_helper_failure(session, reports, &mut receiver, HelperError::SuspectedHookLoss(producer), cutoff.time, true);
            }
            // Open input blind intervals end at the cutoff (already closed at a pause).
            receiver.close_blinds(cutoff.time);
            receiver.publish(&mut native_context, &mut input, &mut input_prefix,
                session.foreground_closing_receipt(cutoff)?, cutoff.time, true)?;
            flush_boundary(&mut input, cutoff.time, session, || Ok(()))?;
            receiver.finish_still_prefix(cutoff.time);
            session.mark_input_prefix(cutoff);
        }
        elevated_saves.close();
        if let Some(cutoff) = cutoff {
            let receipt = session.foreground_closing_receipt(cutoff)?;
            let stopped = session.stop_boundary().unwrap_or(cutoff);
            if input_prefix.last_time.is_some_and(|last| last > cutoff.time)
                || context_prefix.last_receipt.is_some_and(|last| last > stopped.qpc)
            {
                return Err(unavailable("An input or context record was received after its closing boundary"));
            }
            if paused.is_none() && let Some(timing) = input_timing(receipt, cutoff.qpc, None)? {
                send_boundary_events(&mut context, native_context.close_hung_observation(), timing,
                    &mut context_prefix, &mut next_context_index)?;
                for event in native_context.finish_cursor(cutoff.qpc, receipt.clock) {
                    send_event(&mut context, event, timing, &mut context_prefix, &mut next_context_index)?;
                }
            }
            if let Some(timing) = input_timing(receipt, stopped.qpc, None)? {
                if let Some(loss) = staging_loss {
                    publish_input_staging_loss(&mut context, loss, timing, &mut context_prefix, &mut next_context_index)?;
                }
                let paused_slots = if let Some((closed, reasons, ranges)) = paused {
                    // The input stream closed at the pause cutoff: the blind rows of
                    // the pause up to Stop are admitted there.
                    publish_blind_pause(&mut input, &mut input_prefix, closed.time, stopped.time, closed.time, &reasons)?;
                    Some(paused_slot_ranges(&ranges, closed.time, stopped.time))
                } else { None };
                let mut stop = serde_json::json!({
                    "kind": "recorder_control", "command": "stop_boundary",
                    "cutoff_t_us": cutoff.time, "stop_t_us": stopped.time, "paused_slots": paused_slots,
                });
                if let Some(before) = receiver.elevated_hook_loss() {
                    stop["required_hook_first_uncovered_qpc"] = serde_json::json!(before);
                }
                send_boundary_event(&mut context, stop, timing, &mut context_prefix, &mut next_context_index)?;
            }
            publish_controls(final_controls, receipt, stopped.qpc, &mut context, &mut context_prefix,
                &mut next_context_index, |_, _| None)?;
            input.seal(cutoff.time)?;
            context.seal(stopped.time)?;
        } else if input_prefix.last_time.is_some() || context_prefix.last_receipt.is_some() {
            return Err(unavailable("The admitted input prefix has no closing boundary"));
        }
        let (devices, device_inventory_truncated) = receiver.finish_inventory()?;
        let pen = pen_capture(receiver.pen_observation(), registration, remote, devices.as_slice(), device_inventory_truncated);
        let (keyboard_hook_loss_suspected, pointer_hook_loss_suspected) = receiver.hook_loss();
        Ok(InputPrefix {
            cutoff, input: input_prefix, context: context_prefix, video_health,
            capture: Some(InputCapture {
                registration, pen: Some(pen), publication: InputPublication::ApprovedReceiptGate, settings: Some(input_settings),
                transport_overflow: receiver.overflowed(), devices: Some(devices), device_inventory_truncated,
                keyboard_hook_loss_suspected, pointer_hook_loss_suspected, uncertified_closing_tail, blind: None,
                optional_unavailable: receiver.optional_unavailable(),
            }),
            cursors: native_context.cursor_inventory(),
            health_seen: capture_health_seen,
        })
    }
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

/// Write the runtime health found since the last poll: each new video
/// failure, and each new capture health record.
fn publish_runtime_health(
    session: &WorkerSession, context: &mut JournalIngress, receipt: Receipt, observed_at: QpcTick,
    video_health: &mut Vec<MonitorId>, video_health_seen: &mut usize, capture_health_seen: &mut usize,
) -> Result<(), ProbeError> {
    for failed in session.video_failures_after(*video_health_seen) {
        let Some(timing) = input_timing(receipt, observed_at, None)? else { break };
        let detected = failed.at.unwrap_or(timing.time.recv_t_us);
        context.submit_health(video_failure_health(failed.failure, failed.monitor, detected))
            ?;
        *video_health_seen += 1;
        video_health.push(failed.monitor);
    }
    for record in session.health_after(*capture_health_seen) {
        context.submit_health(record)?;
        *capture_health_seen += 1;
    }
    Ok(())
}

/// Run `step` unless an earlier step of the same batch failed; the batch
/// keeps its first error.
fn keep_first_error(first: &mut Result<(), ProbeError>, step: impl FnOnce() -> Result<(), ProbeError>) {
    if first.is_ok() {
        *first = step();
    }
}

fn poll_elevated(
    source: &mut Selector<'_>, applied: &mut Option<VisitBinding>, receiver: &mut receiver::Receiver, native: &mut Context<'_>,
    input: &mut JournalIngress, input_prefix: &mut ReceiptPrefix,
    context: &mut JournalIngress, context_prefix: &mut ReceiptPrefix, next_context_index: &mut u64,
    frames: &FramePolicy, session: &WorkerSession, receipt: Receipt, observed_at: QpcTick,
) -> Result<(), ProbeError> {
    let now = observed_at;
    let admission = session.active_source(receipt.boundary.epoch);
    if let Some(opened) = source.open_visit() {
        match native.elevated_invalidation(opened.binding, opened.focus, opened.completed, receipt) {
            Ok(Some((before, true))) => {
                let closed = source.close(before.max(receipt.boundary.qpc));
                frames.clear();
                session.require_source_change(receipt.boundary.epoch);
                if let Err(failure) = closed {
                    session.fail_source(source.stage(), failure);
                    source.cancel();
                }
            }
            Ok(Some((before, false))) => source.observe_focus_loss(before),
            Err(failure) => {
                frames.clear();
                session.fail_source(source.stage(), failure);
                source.cancel();
            }
            Ok(None) => {}
        }
    } else {
        native.discard_unbound_notifications();
    }
    let loss = source.take_loss();
    let staging_loss = source.take_staging_loss();
    let context_pressure = source.take_context_pressure();
    let overflowed = staging_loss.is_some() || context_pressure.is_some();
    if overflowed { receiver.note_native_gap(); }
    if let Some(loss) = &staging_loss {
        note_staging_loss(receiver, loss, receipt)?;
    }
    for gap in source.take_prior_gaps() {
        receiver.note_elevated_loss(gap.cause.into(), Some(gap), gap.first_qpc, receipt)?;
    }
    // Elevated context evidence the Recorder's staging could not hold is a
    // typed context loss (E3, D-1).
    if let Some(gap) = context_pressure {
        native.note_loss(LossReason::StagingPressure, LossSpan { count: gap.count(), first: gap.first_qpc, last: gap.last_qpc });
    }
    if let Some(span) = source.take_optional_loss() {
        native.note_loss(LossReason::StagingPressure, span);
    }
    if let Some(loss) = staging_loss && let Some(timing) = input_timing(receipt, observed_at, None)? {
        publish_input_staging_loss(context, loss, timing, context_prefix, next_context_index)?;
    }
    if let Some((binding, before, cause, gap)) = loss {
        if gap.is_some() {
            receiver.note_native_gap();
        }
        let reason = match (cause, gap) {
            (_, Some(gap)) => gap.cause.into(),
            (CoverageReason::NativeGap, None) => LossReason::NativeQueueOverflow,
            (CoverageReason::UncertifiedSource, None) => LossReason::UncertifiedSource,
            (CoverageReason::Opening | CoverageReason::HeldUnknown, None) => LossReason::ResyncPending,
        };
        receiver.note_elevated_loss(reason, gap, before, receipt)?;
        if let Some(timing) = input_timing(receipt, observed_at, None)? {
            let row = source_loss_row(binding, before, cause, gap).map_err(|_| unavailable("Encode an input source loss row"))?;
            send_boundary_event(context, row, timing, context_prefix, next_context_index)?;
        }
    }
    if let Some(interrupted) = source.take_interruption() {
        let reason = if session.source_failure().is_some() {
            SourceBlindReason::SourceFailure
        } else if overflowed {
            SourceBlindReason::RecorderStaging
        } else if let Some((_, _, cause, _)) = loss {
            SourceBlindReason::ElevatedOpening(cause)
        } else {
            SourceBlindReason::FocusSuspended
        };
        source.note_blind(receipt.boundary.epoch, interrupted, reason);
        session.source_resynchronizing(receipt.boundary.epoch);
        frames.clear();
        if let Some(previous) = applied.take() {
            let certified = if let Some(certificate) = source.input_certificate(Some(interrupted))
                .filter(|certificate| certificate.visit == previous)
                && accept_elevated_certificate(source, certificate, receiver, native, receipt, None)?
            {
                receiver.advance(receipt.clock.time(certificate.before)?);
                let before = close_failed_hooks(source, receiver, native, session, receipt)?
                    .map_or(certificate.before, |hook| hook.min(certificate.before));
                let through = receipt.clock.time(before)?;
                record_elevated_coverage(session, receipt, receiver, certificate, through)?;
                before
            } else {
                receipt.boundary.qpc
            };
            let cutoff = receipt.clock.time(certified.max(receipt.boundary.qpc))?;
            let through = input_prefix.raised(cutoff);
            let boundary = if loss.is_some() || overflowed { HeldBoundary::InputGap } else { HeldBoundary::FocusChanged };
            receiver.interrupt_certified(cutoff, through, boundary);
            receiver.publish(native, input, input_prefix, receipt, through, true)?;
            receiver.finish_still_prefix(through);
            native.pause(interrupted)?;
            if let Some(timing) = input_timing(receipt, observed_at, None)? {
                send_boundary_events(context, native.close_hung_observation(), timing, context_prefix, next_context_index)?;
            }
            receiver.reopen(through, false);
            // E1: records past the certified prefix are counted, never dropped silently.
            let mut counted = Ok(());
            source.take_uncertified(previous, |record| {
                keep_first_error(&mut counted, || receiver.note_uncertified(record, receipt));
            });
            counted?;
        }
    }
    let live = source.current_visit();
    if !live && let Some(visit) = source.open_visit() {
        source.observe_focus_loss(source.certified_before().unwrap_or(visit.completed));
    }
    let Some(opened) = source.open_visit().filter(|visit| live && visit.binding.epoch == receipt.boundary.epoch
        && session.elevated_active(receipt.boundary.epoch)) else {
        frames.clear();
        native.discard_elevated_clipboard();
        return Ok(());
    };
    if *applied != Some(opened.binding) {
        source.note_opening(receipt.boundary, opened);
        receiver.select_source(Some(opened.binding));
        let visit = native.activate_elevated(opened.binding, opened.focus, opened.completed)?;
        receiver.elevated_synchronization(receipt, opened.completed, opened.held, visit)?;
        *applied = Some(opened.binding);
        if let Some(timing) = input_timing(receipt, observed_at, None)? {
            send_boundary_event(context, serde_json::json!({
                "kind": "input_source_coverage", "source": "elevated", "coverage": opened.coverage,
                "admission_epoch": opened.binding.epoch, "registration": opened.registration,
            }), timing, context_prefix, next_context_index)?;
        }
    }
    if let Some((binding, completed, snapshot)) = source.take_recovered()
        && binding == opened.binding && let Some(visit) = native.visit()
    {
        receiver.recovered_synchronization(receipt, completed, snapshot, visit)?;
    }
    let rows = match native.poll_elevated_queries(source, now, receipt, &admission) {
        Ok(rows) if source.current_visit() && session.elevated_active(receipt.boundary.epoch) => rows,
        Ok(_) => {
            frames.clear();
            native.discard_elevated_clipboard();
            if !source.current_visit() {
                source.observe_focus_loss(source.certified_before().unwrap_or(opened.completed));
            }
            return Ok(());
        }
        Err(failure) => {
            frames.clear();
            session.fail_source(source.stage(), failure);
            source.cancel();
            return Ok(());
        }
    };
    let observed = receipt.clock.time(now)?;
    let since = receipt.clock.time(opened.coverage.before_qpc)?;
    let certificate = source.input_certificate(None);
    let certified = certificate.map(|certificate| receipt.clock.time(certificate.before)).transpose()?;
    if observed.0.saturating_sub(certified.unwrap_or(since).0) >= MAX_ELEVATED_CERTIFICATION_LAG_US {
        frames.clear();
        session.fail_source(source.stage(), screen_fix_elevated::Failure::Witness);
        source.cancel();
        return Ok(());
    }
    let (Some(certificate), Some(through)) = (certificate, certified) else {
        if source.current_visit() && session.elevated_active(receipt.boundary.epoch)
        {
            if source.media_open() {
                frames.publish_elevated(opened.binding, since, observed, since, receipt.boundary, receipt.clock);
            }
            if let Some(timing) = input_timing(receipt, now, None)? {
                for row in rows {
                    send_event(context, row, timing, context_prefix, next_context_index)?;
                }
            }
        } else {
            frames.clear();
        }
        return Ok(());
    };
    if !source.current_visit() {
        frames.clear();
        native.discard_elevated_clipboard();
        source.observe_focus_loss(certificate.before);
        return Ok(());
    }
    if !session.elevated_active(receipt.boundary.epoch) {
        frames.clear();
        native.discard_elevated_clipboard();
        return Ok(());
    }
    if !accept_elevated_certificate(source, certificate, receiver, native, receipt, None)? {
        return Ok(());
    }
    receiver.advance(through);
    if close_failed_hooks(source, receiver, native, session, receipt)?.is_some() {
        return Ok(());
    }
    if !source.current_visit() || !session.elevated_active(receipt.boundary.epoch) {
        frames.clear();
        native.discard_elevated_clipboard();
        return Ok(());
    }
    // Materialization follows the actual Recorder receipts in this poll.
    if let Some(timing) = input_timing(receipt, now, None)? {
        for row in rows {
            send_event(context, row, timing, context_prefix, next_context_index)?;
        }
    }
    // A delivered certificate can still contain records awaiting correlation.
    let frame_before = receiver.frame_before().map_or(since, |before| before.min(through));
    if source.media_open() {
        frames.publish_elevated(opened.binding, since, observed, frame_before, receipt.boundary, receipt.clock);
        receiver.open_elevated_stills(opened.binding, frame_before);
    }
    let (texts, losses) = native.take_confirmed_text(receipt, now);
    for text in texts {
        receiver.stage_confirmed(text, receipt);
    }
    for loss in losses {
        receiver.note_loss(loss);
    }
    receiver.publish(native, input, input_prefix, Receipt { qpc: now, ..receipt }, through, false)?;
    if source.media_open() && record_elevated_coverage(session, receipt, receiver, certificate, through)? {
        source.finish_blind(receipt.boundary, receipt.clock, certificate.opening);
        source.certified_capture(receipt.boundary.epoch);
        session.source_enhanced(receipt.boundary.epoch);
    }
    // Pointer rest is decided only through a whole microsecond and one tick
    // before the certificate, so its session time is certified as well.
    let ticks_per_us = receipt.clock.qpc_frequency.ticks(Duration::from_micros(1)).unwrap_or(i64::MAX);
    let last_certified = QpcTick(certificate.before.0.saturating_sub(ticks_per_us.saturating_add(1)));
    receiver.poll_pointer_rest(native, receipt, last_certified)?;
    Ok(())
}

fn describe_tail(
    receiver: &receiver::Receiver, source: &mut Selector<'_>, session: &WorkerSession,
    receipt: Receipt, cutoff: AdmissionBoundary, certified: Option<SessionTimeUs>,
) -> Option<UncertifiedInputTail> {
    if certified.is_some() {
        if receiver.synchronization_qpc().is_none() {
            source.note_blind(receipt.boundary.epoch, receipt.boundary.qpc,
                SourceBlindReason::ElevatedOpening(crate::protocol::ipc::elevated::runtime::CoverageReason::Opening));
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

fn record_elevated_coverage(
    session: &WorkerSession, receipt: Receipt, receiver: &receiver::Receiver,
    certificate: InputCertificate, through: SessionTimeUs,
) -> Result<bool, ProbeError> {
    let opening = receipt.clock.time(certificate.opening.max(receipt.boundary.qpc))?;
    let Some(start) = opening.0.checked_add(1).map(SessionTimeUs) else { return Ok(false); };
    let Some(before) = receiver.unresolved_before().map(|before| before.min(through)).filter(|before| *before > start)
        else { return Ok(false); };
    session.record_source_coverage(receipt.boundary.epoch, Some(certificate.visit), start, before);
    Ok(true)
}

fn drain_elevated_prefix(
    source: &mut Selector<'_>, receiver: &mut receiver::Receiver, native: &mut Context<'_>,
    receipt: Receipt, closing: AdmissionBoundary, session: &WorkerSession,
) -> Result<SessionTimeUs, ProbeError> {
    let Some(certificate) = source.input_certificate(Some(closing.qpc))
        .filter(|_| !source.awaiting_close()) else { return Ok(receipt.boundary.time); };
    let time = receipt.clock.time(certificate.before)?;
    let closing = AdmissionBoundary { qpc: certificate.before, time, ..closing };
    if !accept_elevated_certificate(source, certificate, receiver, native, receipt, Some(closing))? {
        return Ok(receipt.boundary.time);
    }
    receiver.advance(time);
    let certified = match close_failed_hooks(source, receiver, native, session, receipt)? {
        Some(before) => receipt.clock.time(before.min(certificate.before))?,
        None => time,
    };
    if record_elevated_coverage(session, receipt, receiver, certificate, certified)? {
        source.finish_blind(receipt.boundary, receipt.clock, certificate.opening);
    }
    Ok(certified)
}

fn close_failed_hooks(
    source: &mut Selector<'_>, receiver: &mut receiver::Receiver, native: &mut Context<'_>,
    session: &WorkerSession, receipt: Receipt,
) -> Result<Option<QpcTick>, ProbeError> {
    let Some((visit, before)) = receiver.elevated_hook_cutoff() else { return Ok(None); };
    source.cancel_before(visit, before);
    native.discard_elevated_clipboard();
    native.truncate_elevated_input(before);
    if let Some(certificate) = source.input_certificate(Some(before)) {
        receiver.adopt_elevated_certificate(certificate, receipt, native);
    }
    if receiver.take_hook_reports().into_iter().any(|report| report.is_some()) {
        session.required_elevated_hook_failed(session.stop_requested());
    }
    Ok(Some(before))
}

fn accept_elevated_certificate(
    source: &mut Selector<'_>, certificate: InputCertificate, receiver: &mut receiver::Receiver,
    native: &mut Context<'_>, receipt: Receipt, closing: Option<AdmissionBoundary>,
) -> Result<bool, ProbeError> {
    if !receiver.adopt_elevated_certificate(certificate, receipt, native) { return Ok(false); }
    let mut accepted = Ok(());
    source.take_input(certificate, |record| {
        keep_first_error(&mut accepted, || receiver.accept_elevated(record, receipt, closing, native));
    });
    accepted?;
    receiver.finish_elevated_delivery(receipt)?;
    Ok(true)
}

/// What every foreground source service step needs besides the selector.
#[derive(Clone, Copy)]
struct SourceService<'a> {
    session: &'a WorkerSession,
    topology: &'a super::display::topology::Topology,
    frame_policy: &'a FramePolicy,
}

impl SourceService<'_> {
    /// Advance source selection; a failure fails and cancels the source. A
    /// running standard helper reports when it last attested its registrations.
    fn tick(
        self, source: &mut Selector<'_>, active: Option<AdmissionBoundary>, helper: Option<&InputHelper>,
        receiver: &receiver::Receiver,
    ) {
        let ordinary = helper.map(|_| receiver.helper_checked());
        if let Err(failure) = source.tick(self.topology, active, self.session.take_elevated_environment(), ordinary) {
            self.frame_policy.clear();
            self.session.fail_source(source.stage(), failure);
            source.cancel();
        }
    }
}

fn poll_paused_helper(
    helper: &mut Option<InputHelper>, retired: &mut Option<InputHelper>, receiver: &mut receiver::Receiver, native: &mut Context<'_>,
    session: &WorkerSession, reports: Option<&mpsc::SyncSender<RecordingReport>>, receipt: Receipt,
    cutoff: AdmissionBoundary, admitted: bool, ordinary: bool,
) -> Result<(), ProbeError> {
    if let Some(child) = helper {
        let mut accepted = Ok(());
        let result = child.notify(session.take_input_environment()).and_then(|()| child.poll(|record| {
            if admitted {
                keep_first_error(&mut accepted, || receiver.accept(record, receipt, Some(cutoff), native));
            } else {
                receiver.collect_ordinary_inventory(record, ordinary);
            }
        }));
        accepted?;
        if let Err(error) = result {
            let at = session.session_time().unwrap_or(cutoff.time);
            if ordinary {
                // Once the pause prefix is published, the input stream holds no
                // runtime health row; the activity loss keeps its final row.
                note_helper_failure(session, reports, receiver, error, at, admitted);
            } else {
                note_inactive_helper_failure(session, error, at);
            }
            retire_helper(helper, retired);
        }
    }
    Ok(())
}

fn retire_helper(helper: &mut Option<InputHelper>, retired: &mut Option<InputHelper>) {
    if let Some(mut helper) = helper.take() {
        helper.retire();
        *retired = Some(helper);
    }
}

/// Queue OS flushes without introducing disk waits on the collection path.
#[derive(Default)]
struct DeferredFlush {
    requested: bool,
    waiting: Option<crate::package::journal::FlushOutcome>,
}

impl DeferredFlush {
    fn poll(&mut self, journal: &mut JournalIngress, at: SessionTimeUs, requested: bool) -> Result<(), ProbeError> {
        self.requested |= requested;
        if let Some(waiting) = &self.waiting {
            match waiting.try_recv() {
                Ok(outcome) => { outcome?; self.waiting = None; }
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => return Err(unavailable("A requested journal flush did not complete")),
            }
        }
        if self.requested && let FlushRequest::Queued(waiting) = journal.try_flush(Flush::Os, at)
            ?
        {
            self.waiting = Some(waiting);
            self.requested = false;
        }
        Ok(())
    }
}

fn flush_boundary(
    journal: &mut JournalIngress, at: SessionTimeUs, session: &WorkerSession,
    mut drain: impl FnMut() -> Result<(), ProbeError>,
) -> Result<(), ProbeError> {
    let deadline = Instant::now() + BOUNDARY_FLUSH_TIMEOUT;
    let mut waiting: Option<crate::package::journal::FlushOutcome> = None;
    loop {
        if let Some(result) = &waiting {
            match result.try_recv() {
                Ok(summary) => return summary.map(|_| ()).map_err(ProbeError::from),
                Err(mpsc::TryRecvError::Disconnected) => return Err(unavailable("The journal pause flush did not complete")),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        } else if let FlushRequest::Queued(result) = journal.try_flush(Flush::Os, at)? {
            waiting = Some(result);
        }
        if Instant::now() >= session.stop_deadline().map_or(deadline, |stop| stop.min(deadline)) {
            return Err(unavailable("The lifecycle journal flush exceeded its deadline"));
        }
        drain()?;
        thread::park_timeout(Duration::from_millis(2));
    }
}

fn paused_slot_ranges(ranges: &[PausedVideo], closed: SessionTimeUs, through: SessionTimeUs) -> Vec<PausedSlots> {
    ranges.iter().map(|range| PausedSlots {
        monitor: range.monitor, fps: range.fps, first_paused_slot: range.first_paused_slot,
        end_slot_exclusive: range.end_exclusive(closed, through),
    }).collect()
}

/// The video slots of one monitor that a pause interval covered.
#[derive(serde::Serialize)]
struct PausedSlots {
    monitor: MonitorId,
    fps: u32,
    first_paused_slot: u64,
    end_slot_exclusive: Option<u64>,
}

/// The input blind rows of a pause interval (INP-13), admitted to the input
/// stream at `at`: the resume boundary, or the input cutoff of a Stop while paused.
fn publish_blind_pause(
    input: &mut JournalIngress, prefix: &mut ReceiptPrefix, start: SessionTimeUs, end: SessionTimeUs,
    at: SessionTimeUs, reasons: &[PauseReason],
) -> Result<(), ProbeError> {
    let rows = pause_blind_labels(reasons).into_iter().map(|reason| BlindRow::new(reason, start, end, &[]).value())
        .collect::<serde_json::Result<_>>().map_err(|_| unavailable("Encode an input blind row"))?;
    prefix.submit_boundary(input, at, rows)
}

/// The reason of an input blind row of a pause interval (INP-13).
fn send_boundary_event(
    journal: &mut JournalIngress, event: serde_json::Value, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next_context_index: &mut u64,
) -> Result<(), ProbeError> {
    send_event_with_priority(journal, event, timing, prefix, next_context_index, true)
}

fn publish_input_staging_loss(
    journal: &mut JournalIngress, loss: InputStagingLoss, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next_context_index: &mut u64,
) -> Result<(), ProbeError> {
    let row = staging_loss_row(&loss).map_err(|_| unavailable("Encode an input source loss row"))?;
    send_boundary_event(journal, row, timing, prefix, next_context_index)
}

/// The pause reasons of a `pause` or `resume` row: `reason` is the primary
/// (first ordered) reason (CTX-6); `reasons` keeps the full ordered set as an extension.
#[derive(serde::Serialize)]
struct PauseReasons<'a> {
    reason: Option<&'a PauseReason>,
    reasons: &'a [PauseReason],
}

impl<'a> PauseReasons<'a> {
    fn new(reasons: &'a [PauseReason]) -> Self {
        Self { reason: reasons.first(), reasons }
    }
}

/// The context `pause` row at the input cutoff.
#[derive(serde::Serialize)]
struct PauseRow<'a> {
    kind: RowKind,
    #[serde(flatten)]
    reasons: PauseReasons<'a>,
    cutoff_t_us: SessionTimeUs,
    uncertified_input_tail: Option<UncertifiedInputTail>,
    paused_slots: &'a [PausedVideo],
    paused_slots_all_monitors: bool,
    /// The first QPC a required hook did not cover, when one stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    required_hook_first_uncovered_qpc: Option<QpcTick>,
}

fn pause_row(
    reasons: &[PauseReason], cutoff: SessionTimeUs, tail: Option<UncertifiedInputTail>, ranges: &[PausedVideo],
    all_monitors: bool, hook_loss: Option<QpcTick>,
) -> serde_json::Result<serde_json::Value> {
    serde_json::to_value(PauseRow {
        kind: RowKind::Pause, reasons: PauseReasons::new(reasons), cutoff_t_us: cutoff, uncertified_input_tail: tail,
        paused_slots: ranges, paused_slots_all_monitors: all_monitors, required_hook_first_uncovered_qpc: hook_loss,
    })
}

/// The context `discontinuity` row from the pause cutoff to the reopened boundary.
#[derive(serde::Serialize)]
struct DiscontinuityRow<'a> {
    kind: RowKind,
    start_t_us: SessionTimeUs,
    end_t_us: SessionTimeUs,
    gap_us: i64,
    previous_admission_epoch: AdmissionEpoch,
    reasons: &'a [PauseReason],
}

/// The `discontinuity` row from the pause cutoff `closed` to the reopened boundary at `opened`.
fn discontinuity_row(closed: AdmissionBoundary, opened: SessionTimeUs, reasons: &[PauseReason])
    -> serde_json::Result<serde_json::Value>
{
    serde_json::to_value(DiscontinuityRow {
        kind: RowKind::Discontinuity, start_t_us: closed.time, end_t_us: opened,
        gap_us: opened.0.saturating_sub(closed.time.0), previous_admission_epoch: closed.epoch, reasons,
    })
}

/// The context `resume` row with the totals suppressed during the pause.
#[derive(serde::Serialize)]
struct ResumeRow<'a> {
    kind: RowKind,
    #[serde(flatten)]
    reasons: PauseReasons<'a>,
    paused_start_t_us: SessionTimeUs,
    paused_slots: &'a [PausedSlots],
    suppressed_context_notifications: u64,
    suppressed_health: &'a BTreeMap<HealthKind, u64>,
}

fn resume_row(
    reasons: &[PauseReason], paused_start: SessionTimeUs, paused_slots: &[PausedSlots],
    suppressed_notifications: u64, suppressed_health: &BTreeMap<HealthKind, u64>,
) -> serde_json::Result<serde_json::Value> {
    serde_json::to_value(ResumeRow {
        kind: RowKind::Resume, reasons: PauseReasons::new(reasons), paused_start_t_us: paused_start, paused_slots,
        suppressed_context_notifications: suppressed_notifications, suppressed_health,
    })
}

/// The input source an `input_source_loss` row reports.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum LossSource {
    Elevated,
}

/// A Recorder staging overflow of elevated input, as an `input_source_loss` cause (E3).
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum StagingCause {
    RecorderStaging,
}

/// What an `input_source_loss` row reports beside its source and visit.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum SourceLoss<'a> {
    /// The collector's coverage loss, with the native producer gap that caused it.
    Coverage { cause: CoverageReason, native_gap: Option<ProducerGap> },
    /// The overflowed producer ranges and the undecodable records.
    Staging { cause: StagingCause, native_gaps: &'a [Option<ProducerGap>], malformed: u64 },
}

/// The context `input_source_loss` row of an elevated visit.
#[derive(serde::Serialize)]
struct InputSourceLossRow<'a> {
    kind: RowKind,
    source: LossSource,
    visit: VisitBinding,
    first_uncovered_qpc: QpcTick,
    #[serde(flatten)]
    loss: SourceLoss<'a>,
}

impl<'a> InputSourceLossRow<'a> {
    fn value(visit: VisitBinding, first_uncovered_qpc: QpcTick, loss: SourceLoss<'a>) -> serde_json::Result<serde_json::Value> {
        serde_json::to_value(Self { kind: RowKind::InputSourceLoss, source: LossSource::Elevated, visit, first_uncovered_qpc, loss })
    }
}

/// The context row of an elevated visit's input coverage loss.
fn source_loss_row(visit: VisitBinding, before: QpcTick, cause: CoverageReason, gap: Option<ProducerGap>)
    -> serde_json::Result<serde_json::Value>
{
    InputSourceLossRow::value(visit, before, SourceLoss::Coverage { cause, native_gap: gap })
}

/// The context row of a Recorder staging overflow of elevated input (E3).
fn staging_loss_row(loss: &InputStagingLoss) -> serde_json::Result<serde_json::Value> {
    InputSourceLossRow::value(loss.visit, loss.before, SourceLoss::Staging {
        cause: StagingCause::RecorderStaging, native_gaps: &loss.gaps, malformed: loss.malformed,
    })
}

/// Why a checkpoint Save has no selected context.
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointUnavailable {
    /// The checkpoint queue was full when it was requested.
    QueueFull,
    TimedOut,
    AdmissionClosed,
}

/// Whether a checkpoint Save's context was observed.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ContextStatus {
    Observed,
    Unavailable,
}

/// The context row of a checkpoint Save's selected context, or of why it has none.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename = "checkpoint_context")]
struct CheckpointContextRow<S> {
    request_id: RequestId,
    control_receipt_t_us: Option<SessionTimeUs>,
    save_context: Option<S>,
    status: ContextStatus,
    reason: Option<CheckpointUnavailable>,
}

fn checkpoint_context_row<S: serde::Serialize>(
    id: RequestId, control_receipt: Option<SessionTimeUs>, selected: Result<S, CheckpointUnavailable>,
) -> serde_json::Result<serde_json::Value> {
    let status = if selected.is_ok() { ContextStatus::Observed } else { ContextStatus::Unavailable };
    let (save_context, reason) = match selected {
        Ok(selected) => (Some(selected), None),
        Err(reason) => (None, Some(reason)),
    };
    serde_json::to_value(CheckpointContextRow { request_id: id, control_receipt_t_us: control_receipt, save_context, status, reason })
}

/// The input-stream markers of a Recorder staging overflow (E3): one per
/// overflowed producer range, and the undecodable records.
fn note_staging_loss(receiver: &mut receiver::Receiver, loss: &InputStagingLoss, receipt: Receipt) -> Result<(), ProbeError> {
    for gap in loss.gaps.iter().flatten() {
        receiver.note_elevated_loss(LossReason::StagingPressure, Some(*gap), loss.before, receipt)?;
    }
    if let Some(count) = std::num::NonZeroU64::new(loss.malformed) && loss.before >= receipt.boundary.qpc {
        let at = receipt.clock.time(loss.before)?;
        receiver.note_loss(LossEvidence::ordinary(LossReason::MalformedPacket, None, count, at, at));
    } else if loss.gaps.iter().all(Option::is_none) {
        receiver.note_elevated_loss(LossReason::StagingPressure, None, loss.before, receipt)?;
    }
    Ok(())
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

#[derive(Default)]
struct ElevatedSaves {
    fixtures: VecDeque<FixtureNotice>,
    checkpoints: VecDeque<(RequestId, QpcTick)>,
    /// Checkpoints refused by the full queue; each still gets its row (E4).
    refused: Vec<(RequestId, QpcTick)>,
}

impl ElevatedSaves {
    fn fixture(&mut self, notice: FixtureNotice) {
        if self.fixtures.len() < MAX_FIXTURE_SAVES {
            self.fixtures.push_back(notice);
        } else {
            let _ = notice.reply.try_send(Err(FixtureSaveError::Busy));
        }
    }

    fn checkpoint(&mut self, id: RequestId, at: QpcTick) {
        if self.checkpoints.len() < MAX_FIXTURE_SAVES {
            self.checkpoints.push_back((id, at));
        } else {
            self.refused.push((id, at));
        }
    }

    /// Resolve deferred Saves against the certified prefix. When `closing`
    /// (Pause or Stop, E4), this is the final poll: every remaining item gets
    /// its terminal outcome, `admission_closed`.
    fn poll(
        &mut self, certified: Option<(VisitBinding, QpcTick)>, receipt: Receipt, observed: QpcTick, session: &WorkerSession,
        layout: &AttemptLayout, native: &mut Context<'_>, endpoints: &BTreeMap<MonitorId, stills::Requests>,
        still_rows: &still_rows::Rows, context: &mut JournalIngress, prefix: &mut ReceiptPrefix, next: &mut u64,
        closing: bool,
    ) -> Result<(), ProbeError> {
        let now = receipt.clock.time(observed)?;
        let expired = |at| receipt.clock.time(at).is_ok_and(|at| now.0.saturating_sub(at.0) >= DEFERRED_SAVE_TIMEOUT_US);
        for _ in 0..self.fixtures.len() {
            let Some(notice) = self.fixtures.pop_front() else { break; };
            let received = notice.receipt.qpc;
            let result = if receipt.boundary != notice.receipt.boundary || session.stop_requested() {
                Err(FixtureSaveError::AdmissionClosed)
            } else if expired(received) {
                Err(FixtureSaveError::UnapprovedContext)
            } else if let Some((binding, _)) = certified.filter(|(_, before)| received < *before) {
                route_fixture(&notice, session, layout, native, endpoints, still_rows, receipt, Some(binding))
            } else if closing {
                Err(FixtureSaveError::AdmissionClosed)
            } else {
                self.fixtures.push_back(notice);
                continue;
            };
            let _ = notice.reply.try_send(result);
        }
        let Some(timing) = input_timing(receipt, observed, None)? else { return Ok(()) };
        let mut row = |id: RequestId, at: QpcTick, selected: Result<SaveWindowContext, CheckpointUnavailable>| {
            let row = checkpoint_context_row(id, receipt.clock.time(at).ok(), selected)
                .map_err(|_| unavailable("Encode a checkpoint context row"))?;
            send_event(context, row, timing, prefix, next)
        };
        for (id, at) in std::mem::take(&mut self.refused) {
            row(id, at, Err(CheckpointUnavailable::QueueFull))?;
        }
        for _ in 0..self.checkpoints.len() {
            let Some((id, at)) = self.checkpoints.pop_front() else { break; };
            let available = certified.filter(|(_, before)| at < *before && !expired(at));
            if available.is_none() && !expired(at) && !closing {
                self.checkpoints.push_back((id, at));
                continue;
            }
            let selected = available.and_then(|(binding, _)| native.checkpoint_context(at, Some(binding)))
                .ok_or_else(|| if expired(at) { CheckpointUnavailable::TimedOut } else { CheckpointUnavailable::AdmissionClosed });
            row(id, at, selected)?;
        }
        Ok(())
    }

    /// Without a closing receipt no row can be written; this happens only
    /// when no admitted epoch closed, so no checkpoint can be pending.
    fn close(&mut self) {
        for notice in self.fixtures.drain(..) {
            let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
        }
        self.checkpoints.clear();
        self.refused.clear();
    }
}

fn route_fixture(
    notice: &FixtureNotice, session: &WorkerSession, layout: &AttemptLayout, native: &mut Context<'_>,
    endpoints: &BTreeMap<MonitorId, stills::Requests>, rows: &still_rows::Rows, open: Receipt, source: Option<VisitBinding>,
) -> FixtureSaveResult {
    let received = notice.receipt;
    if received.boundary != open.boundary {
        return Err(FixtureSaveError::AdmissionClosed);
    }
    let signal = &notice.signal;
    let (proof, visit) = native.fixture_save_proof(&signal.executable, received.qpc)
        .ok_or(FixtureSaveError::UnapprovedContext)?;
    if visit != source { return Err(FixtureSaveError::UnapprovedContext); }
    let routing = if let Some(monitors) = &signal.affected_monitors {
        SaveRouting::Monitors { monitors: monitors.clone(), basis: MonitorRoutingBasis::ApplicationGeometry }
    } else {
        native.checkpoint_context(received.qpc, source).ok_or(FixtureSaveError::InvalidRouting)?.fallback_routing()
    };
    let SaveRouting::Monitors { monitors, .. } = &routing else { return Err(FixtureSaveError::InvalidRouting) };
    let targets = monitors.as_slice().to_vec();
    let time = received.clock.time(received.qpc).map_err(|_| FixtureSaveError::ClockUnavailable)?;
    let evidence = SaveEvidence {
        id: notice.id, time: RecordTime { recv_t_us: time, source_t_us: None, source_uncertainty_us: None, coarse: None },
        kind: signal.kind, source: ConfirmationSource::Fixture, executable: signal.executable.clone(), request: None,
    };
    let confirmation = ConfirmedSave::new(layout.id().clone(), session.config(), evidence, routing)
        .map_err(|_| FixtureSaveError::InvalidRouting)?;
    let mut admitted = 0;
    for &monitor in &targets {
        let Some(geometry) = native.save_geometry(monitor, received.qpc) else { continue; };
        let Some(endpoint) = endpoints.get(&monitor) else { continue; };
        let Some(confirmation) = confirmation.for_monitor(monitor) else { continue; };
        let (trigger, role) = stills::save_slot(confirmation.kind());
        let fallback = stills::fallback_row(trigger, role, time, crate::protocol::Evidence::Observed(geometry),
            open.boundary.epoch, None, stills::DropReason::SaveHandoffUnavailable).with_save(&confirmation);
        let reservation = match rows.reserve(time, Some(fallback)) {
            Ok(reservation) => reservation,
            Err(reason) => {
                rows.note_loss(time, reason);
                continue;
            }
        };
        if endpoint.confirmed_save(stills::Save { reservation, confirmation, geometry, admission: open.boundary.epoch, proof, visit }) {
            admitted += 1;
        }
    }
    if admitted == targets.len() {
        Ok(notice.id)
    } else if admitted == 0 {
        Err(FixtureSaveError::MonitorUnavailable)
    } else {
        Err(FixtureSaveError::PartiallyRouted { id: notice.id, admitted })
    }
}

fn publish_controls(
    controls: ControlPrefix,
    receipt: Receipt,
    observed: QpcTick,
    context: &mut JournalIngress,
    prefix: &mut ReceiptPrefix,
    next_context_index: &mut u64,
    mut checkpoint: impl FnMut(RequestId, QpcTick) -> Option<crate::capture::save::SaveWindowContext>,
) -> Result<(), ProbeError> {
    let Some(timing) = input_timing(receipt, observed, None)? else { return Ok(()) };
    if let Some(loss) = controls.loss {
        let event = serde_json::json!({
            "kind": "recorder_control_gap",
            "index": *next_context_index,
            "admitted_100ns": timing.admitted_100ns,
            "dropped": loss.count,
            "first_control_receipt_t_us": receipt.clock.time(loss.first)?,
            "last_control_receipt_t_us": receipt.clock.time(loss.last)?,
        });
        send_boundary_event(context, event, timing, prefix, next_context_index)?;
    }
    for notice in controls.into_notices() {
        let activation = notice.activation;
        let request = matches!(notice.outcome, ControlOutcome::CheckpointRequested)
            .then(|| activation.checkpoint().id);
        let save_context = request.and_then(|id| checkpoint(id, activation.at));
        let event = serde_json::json!({
            "kind": "recorder_control",
            "index": *next_context_index,
            "admitted_100ns": timing.admitted_100ns,
            "command": activation.action,
            "outcome": notice.outcome,
            "request_id": request,
            "save_context": save_context,
            "control_receipt_t_us": receipt.clock.time(activation.at)?,
        });
        send_boundary_event(context, event, timing, prefix, next_context_index)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct InputTiming {
    time: RecordTime,
    received: QpcTick,
    admitted_100ns: i64,
    epoch: AdmissionEpoch,
}

fn input_timing(
    receipt: Receipt,
    received: QpcTick,
    tick: Option<MessageTickMs>,
) -> Result<Option<InputTiming>, ProbeError> {
    if received < receipt.boundary.qpc {
        return Ok(None);
    }
    let recv_t_us = receipt.clock.time(received)?;
    let coarse = tick.map(CoarseTime::windows_message);
    // A queued message can survive the boundary. Exclude it only when
    // its entire coarse time interval predates admission.
    if coarse.and_then(|coarse| clock::message_tick_estimate(receipt.clock, receipt.tick_anchor, coarse))
        .is_some_and(|estimate| estimate.predates(receipt.boundary.time))
    {
        return Ok(None);
    }
    Ok(Some(InputTiming {
        time: RecordTime { recv_t_us, source_t_us: None, source_uncertainty_us: None, coarse },
        received,
        admitted_100ns: receipt.clock.offset_100ns(received)?,
        epoch: receipt.boundary.epoch,
    }))
}

/// Context losses as typed gap entries of the context stream, and context
/// health rows; an auxiliary activity loss is also an L-auxiliary report (C3).
fn publish_context_health(
    native: &mut Context<'_>, context: &mut JournalIngress, prefix: &ReceiptPrefix, session: &WorkerSession,
    receipt: Receipt, observed: QpcTick,
) -> Result<(), ProbeError> {
    use crate::{package::{journal::HealthRecord, metadata::LostActivity}, session::fault::FaultReport};
    let Some(timing) = input_timing(receipt, observed, None)? else { return Ok(()) };
    let admitted = prefix.raised(timing.time.recv_t_us);
    for loss in native.take_losses(receipt, observed) {
        context.record_loss(loss, admitted)?;
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

fn send_event(
    journal: &mut JournalIngress, event: serde_json::Value, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next_context_index: &mut u64,
) -> Result<(), ProbeError> {
    send_event_with_priority(journal, event, timing, prefix, next_context_index, false)
}

fn send_event_with_priority(
    journal: &mut JournalIngress, mut event: serde_json::Value, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next_context_index: &mut u64, lifecycle: bool,
) -> Result<(), ProbeError> {
    apply_context_envelope(&mut event, timing)?;
    let disposition = if lifecycle {
        journal.submit_boundary(timing.time.recv_t_us, [event])?;
        Submit::Queued
    } else {
        journal.try_submit(timing.time.recv_t_us, event)?
    };
    prefix.advance(timing.received, timing.time.recv_t_us, disposition)?;
    // Control rows use this context index. The journal writer allocates its own sequence.
    *next_context_index = next_context_index.checked_add(1)
        .ok_or_else(|| unavailable("The recording event counter overflowed"))?;
    Ok(())
}


fn send_boundary_events(
    journal: &mut JournalIngress, mut events: Vec<serde_json::Value>, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next_context_index: &mut u64,
) -> Result<(), ProbeError> {
    let count = events.len();
    for event in &mut events {
        apply_context_envelope(event, timing)?;
    }
    journal.submit_boundary(timing.time.recv_t_us, events)?;
    for _ in 0..count {
        prefix.advance(timing.received, timing.time.recv_t_us, Submit::Queued)?;
        *next_context_index = next_context_index.checked_add(1)
            .ok_or_else(|| unavailable("The recording event counter overflowed"))?;
    }
    Ok(())
}

/// The envelope of a context row. The journal stamps its `recv_t_us`, the
/// receipt it is submitted at. The package format version (manifest v6)
/// versions every row; a row's own `version` is the foreground product version (D-1).
fn apply_context_envelope(event: &mut serde_json::Value, timing: InputTiming) -> Result<(), ProbeError> {
    RowEnvelope::new(timing.time, None, timing.epoch).write_keeping_tick(event)
        .map_err(|_| unavailable("The input record is not an object"))
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use serde_json::{Value, json};

    use super::*;
    use crate::protocol::{
        FocusGeneration, SourceSequence, WindowId,
        ipc::{GapCause, ProcessInstance, Producer, elevated::WindowInstance},
    };

    const VIDEO: PausedVideo = PausedVideo { monitor: MonitorId(1), first_paused_slot: 12, fps: 30 };
    const VISIT: &str = r#"{"epoch":1,"focus_generation":0,"focus_revision":0,"foreground":{"process":{"creation_filetime":0,"pid":1},"thread":1,"window":1},"grant":1,"resync_revision":1,"source_generation":1,"witness_revision":0}"#;
    const GAP: &str = r#"{"cause":"overflow","first_qpc":400,"first_sequence":3,"last_qpc":450,"last_sequence":5,"producer":"raw_input"}"#;

    fn visit() -> VisitBinding {
        VisitBinding {
            epoch: AdmissionEpoch(1), source_generation: NonZeroU64::MIN, grant: NonZeroU64::MIN, witness_revision: 0,
            foreground: WindowInstance {
                window: WindowId(1), process: ProcessInstance { pid: NonZeroU32::MIN, creation_filetime: 0 }, thread: NonZeroU32::MIN,
            },
            focus_generation: FocusGeneration(0), focus_revision: 0, resync_revision: NonZeroU64::MIN,
        }
    }

    fn gap() -> ProducerGap {
        ProducerGap {
            producer: Producer::RawInput, cause: GapCause::Overflow, first_sequence: SourceSequence(3),
            last_sequence: SourceSequence(5), first_qpc: QpcTick(400), last_qpc: QpcTick(450),
        }
    }

    #[test]
    fn pause_bytes() -> serde_json::Result<()> {
        let tail = UncertifiedInputTail { first_t_us: SessionTimeUs(150), cutoff_t_us: SessionTimeUs(200), reason: None };
        let pause = pause_row(&[PauseReason::User, PauseReason::LowDisk], SessionTimeUs(200), Some(tail), &[VIDEO], true,
            Some(QpcTick(777)))?;
        assert_eq!(serde_json::to_string(&pause)?,
            r#"{"cutoff_t_us":200,"kind":"pause","paused_slots":[{"first_paused_slot":12,"fps":30,"monitor":1}],"paused_slots_all_monitors":true,"reason":"user","reasons":["user","low_disk"],"required_hook_first_uncovered_qpc":777,"uncertified_input_tail":{"cutoff_t_us":200,"first_t_us":150}}"#);
        assert_eq!(serde_json::to_string(&pause_row(&[], SessionTimeUs(200), None, &[], false, None)?)?,
            r#"{"cutoff_t_us":200,"kind":"pause","paused_slots":[],"paused_slots_all_monitors":false,"reason":null,"reasons":[],"uncertified_input_tail":null}"#);
        Ok(())
    }

    #[test]
    fn resume_bytes() -> serde_json::Result<()> {
        let closed = AdmissionBoundary { epoch: AdmissionEpoch(3), qpc: QpcTick(2_000), time: SessionTimeUs(200) };
        assert_eq!(serde_json::to_string(&discontinuity_row(closed, SessionTimeUs(900), &[PauseReason::Sleep])?)?,
            r#"{"end_t_us":900,"gap_us":700,"kind":"discontinuity","previous_admission_epoch":3,"reasons":["sleep"],"start_t_us":200}"#);
        let slots = paused_slot_ranges(&[VIDEO], closed.time, SessionTimeUs(900));
        let health = BTreeMap::from([(HealthKind::VideoFailure, 2)]);
        assert_eq!(serde_json::to_string(&resume_row(&[PauseReason::Sleep], closed.time, &slots, 4, &health)?)?,
            r#"{"kind":"resume","paused_slots":[{"end_slot_exclusive":13,"first_paused_slot":12,"fps":30,"monitor":1}],"paused_start_t_us":200,"reason":"sleep","reasons":["sleep"],"suppressed_context_notifications":4,"suppressed_health":{"video_failure":2}}"#);
        Ok(())
    }

    #[test]
    fn input_source_loss_bytes() -> serde_json::Result<()> {
        let row = source_loss_row(visit(), QpcTick(500), CoverageReason::NativeGap, Some(gap()))?;
        assert_eq!(serde_json::to_string(&row)?, format!(
            r#"{{"cause":"native_gap","first_uncovered_qpc":500,"kind":"input_source_loss","native_gap":{GAP},"source":"elevated","visit":{VISIT}}}"#));
        let row = source_loss_row(visit(), QpcTick(500), CoverageReason::Opening, None)?;
        assert_eq!(serde_json::to_string(&row)?, format!(
            r#"{{"cause":"opening","first_uncovered_qpc":500,"kind":"input_source_loss","native_gap":null,"source":"elevated","visit":{VISIT}}}"#));
        let loss = InputStagingLoss { visit: visit(), before: QpcTick(500), gaps: [None, Some(gap()), None, None, None], malformed: 2 };
        assert_eq!(serde_json::to_string(&staging_loss_row(&loss)?)?, format!(
            r#"{{"cause":"recorder_staging","first_uncovered_qpc":500,"kind":"input_source_loss","malformed":2,"native_gaps":[null,{GAP},null,null,null],"source":"elevated","visit":{VISIT}}}"#));
        Ok(())
    }

    #[test]
    fn checkpoint_context_bytes() -> serde_json::Result<()> {
        let row = checkpoint_context_row(RequestId(5), Some(SessionTimeUs(300)), Err::<Value, _>(CheckpointUnavailable::QueueFull))?;
        assert_eq!(serde_json::to_string(&row)?,
            r#"{"control_receipt_t_us":300,"kind":"checkpoint_context","reason":"queue_full","request_id":5,"save_context":null,"status":"unavailable"}"#);
        let row = checkpoint_context_row(RequestId(6), None, Ok(json!({ "id": 1 })))?;
        assert_eq!(serde_json::to_string(&row)?,
            r#"{"control_receipt_t_us":null,"kind":"checkpoint_context","reason":null,"request_id":6,"save_context":{"id":1},"status":"observed"}"#);
        Ok(())
    }
}
