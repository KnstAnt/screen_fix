use std::{
    os::windows::io::BorrowedHandle,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use screen_fix_native_observation::{
    held,
    input::{self, CaptureScope, Collector, HookForeground, NativeError, Settings, loss::{self, Losses}},
};
use screen_fix_protocol::{
    QpcFrequency, QpcTick,
    ipc::{
        EnvironmentState, HelperFailure, HelperFailureStage, HelperToCore, InputRegistration, Producer,
        elevated::{GrantedInput, HeldSnapshot, InputEvidence, VisitBinding, runtime::CoverageReason},
    },
};

use crate::win::{Failure, Result};

use super::{grant::{Grant, InputLoss, INPUT_HOLDBACK}, witness::{HeldAccess, coarse_lower}};

pub(super) const SLOTS: usize = 256;

pub(super) struct Item {
    pub(super) grant: Arc<Grant>,
    pub(super) record: GrantedInput,
}

pub(super) struct Native {
    pub(super) ready: Receiver<InputRegistration>,
    worker: JoinHandle<std::result::Result<(), NativeError>>,
}

impl Native {
    pub(super) fn start(
        grant: Arc<Grant>, frequency: QpcFrequency, pen_hid: bool, send: SyncSender<Item>,
    ) -> Result<Self> {
        let (ready, registered) = mpsc::sync_channel(1);
        let worker = thread::Builder::new().name("elevated-native-input".into()).spawn(move || {
            let hooks = Arc::clone(&grant);
            let raw = Arc::clone(&grant);
            let scope_visit = Arc::clone(&grant);
            let scope = Arc::new(CaptureScope {
                foreground: grant.visit().foreground,
                hook_open: Arc::new(move || hooks.hook_stamp()),
                raw_open: Arc::new(move || raw.read_floor(false).is_ok() && raw.check(true).is_ok()),
                serial: Arc::new(move || scope_visit.scope_serial()),
            });
            let owner = unsafe { BorrowedHandle::borrow_raw(grant.parent.raw()) };
            let wake = unsafe { BorrowedHandle::borrow_raw(grant.stop.handle().raw()) };
            let mut output = Output {
                grant: Arc::clone(&grant), send, ready, losses: Losses::default(), environment: None, visit: None, raw_source: None,
                notices: Vec::new(),
            };
            input::run_scoped(Settings { qpc_frequency: frequency, pen_hid }, owner, wake, &mut output, Some(scope))
        }).map_err(|_| Failure::Supervisor)?;
        Ok(Self { ready: registered, worker })
    }

    pub(super) fn ended(&self) -> bool { self.worker.is_finished() }

    pub(super) fn join(self) -> Result<Option<HelperFailure>> {
        let failure = self.worker.join().map_err(|_| Failure::Supervisor)?.err();
        Ok(failure.map(NativeError::into_helper_failure))
    }
}

pub(super) fn held(grant: &Grant) -> Result<(QpcTick, QpcTick, Option<HeldSnapshot>)> {
    let start = grant.check(true)?;
    let access = HeldAccess::open(grant.target.window.thread)?;
    let state = held::read();
    if !access.current() { return Err(Failure::TargetAccess); }
    let end = grant.check(true)?;
    if end < start { return Err(Failure::Supervisor); }
    Ok((start, end, state))
}

struct Output {
    grant: Arc<Grant>,
    send: SyncSender<Item>,
    ready: SyncSender<InputRegistration>,
    losses: Losses,
    environment: Option<EnvironmentState>,
    visit: Option<VisitBinding>,
    /// Source lower bound of the latest Raw Input record with a message time.
    /// Records drained from the Raw Input buffer after it have no message
    /// time and were queued after it, so it bounds their source time too.
    raw_source: Option<QpcTick>,
    /// Optional-collection notices waiting for queue room (E3); at most one
    /// per collection.
    notices: Vec<InputEvidence>,
}

fn transport() -> NativeError { NativeError::without_code(HelperFailureStage::Transport) }

impl Collector for Output {
    fn stop_requested(&self) -> bool { self.grant.stopped() }
    fn take_environment(&mut self) -> u32 { 0 }

    fn ready(&mut self, registration: InputRegistration) -> std::result::Result<(), NativeError> {
        self.ready.try_send(registration).map_err(|_| transport())
    }

    fn offer_hook(&mut self, record: HelperToCore, proof: Option<HookForeground>) -> std::result::Result<(), NativeError> {
        if matches!(&record, HelperToCore::Input(_)) {
            let visit = self.grant.visit();
            let serial = self.grant.scope_serial();
            if proof.is_some_and(|proof| proof.serial != serial) { return Ok(()); }
            let target = visit.foreground;
            if proof != Some(HookForeground { window: target.window, pid: target.process.pid.get(), thread: target.thread.get(), serial }) {
                self.grant.mark_uncertain(visit, self.grant.arm);
                return Ok(());
            }
        }
        self.offer(record)
    }

    fn offer(&mut self, record: HelperToCore) -> std::result::Result<(), NativeError> {
        let visit = self.grant.visit();
        let floor = self.grant.read_floor(false).ok().filter(|_| self.grant.visit() == visit);
        if self.visit != Some(visit) {
            // The old binding's evidence is closed. Its pending gaps are kept
            // and delivered first in the new binding, where gaps before the
            // resync sample only count (E3).
            self.environment = None;
            self.raw_source = None;
            self.notices.clear();
            self.visit = Some(visit);
        }
        // A native gap is never dropped: outside the open scope it waits as a
        // pending loss (E3).
        let Some(floor) = floor else {
            if matches!(record, HelperToCore::Gap(_)) { self.losses.drop_record(&record); }
            return Ok(());
        };
        if !self.eligible(&record, visit, floor) {
            if matches!(record, HelperToCore::Gap(_)) { self.losses.drop_record(&record); }
            return Ok(());
        }
        if let Some(producer) = loss::producer(&record) && !self.flush_loss(producer)? {
            self.losses.drop_record(&record);
            return Ok(());
        }
        if let HelperToCore::Environment(state) = record {
            let mut state = state;
            if let Some(pending) = self.environment.take() { state.changes |= pending.changes; }
            if !self.enqueue(InputEvidence::Environment(state))? { self.environment = Some(state); }
            return Ok(());
        }
        if let HelperToCore::OptionalUnavailable { collection, observed_qpc } = record {
            self.notices.push(InputEvidence::OptionalUnavailable { collection, observed_qpc });
            return self.flush_notices();
        }
        let value = match &record {
            HelperToCore::Input(input) => InputEvidence::Input(input.clone()),
            HelperToCore::DeviceArrived { receipt, descriptor } => InputEvidence::DeviceArrived { receipt: *receipt, descriptor: descriptor.clone() },
            HelperToCore::DeviceRemoved { receipt, id } => InputEvidence::DeviceRemoved { receipt: *receipt, id: *id },
            HelperToCore::Progress(progress) => InputEvidence::Progress(*progress),
            HelperToCore::Gap(gap) => InputEvidence::Gap(*gap),
            _ => return Err(transport()),
        };
        if !self.enqueue(value)? { self.losses.drop_record(&record); }
        Ok(())
    }

    fn flush(&mut self) -> std::result::Result<(), NativeError> {
        if self.grant.read_floor(false).is_err() || self.visit != Some(self.grant.visit()) { return Ok(()) }
        for producer in Producer::ALL {
            if !self.flush_loss(producer)? { return Ok(()) }
        }
        if let Some(state) = self.environment
            && self.enqueue(InputEvidence::Environment(state))?
        {
            self.environment = None;
        }
        self.flush_notices()
    }

    fn finish(&mut self, _: BorrowedHandle<'_>) -> std::result::Result<(), NativeError> { Ok(()) }
}

impl Output {
    fn eligible(&mut self, record: &HelperToCore, visit: VisitBinding, floor: QpcTick) -> bool {
        match record {
            HelperToCore::Input(input) => {
                let horizon = floor.0.saturating_add(self.grant.lifetime.ticks(INPUT_HOLDBACK).unwrap_or(0));
                if input.receipt.qpc.0 <= horizon { return false; }
                let raw = input.sample.producer().raw_input();
                let source = match input.receipt.message_tick_ms {
                    Some(tick) => {
                        let source = coarse_lower(tick.0, &self.grant.lifetime);
                        if raw { self.raw_source = source; }
                        source
                    }
                    None if raw => self.raw_source,
                    None => None,
                };
                let in_scope = source.is_some_and(|source| source > floor && source <= input.receipt.qpc)
                    && match input.sample.producer() {
                        producer if producer.raw_input() => input.foreground_at_receipt.is_some_and(|foreground| {
                            let target = visit.foreground;
                            foreground.window == target.window && foreground.process == target.process
                                && foreground.focus == self.grant.expected_focus(visit)
                        }),
                        _ => true,
                    };
                if !in_scope {
                    // No details or event counts from ambiguous input leave this process.
                    self.grant.input_loss(visit, InputLoss {
                        before: source.unwrap_or(self.grant.arm).min(input.receipt.qpc),
                        reason: CoverageReason::UncertifiedSource,
                        gap: None,
                    });
                }
                in_scope
            }
            HelperToCore::DeviceArrived { receipt, .. } | HelperToCore::DeviceRemoved { receipt, .. } => receipt.qpc > floor,
            HelperToCore::Gap(gap) => gap.first_qpc > floor,
            HelperToCore::Progress(progress) => progress.before_qpc > floor,
            HelperToCore::Environment(state) => state.observed_qpc > floor,
            HelperToCore::OptionalUnavailable { observed_qpc, .. } => *observed_qpc > floor,
            _ => false,
        }
    }

    fn flush_notices(&mut self) -> std::result::Result<(), NativeError> {
        while let Some(notice) = self.notices.first() {
            if !self.enqueue(notice.clone())? { break; }
            self.notices.remove(0);
        }
        Ok(())
    }

    fn flush_loss(&mut self, producer: Producer) -> std::result::Result<bool, NativeError> {
        while let Some(gap) = self.losses.front(producer) {
            if !self.enqueue(InputEvidence::Gap(gap))? { return Ok(false); }
            self.losses.delivered(producer);
        }
        Ok(true)
    }

    fn enqueue(&self, evidence: InputEvidence) -> std::result::Result<bool, NativeError> {
        let Some(visit) = self.visit else { return Ok(false); };
        if self.grant.evidence_cancel(visit).is_none() { return Ok(false); }
        let record = GrantedInput { visit, evidence };
        match self.send.try_send(Item { grant: Arc::clone(&self.grant), record }) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(transport()),
        }
    }
}
