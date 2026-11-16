mod gate;
mod hid;
mod hook_health;
mod receiver;
mod resync;
pub(crate) use resync::on_input_desktop;
mod text;

use std::{
    collections::BTreeMap,
    ops::ControlFlow,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crate::{
    capture::input::HeldBoundary,
    capture::save::{ConfirmationSource, ConfirmedSave, FixtureSaveError, FixtureSaveResult, MonitorRoutingBasis,
        SaveEvidence, SaveRouting},
    package::{diagnostics::ReportOutcome, journal::{Flush, FlushRequest, JournalIngress, Submit}, metadata::{AttemptLayout, InputCapture, InputPublication}},
    protocol::{AdmissionEpoch, CoarseTime, MonitorId, MessageTickMs, QpcTick, RecordTime, SessionId, SessionTimeUs, ipc::{ConfigureInput}},
    session::{
        AdmissionBoundary, PauseReason, hotkey::ControlOutcome, windows::{Admission, ControlPrefix, FixtureNotice, PauseProducer, Receipt, WorkerSession},
    },
};

use super::{ProbeError, RecordingReport, clock, clock_error, context::{Context, FramePolicy}, input_helper::{HelperError, InputHelper, READY_WAIT}, mp4::io_error, package_journal_error, still_rows, stills, unavailable};

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
}

impl InputPrefix {
    pub(super) fn cancelled() -> Self {
        Self {
            cutoff: None,
            input: ReceiptPrefix::default(),
            context: ReceiptPrefix::default(),
            video_health: Vec::new(),
            capture: None,
        }
    }
}

#[derive(Default)]
pub(super) struct ReceiptPrefix {
    pub(super) queued: u64,
    pub(super) last_queued: Option<SessionTimeUs>,
    last_receipt: Option<QpcTick>,
    pub(super) last_time: Option<SessionTimeUs>,
}

impl ReceiptPrefix {
    fn advance(&mut self, received: QpcTick, admitted: SessionTimeUs, disposition: Submit) -> Result<(), ProbeError> {
        if self.last_receipt.is_some_and(|previous| previous > received) {
            return Err(unavailable("Input receipts are out of order"));
        }
        self.last_receipt = Some(received);
        self.advance_time(admitted, disposition)
    }

