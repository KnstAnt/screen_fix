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
    input::{self, CaptureScope, Collector, HookForeground, NativeError, Settings, loss::{self, Losses, PendingStatus}},
};
use screen_fix_protocol::{
    QpcFrequency, QpcTick,
    ipc::{
        HelperFailure, HelperFailureStage, HelperToCore, InputRegistration, Producer,
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
                raw_open: Arc::new(move || raw.read_open(false).is_ok()),
                serial: Arc::new(move || scope_visit.scope_serial()),
            });
            let owner = unsafe { BorrowedHandle::borrow_raw(grant.parent.raw()) };
            let wake = unsafe { BorrowedHandle::borrow_raw(grant.stop.handle().raw()) };
            let mut output = Output {
                grant: Arc::clone(&grant), send, ready, losses: Losses::default(), status: PendingStatus::default(), visit: None, raw_source: None,
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
    /// Status records waiting for queue room (E3).
    status: PendingStatus,
    visit: Option<VisitBinding>,
    /// Source lower bound of the latest Raw Input record with a message time.
    /// Records drained from the Raw Input buffer after it have no message
    /// time and were queued after it, so it bounds their source time too.
    raw_source: Option<QpcTick>,
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
            self.status = PendingStatus::default();
            self.raw_source = None;
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
        // A status record waits for queue room behind the pending ones.
        let record = match self.status.defer(record) {
            Ok(()) => return self.flush_status().map(|_| ()),
            Err(record) => record,
        };
        // Status records were deferred above.
        let value = InputEvidence::try_from(record.clone()).map_err(|_| transport())?;
        if !self.enqueue(value)? { self.losses.drop_record(&record); }
        Ok(())
    }

    fn flush(&mut self) -> std::result::Result<(), NativeError> {
        if self.grant.read_floor(false).is_err() || self.visit != Some(self.grant.visit()) { return Ok(()) }
        for producer in Producer::ALL {
            if !self.flush_loss(producer)? { return Ok(()) }
        }
        self.flush_status().map(|_| ())
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

    fn flush_status(&mut self) -> std::result::Result<bool, NativeError> {
        // The pending records leave `self` while `enqueue` borrows it.
        let mut status = std::mem::take(&mut self.status);
        // Only status records are pending.
        let flushed = status.flush(|record| self.enqueue(InputEvidence::try_from(record).map_err(|_| transport())?));
        self.status = status;
        flushed
    }

    fn flush_loss(&mut self, producer: Producer) -> std::result::Result<bool, NativeError> {
        // The pending gaps leave `self` while `enqueue` borrows it.
        let mut losses = std::mem::take(&mut self.losses);
        let flushed = losses.flush(producer, |gap| self.enqueue(InputEvidence::Gap(gap)));
        self.losses = losses;
        flushed
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