    fn advance_time(&mut self, admitted: SessionTimeUs, disposition: Submit) -> Result<(), ProbeError> {
        if self.last_time.is_some_and(|previous| previous > admitted) {
            return Err(unavailable("Input receipts are out of order"));
        }
        self.last_time = Some(admitted);
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
    mut input: JournalIngress,
    mut context: JournalIngress,
    topology: &super::display::topology::Topology,
    still_endpoints: &BTreeMap<MonitorId, stills::Requests>,
    still_rows: &still_rows::Rows,
    reports: Option<&mpsc::SyncSender<RecordingReport>>,
    executable_versions: super::context::ExecutableVersions,
) -> Result<InputPrefix, ProbeError> {
    let mut helper = InputHelper::start(ConfigureInput {
        session: SessionId::new(layout.id().as_str())
            .map_err(|_| unavailable("The input helper session identity is invalid"))?,
        attempt: layout.id().clone(),
        qpc_frequency: clock::frequency()?,
        pen_hid: session.config().profile().pen_hid(),
    })?;
    let deadline = Instant::now() + READY_WAIT;
    let registration = loop {
        if let Some(registration) = helper.poll_status()? { break registration; }
        if let Some(code) = helper.exit_code()? {
            // The child may have published a reason immediately before exit.
            helper.poll_status()?;
            return Err(HelperError::UnexpectedExit(code).into());
        }
        if matches!(session.admit()?, Admission::Closed(_)) {
            helper.notify(session.take_input_environment())?;
            helper.shutdown(session.stop_deadline(), drop)?;
            let _ = ready.try_send(InputStartup::Cancelled);
            return Ok(InputPrefix::cancelled());
        }
        if Instant::now() >= deadline {
            return Err(HelperError::ReadyTimeout.into());
        }
        let _ = session.wait_for_close(Duration::from_millis(5))?;
    };
    session.input_ready(registration);
    if !registration.raw_keyboard || !registration.raw_pointer
        || !registration.keyboard_hook || !registration.pointer_hook
        || (session.config().profile().pen_hid() && !registration.pen_hid)
    {
        return Err(unavailable("The input helper did not confirm all required input registrations"));
    }
    let mut helper = Some(helper);
    let mut retired_helper = None;
    let mut native_context = Context::start(layout, session.config(), topology, executable_versions)?;
    let input_settings = native_context.input_settings();
    let remote = unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
        GetSystemMetrics(SM_REMOTESESSION) != 0
    };
    ready.try_send(InputStartup::Ready).map_err(|_| unavailable("The recording coordinator is unavailable"))?;
    let mut input_prefix = ReceiptPrefix::default();
    let mut context_prefix = ReceiptPrefix::default();
    let mut next_legacy_index = 0_u64;
    let mut video_health = Vec::new();
    let mut video_health_seen = 0;
    let mut receiver = receiver::Receiver::new(session.hotkey_keys(), still_endpoints.clone(), still_rows.clone());
    let mut input_flush = DeferredFlush::default();
    let mut context_flush = DeferredFlush::default();
    let mut journal_revision = session.journal_flush_revision();
    let mut paused = None;
    let cutoff = 'attempt: loop {
        let cutoff = loop {
            let observed_at = clock::qpc();
            let keep_pumping = (|| {
                let observed_at = observed_at?;
                let receipt = match session.admit()? {
                    Admission::Starting => {
                        if let Some(child) = &mut helper {
                            child.notify(session.take_input_environment())?;
                            child.poll(|record| receiver.countdown(record))?;
                        }
                        return Ok(ControlFlow::Continue(()));
                    }
                    Admission::Closed(cutoff) => return Ok(ControlFlow::Break(cutoff)),
                    Admission::Open(receipt) => receipt,
                };
                if observed_at < receipt.boundary.qpc {
                    return Ok(ControlFlow::Continue(()));
                }
                if let Some(timing) = input_timing(receipt, observed_at, None)? {
                    for event in native_context.poll(receipt, observed_at, frame_policy)? {
                        send_event(&mut context, event, timing, &mut context_prefix, &mut next_legacy_index)?;
                    }
                }
                for notice in session.take_fixture_saves() {
                    let result = route_fixture(&notice, session, layout, &mut native_context, still_endpoints, still_rows, receipt);
                    let _ = notice.reply.try_send(result);
                }
                let controls = session.take_hotkeys();
                let wrote_control = controls.loss.is_some() || controls.notices().next().is_some();
                receiver.accept_controls(&controls, receipt)?;
                if let Some(child) = &mut helper {
                    let mut accepted = Ok(());
                    let polled = child.notify(session.take_input_environment()).and_then(|()| child.poll(|record| {
                        accepted = std::mem::replace(&mut accepted, Ok(())).and_then(|()| receiver.accept(record, receipt, None, &mut native_context));
                    }));
                    accepted?;
                    let at = receipt.clock.time(receipt.qpc).map_err(clock_error)?;
                    receiver.advance(at);
                    for producer in receiver.take_hook_reports().into_iter().flatten() {
                        note_helper_failure(session, reports, HelperError::SuspectedHookLoss(producer), at);
                    }
                    let failure = polled.err().or_else(|| receiver.stalled_producer(at).map(HelperError::ProducerStalled));
                    if let Some(error) = failure {
                        note_helper_failure(session, reports, error, at);
                        retire_helper(&mut helper, &mut retired_helper);
                        receiver.interrupt(at, HeldBoundary::AdmissionClosed);
                    } else {
                        receiver.try_synchronize(receipt, at, &native_context)?;
                    }
                }
                publish_controls(controls, receipt, observed_at,
                    &mut context, &mut context_prefix, &mut next_legacy_index, Some(&native_context))?;
                for failed in session.video_failures_after(video_health_seen) {
                    let Some(timing) = input_timing(receipt, observed_at, None)? else { break };
                    let event = serde_json::json!({
                        "kind": "health", "monitor": failed.monitor, "failure": failed.failure,
                        "detected_t_us": failed.at, "admitted_100ns": timing.admitted_100ns,
                    });
                    let before = context_prefix.queued;
                    send_event(&mut context, event, timing, &mut context_prefix, &mut next_legacy_index)?;
                    video_health_seen += 1;
                    if context_prefix.queued > before {
                        video_health.push(failed.monitor);
                    }
                }
                let watermark = receipt.clock.time(observed_at).map_err(clock_error)?;
                for text in native_context.take_confirmed_text(observed_at) {
                    receiver.stage_confirmed(text, receipt);
                }
                receiver.publish(&mut native_context, &mut input, &mut input_prefix, receipt, watermark, false)?;
                receiver.poll_pointer_rest(&mut native_context, receipt, observed_at)?;
                receiver.advance_stills(watermark, helper.is_some());
                if let Some(timing) = input_timing(receipt, observed_at, None)? {
                    for event in native_context.poll_clipboard(receipt, observed_at) {
                        send_event(&mut context, event, timing, &mut context_prefix, &mut next_legacy_index)?;
                    }
                }
                context.offer_gap(watermark).map_err(package_journal_error)?;
                let revision = session.journal_flush_revision();
                let closed_segment = journal_revision != revision;
                journal_revision = revision;
                input_flush.poll(&mut input, input_prefix.last_time.map_or(watermark, |at| at.max(watermark)), closed_segment)?;
                context_flush.poll(&mut context, watermark, closed_segment || wrote_control)?;
                Ok::<_, ProbeError>(ControlFlow::Continue(()))
            })();

            if let ControlFlow::Break(cutoff) = keep_pumping? {
                break cutoff;
            }
            if let Some(cutoff) = session.wait_for_close(Duration::from_millis(5))? {
                break Some(cutoff);
            }
        };
        let Some(closed) = cutoff.filter(|cutoff| session.pause_pending(*cutoff)) else {
            break 'attempt cutoff;
        };
        let receipt = session.foreground_closing_receipt(closed)?;
        let until = Instant::now() + Duration::from_millis(250);
        while session.pause_pending(closed) && !receiver.certified_through(closed.time)
            && Instant::now() < until && helper.is_some()
        {
            poll_paused_helper(&mut helper, &mut retired_helper, &mut receiver, &mut native_context,
                session, reports, receipt, closed, true)?;
            thread::park_timeout(Duration::from_millis(2));
        }
        let tail = receiver.uncertified_tail(receipt.boundary.time, closed.time);
        let controls = session.take_hotkeys();
        let mut resume_chord = controls.notices().any(|notice| notice.outcome == ControlOutcome::ResumeRequested);
        receiver.accept_controls(&controls, receipt)?;
        receiver.interrupt(closed.time, HeldBoundary::Pause);
        receiver.publish(&mut native_context, &mut input, &mut input_prefix, receipt, closed.time, true)?;
        for notice in session.take_fixture_saves() {
            let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
        }
        native_context.pause(closed.qpc)?;
        flush_boundary(&mut input, closed.time, session, || {
            poll_paused_helper(&mut helper, &mut retired_helper, &mut receiver, &mut native_context,
                session, reports, receipt, closed, false)
        })?;
        receiver.finish_still_prefix(closed.time);
        session.mark_input_prefix(closed);
        let timing = input_timing(receipt, closed.qpc, None)?
            .ok_or_else(|| unavailable("The input pause cutoff is invalid"))?;
        send_boundary_events(&mut context, native_context.close_hung_observation(), timing,
            &mut context_prefix, &mut next_legacy_index)?;
        let ranges = loop {
            let ranges = session.paused_video();
            if ranges.len() == session.config().monitors().len() || !session.pause_pending(closed) { break ranges; }
            poll_paused_helper(&mut helper, &mut retired_helper, &mut receiver, &mut native_context,
                session, reports, receipt, closed, false)?;
            thread::park_timeout(Duration::from_millis(2));
        };
        let reasons = session.pause_interval_reasons(closed);
        let pause = serde_json::json!({
            "kind": "pause", "reasons": reasons, "cutoff_t_us": closed.time,
            "uncertified_input_tail": tail, "paused_slots": ranges,
            "paused_slots_all_monitors": ranges.len() == session.config().monitors().len(),
        });
        send_boundary_event(&mut context, pause, timing, &mut context_prefix, &mut next_legacy_index)?;
        publish_controls(controls, receipt, closed.qpc, &mut context, &mut context_prefix,
            &mut next_legacy_index, None)?;
        flush_boundary(&mut context, closed.time, session, || {
            poll_paused_helper(&mut helper, &mut retired_helper, &mut receiver, &mut native_context,
                session, reports, receipt, closed, false)
        })?;
        session.finish_pause(closed, PauseProducer::Input);
        let mut discarded_context = 0_u64;
        while session.pause_pending(closed) {
            let at = clock::qpc()?;
            poll_paused_helper(&mut helper, &mut retired_helper, &mut receiver, &mut native_context,
                session, reports, receipt, closed, false)?;
            discarded_context = discarded_context.saturating_add(native_context.discard_paused_notifications());
            if !session.pause_pending(closed) { break; }
            let controls = session.take_hotkeys();
            let wrote_control = controls.loss.is_some() || controls.notices().next().is_some();
            resume_chord |= controls.notices().any(|notice| notice.outcome == ControlOutcome::ResumeRequested);
            publish_controls(controls, receipt, at, &mut context, &mut context_prefix,
                &mut next_legacy_index, None)?;
            let through = receipt.clock.time(at).map_err(clock_error)?;
            context_flush.poll(&mut context, through, wrote_control)?;
            thread::park_timeout(Duration::from_millis(5));
        }
        let Some(opened) = session.wait_for_reopen(closed)? else {
            paused = Some((closed, session.pause_interval_reasons(closed), ranges));
            break 'attempt Some(closed);
        };
        let reasons = session.pause_interval_reasons(closed);
        let resumed = Receipt { qpc: opened.boundary.qpc, clock: opened.clock, boundary: opened.boundary, tick_anchor: None };
        let timing = input_timing(resumed, opened.boundary.qpc, None)?
            .ok_or_else(|| unavailable("The input resume boundary is invalid"))?;
        publish_blind_pause(&mut context, timing, &mut context_prefix, &mut next_legacy_index, closed, &reasons)?;
        send_boundary_event(&mut context, serde_json::json!({
            "kind": "discontinuity", "start_t_us": closed.time, "end_t_us": opened.boundary.time,
            "gap_us": opened.boundary.time.0.saturating_sub(closed.time.0),
            "previous_admission_epoch": closed.epoch, "reasons": reasons,
        }), timing, &mut context_prefix, &mut next_legacy_index)?;
        let paused_slots = paused_slot_ranges(&ranges, closed.time, opened.boundary.time);
        send_boundary_event(&mut context, serde_json::json!({
            "kind": "resume", "reasons": reasons, "paused_start_t_us": closed.time,
            "paused_slots": paused_slots, "suppressed_context_notifications": discarded_context,
        }), timing, &mut context_prefix, &mut next_legacy_index)?;
        native_context.resume();
        receiver.reopen(opened.boundary.time, resume_chord);
    };
    for notice in session.take_fixture_saves() {
        let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
    }
    let final_controls = session.take_hotkeys();
    if let Some(cutoff) = cutoff.filter(|_| paused.is_none()) {
        receiver.accept_controls(&final_controls, session.foreground_closing_receipt(cutoff)?)?;
    }
    if let Some(mut child) = helper {
        let closed = cutoff.map(|boundary| session.foreground_closing_receipt(boundary)).transpose()?;
        let mut accepted = Ok(());
        let shutdown = child.notify(session.take_input_environment()).and_then(|()| child.shutdown(session.stop_deadline(), |record| {
            if paused.is_none() && let (Some(receipt), Some(cutoff)) = (closed, cutoff) {
                accepted = std::mem::replace(&mut accepted, Ok(())).and_then(|()| receiver.accept(record, receipt, Some(cutoff), &mut native_context));
            }
        }));
        accepted?;
        if let Err(error) = shutdown {
            if let Some(cutoff) = cutoff {
                note_helper_failure(session, reports, error, cutoff.time);
            } else {
                return Err(error.into());
            }
        }
    }
    let mut uncertified_closing_tail = None;
    if let Some(cutoff) = cutoff.filter(|_| paused.is_none()) {
        let opened = session.foreground_closing_receipt(cutoff)?.boundary.time;
        uncertified_closing_tail = receiver.uncertified_tail(opened, cutoff.time);
        receiver.interrupt(cutoff.time, HeldBoundary::Stop);
        receiver.publish(&mut native_context, &mut input, &mut input_prefix,
            session.foreground_closing_receipt(cutoff)?, cutoff.time, true)?;
        flush_boundary(&mut input, cutoff.time, session, || Ok(()))?;
        receiver.finish_still_prefix(cutoff.time);
        session.mark_input_prefix(cutoff);
        for producer in receiver.take_hook_reports().into_iter().flatten() {
            note_helper_failure(session, reports, HelperError::SuspectedHookLoss(producer), cutoff.time);
        }
    }
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
                &mut context_prefix, &mut next_legacy_index)?;
            for event in native_context.finish_cursor(cutoff.qpc, receipt.clock) {
                send_event(&mut context, event, timing, &mut context_prefix, &mut next_legacy_index)?;
            }
        }
        if let Some(timing) = input_timing(receipt, stopped.qpc, None)? {
            let paused_slots = if let Some((closed, reasons, ranges)) = paused {
                publish_blind_pause(&mut context, timing, &mut context_prefix, &mut next_legacy_index, closed, &reasons)?;
                Some(paused_slot_ranges(&ranges, closed.time, stopped.time))
            } else { None };
            send_boundary_event(&mut context, serde_json::json!({
                "kind": "recorder_control", "command": "stop_boundary",
                "cutoff_t_us": cutoff.time, "stop_t_us": stopped.time, "paused_slots": paused_slots,
            }), timing, &mut context_prefix, &mut next_legacy_index)?;
        }
        publish_controls(final_controls, receipt, stopped.qpc, &mut context, &mut context_prefix,
            &mut next_legacy_index, None)?;
        input.seal(cutoff.time).map_err(package_journal_error)?;
        context.seal(stopped.time).map_err(package_journal_error)?;
    } else if input_prefix.last_time.is_some() || context_prefix.last_receipt.is_some() {
        return Err(unavailable("The admitted input prefix has no closing boundary"));
    }
    use crate::package::metadata::{PenCapture, PenPressure, TabletDriverMode, TabletInputPath};
    use crate::protocol::{Evidence, UnavailableReason, ipc::DeviceKind};
    let (pen_observed, pressure_observed) = receiver.pen_observation();
    let (devices, device_inventory_truncated) = receiver.finish_inventory()?;
    let pen_pressure = if pressure_observed {
        PenPressure::Captured
    } else if !registration.pen_hid {
        PenPressure::Disabled
    } else if remote {
        PenPressure::UnavailableVmOrRemote
    } else if !device_inventory_truncated && !devices.as_slice().iter().any(|descriptor|
        matches!(descriptor.kind, DeviceKind::Hid { usage_page: 0x0D, usage: 1 | 2 }))
    {
        PenPressure::UnavailableNoHidCollection
    } else {
        PenPressure::UnavailableUnknown
    };
    let pen = PenCapture {
        pressure: pen_pressure,
        observed_path: pen_observed.then_some(TabletInputPath::NativeDigitizerHid),
        driver_mode: TabletDriverMode::Unknown,
        app_tablet_settings: Evidence::Unavailable(UnavailableReason::Unsupported),
    };
    let (keyboard_hook_loss_suspected, pointer_hook_loss_suspected) = receiver.hook_loss();
    Ok(InputPrefix {
        cutoff, input: input_prefix, context: context_prefix, video_health,
        capture: Some(InputCapture {
            registration, pen: Some(pen), publication: InputPublication::ApprovedReceiptGate, settings: Some(input_settings),
            transport_overflow: receiver.overflowed(), devices: Some(devices), device_inventory_truncated,
            keyboard_hook_loss_suspected, pointer_hook_loss_suspected, uncertified_closing_tail, blind: None,
        }),
    })
}

fn poll_paused_helper(
    helper: &mut Option<InputHelper>, retired: &mut Option<InputHelper>, receiver: &mut receiver::Receiver, native: &mut Context<'_>,
    session: &WorkerSession, reports: Option<&mpsc::SyncSender<RecordingReport>>, receipt: Receipt,
    cutoff: AdmissionBoundary, admitted: bool,
) -> Result<(), ProbeError> {
    if let Some(child) = helper {
        let mut accepted = Ok(());
        let result = child.notify(session.take_input_environment()).and_then(|()| child.poll(|record| {
            if admitted {
                accepted = std::mem::replace(&mut accepted, Ok(())).and_then(|()| receiver.accept(record, receipt, Some(cutoff), native));
            } else {
                receiver.inventory(record);
            }
        }));
        accepted?;
        if let Err(error) = result {
            note_helper_failure(session, reports, error, session.session_time().unwrap_or(cutoff.time));
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
                Ok(outcome) => { outcome.map_err(package_journal_error)?; self.waiting = None; }
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => return Err(unavailable("A requested journal flush did not complete")),
            }
        }
        if self.requested && let FlushRequest::Queued(waiting) = journal.try_flush(Flush::Os, at)
            .map_err(package_journal_error)?
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
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut waiting: Option<crate::package::journal::FlushOutcome> = None;
    loop {
        if let Some(result) = &waiting {
            match result.try_recv() {
                Ok(summary) => return summary.map(|_| ()).map_err(package_journal_error),
                Err(mpsc::TryRecvError::Disconnected) => return Err(unavailable("The journal pause flush did not complete")),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        } else if let FlushRequest::Queued(result) = journal.try_flush(Flush::Os, at).map_err(package_journal_error)? {
            waiting = Some(result);
        }
        if Instant::now() >= session.stop_deadline().map_or(deadline, |stop| stop.min(deadline)) {
            return Err(unavailable("The lifecycle journal flush exceeded its deadline"));
        }
        drain()?;
        thread::park_timeout(Duration::from_millis(2));
    }
}

fn paused_slot_ranges(
    ranges: &[crate::session::windows::PausedVideo], closed: SessionTimeUs, through: SessionTimeUs,
) -> Vec<serde_json::Value> {
    ranges.iter().map(|range| serde_json::json!({
        "monitor": range.monitor, "fps": range.fps, "first_paused_slot": range.first_paused_slot,
        "end_slot_exclusive": range.end_exclusive(closed, through),
    })).collect()
}

fn publish_blind_pause(
    context: &mut JournalIngress, timing: InputTiming, prefix: &mut ReceiptPrefix, next: &mut u64,
    boundary: AdmissionBoundary, reasons: &[PauseReason],
) -> Result<(), ProbeError> {
    if reasons.iter().any(|reason| matches!(reason, PauseReason::LockScreen | PauseReason::SecureDesktop)) {
        send_boundary_event(context, serde_json::json!({
            "kind": "blind", "reason": "secure_desktop",
            "start_t_us": boundary.time, "end_t_us": timing.time.recv_t_us,
        }), timing, prefix, next)?;
    }
    Ok(())
}

fn send_boundary_event(
    journal: &mut JournalIngress, event: serde_json::Value, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next: &mut u64,
) -> Result<(), ProbeError> {
    send_event_with_priority(journal, event, timing, prefix, next, true)
}

fn note_helper_failure(
    session: &WorkerSession, reports: Option<&mpsc::SyncSender<RecordingReport>>,
    error: HelperError, at: SessionTimeUs,
) {
    let failure = error.runtime_failure(at);
    if !session.input_failed(failure) {
        return;
    }
    if reports.is_none_or(|reports| reports.try_send(RecordingReport::Input(failure)).is_err()) {
        session.set_input_report_for(failure, ReportOutcome::Unavailable { native_code: None });
    }
}

fn route_fixture(
    notice: &FixtureNotice, session: &WorkerSession, layout: &AttemptLayout, native: &mut Context<'_>,
    endpoints: &BTreeMap<MonitorId, stills::Requests>, rows: &still_rows::Rows, open: Receipt,
) -> FixtureSaveResult {
    let received = notice.receipt;
    if received.boundary != open.boundary {
        return Err(FixtureSaveError::AdmissionClosed);
    }
    let signal = &notice.signal;
    let proof = native.fixture_save_proof(&signal.executable, received.qpc)
        .ok_or(FixtureSaveError::UnapprovedContext)?;
    let routing = if let Some(monitors) = &signal.affected_monitors {
        SaveRouting::Monitors { monitors: monitors.clone(), basis: MonitorRoutingBasis::ApplicationGeometry }
    } else {
        native.checkpoint_context(received.qpc).ok_or(FixtureSaveError::InvalidRouting)?.fallback_routing()
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
        let (trigger, role) = match confirmation.kind() {
            crate::capture::save::SaveKind::Checkpoint => ("checkpoint", "image"),
            crate::capture::save::SaveKind::Final => ("final", "final_save_settled"),
        };
        let mut fallback = stills::dropped_trigger(trigger, role, time, geometry, open.boundary.epoch, "save_handoff_unavailable");
        fallback["save"] = serde_json::json!({ "id": confirmation.id(), "kind": confirmation.kind(),
            "routing_basis": confirmation.routing_basis() });
        let Some(reservation) = rows.reserve(time, Some(fallback)) else {
            rows.note_loss(time);
            continue;
        };
        if endpoint.confirmed_save(stills::Save { reservation, confirmation, geometry, admission: open.boundary.epoch, proof }) {
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
    next: &mut u64,
    native: Option<&Context<'_>>,
) -> Result<(), ProbeError> {
    let Some(timing) = input_timing(receipt, observed, None)? else { return Ok(()) };
    if let Some(loss) = controls.loss {
        let event = serde_json::json!({
            "kind": "recorder_control_gap",
            "index": *next,
            "admitted_100ns": timing.admitted_100ns,
            "dropped": loss.count,
            "first_control_receipt_t_us": receipt.clock.time(loss.first).map_err(clock_error)?,
            "last_control_receipt_t_us": receipt.clock.time(loss.last).map_err(clock_error)?,
        });
        send_boundary_event(context, event, timing, prefix, next)?;
    }
    for notice in controls.into_notices() {
        let activation = notice.activation;
        let request = matches!(notice.outcome, ControlOutcome::CheckpointRequested)
            .then(|| activation.checkpoint().id);
        let save_context = request.and_then(|_| native.and_then(|context| context.checkpoint_context(activation.at)));
        let event = serde_json::json!({
            "kind": "recorder_control",
            "index": *next,
            "admitted_100ns": timing.admitted_100ns,
            "command": activation.action,
            "outcome": notice.outcome,
            "request_id": request,
            "save_context": save_context,
            "control_receipt_t_us": receipt.clock.time(activation.at).map_err(clock_error)?,
        });
        send_boundary_event(context, event, timing, prefix, next)?;
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
    let recv_t_us = receipt.clock.time(received).map_err(clock_error)?;
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
        admitted_100ns: receipt.clock.offset_100ns(received).map_err(clock_error)?,
        epoch: receipt.boundary.epoch,
    }))
}

fn send_event(
    journal: &mut JournalIngress, event: serde_json::Value, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next: &mut u64,
) -> Result<(), ProbeError> {
    send_event_with_priority(journal, event, timing, prefix, next, false)
}

fn send_event_with_priority(
    journal: &mut JournalIngress, mut event: serde_json::Value, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next_legacy_index: &mut u64, lifecycle: bool,
) -> Result<(), ProbeError> {
    apply_context_envelope(&mut event, timing)?;
    let disposition = if lifecycle {
        journal.submit_boundary(timing.time.recv_t_us, [event]).map_err(package_journal_error)?;
        Submit::Queued
    } else {
        journal.try_submit(timing.time.recv_t_us, event).map_err(package_journal_error)?
    };
    prefix.advance(timing.received, timing.time.recv_t_us, disposition)?;
    // Transitional PNG export uses this attempt-wide evidence ID. Journal
    // sequence is allocated independently by the successful stream writer.
    *next_legacy_index = next_legacy_index.checked_add(1)
        .ok_or_else(|| unavailable("The recording event counter overflowed"))?;
    Ok(())
}


fn send_boundary_events(
    journal: &mut JournalIngress, mut events: Vec<serde_json::Value>, timing: InputTiming,
    prefix: &mut ReceiptPrefix, next_legacy_index: &mut u64,
) -> Result<(), ProbeError> {
    let count = events.len();
    for event in &mut events {
        apply_context_envelope(event, timing)?;
    }
    journal.submit_boundary(timing.time.recv_t_us, events).map_err(package_journal_error)?;
    for _ in 0..count {
        prefix.advance(timing.received, timing.time.recv_t_us, Submit::Queued)?;
        *next_legacy_index = next_legacy_index.checked_add(1)
            .ok_or_else(|| unavailable("The recording event counter overflowed"))?;
    }
    Ok(())
}

fn apply_context_envelope(event: &mut serde_json::Value, timing: InputTiming) -> Result<(), ProbeError> {
    let record = event.as_object_mut().ok_or_else(|| unavailable("The input record is not an object"))?;
    record.insert("version".into(), serde_json::json!(2));
    record.insert("recv_t_us".into(), serde_json::json!(timing.time.recv_t_us));
    record.insert("source_t_us".into(), serde_json::json!(timing.time.source_t_us));
    record.insert("source_uncertainty_us".into(), serde_json::json!(timing.time.source_uncertainty_us));
    record.entry("tick_ms").or_insert_with(|| serde_json::json!(timing.time.coarse.map(|time| time.tick_ms)));
    record.entry("tick_uncertainty_us").or_insert_with(|| serde_json::json!(timing.time.coarse.map(|time| time.uncertainty_us)));
    record.entry("late").or_insert(serde_json::Value::Null);
    record.insert("admission_epoch".into(), serde_json::json!(timing.epoch));
    Ok(())
}
