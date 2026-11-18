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
    input::{self, Collector, HookForeground, InputScope, NativeError, Settings, loss::{Backlog, Sent}},
};
use screen_fix_protocol::{
    QpcFrequency, QpcTick,
    ipc::{
        HelperFailure, HelperFailureStage, HelperToCore, InputRegistration, Producer,
        elevated::{GrantedInput, HeldSnapshot, InputEvidence, VisitBinding, WindowInstance, runtime::CoverageReason},
    },
};

use crate::win::{Failure, Result};

use super::{grant::{Grant, InputLoss}, timing::INPUT_HOLDBACK, witness::{HeldAccess, coarse_lower}};

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
            let scope: Arc<dyn InputScope> = Arc::new(Scope { foreground: grant.visit().foreground, grant: Arc::clone(&grant) });
            let owner = unsafe { BorrowedHandle::borrow_raw(grant.session().peer.raw()) };
            let wake = unsafe { BorrowedHandle::borrow_raw(grant.stop_event().raw()) };
            let mut output = Output {
                sink: Sink { grant: Arc::clone(&grant), send, visit: None }, ready, backlog: Backlog::default(),
                raw_source: None,
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

/// The grant's scope as the native input producers see it.
struct Scope {
    grant: Arc<Grant>,
    /// The visit's foreground when the producers started.
    foreground: WindowInstance,
}

impl InputScope for Scope {
    fn foreground(&self) -> WindowInstance { self.foreground }

    fn hook_open(&self) -> Option<u64> { self.grant.hook_stamp() }

    fn raw_open(&self) -> bool { self.grant.read_open(false).is_ok() }

    fn serial(&self) -> u64 { self.grant.scope_serial() }
}

pub(super) fn held(grant: &Grant) -> Result<(QpcTick, QpcTick, Option<HeldSnapshot>)> {
    let start = grant.check(true)?;
    let access = HeldAccess::open(grant.target().window.thread)?;
    let state = held::read();
    if !access.current() { return Err(Failure::TargetAccess); }
    let end = grant.check(true)?;
    if end < start { return Err(Failure::Supervisor); }
    Ok((start, end, state))
}

struct Output {
    sink: Sink,
    ready: SyncSender<InputRegistration>,
    /// Pending gaps and status records waiting for queue room (E3).
    backlog: Backlog,
    /// Source lower bound of the latest Raw Input record with a message time.
    /// Records drained from the Raw Input buffer after it have no message
    /// time and were queued after it, so it bounds their source time too.
    raw_source: Option<QpcTick>,
}

/// The egress queue of the current binding. It is apart from the backlog,
/// which sends through it.
struct Sink {
    grant: Arc<Grant>,
    send: SyncSender<Item>,
    visit: Option<VisitBinding>,
}

fn transport() -> NativeError { NativeError::without_code(HelperFailureStage::Transport) }

impl Collector for Output {
    fn stop_requested(&self) -> bool { self.sink.grant.stopped() }
    fn take_environment(&mut self) -> u32 { 0 }

    fn ready(&mut self, registration: InputRegistration) -> std::result::Result<(), NativeError> {
        self.ready.try_send(registration).map_err(|_| transport())
    }

    fn offer_hook(&mut self, record: HelperToCore, proof: Option<HookForeground>) -> std::result::Result<(), NativeError> {
        if matches!(&record, HelperToCore::Input(_)) {
            let grant = &self.sink.grant;
            let visit = grant.visit();
            let serial = grant.scope_serial();
            if proof.is_some_and(|proof| proof.serial != serial) { return Ok(()); }
            let target = visit.foreground;
            if proof != Some(HookForeground { window: target.window, pid: target.process.pid.get(), thread: target.thread.get(), serial }) {
                grant.mark_uncertain(visit, grant.armed_qpc());
                return Ok(());
            }
        }
        self.offer(record)
    }

    fn offer(&mut self, record: HelperToCore) -> std::result::Result<(), NativeError> {
        let grant = &self.sink.grant;
        let visit = grant.visit();
        let floor = grant.read_floor(false).ok().filter(|_| grant.visit() == visit);
        if self.sink.visit != Some(visit) {
            // The old binding's evidence is closed. Its pending gaps are kept
            // and delivered first in the new binding, where gaps before the
            // resync sample only count (E3).
            self.backlog.discard_status();
            self.raw_source = None;
            self.sink.visit = Some(visit);
        }
        // A native gap is never dropped: outside the open scope it waits as a
        // pending loss (E3).
        let Some(floor) = floor else {
            if matches!(record, HelperToCore::Gap(_)) { self.backlog.drop_record(&record); }
            return Ok(());
        };
        if !self.eligible(&record, visit, floor) {
            if matches!(record, HelperToCore::Gap(_)) { self.backlog.drop_record(&record); }
            return Ok(());
        }
        // A status record waits for queue room behind the pending ones; any
        // other record follows its producer's pending gaps.
        let sink = &self.sink;
        self.backlog.offer(record, |record| sink.offer(record))
    }

    fn flush(&mut self) -> std::result::Result<(), NativeError> {
        let sink = &self.sink;
        if sink.grant.read_floor(false).is_err() || sink.visit != Some(sink.grant.visit()) { return Ok(()) }
        self.backlog.flush(Producer::ALL, |record| sink.offer(record)).map(drop)
    }

    fn finish(&mut self, _: BorrowedHandle<'_>) -> std::result::Result<(), NativeError> { Ok(()) }
}

impl Output {
    fn eligible(&mut self, record: &HelperToCore, visit: VisitBinding, floor: QpcTick) -> bool {
        let grant = &self.sink.grant;
        match record {
            HelperToCore::Input(input) => {
                let horizon = floor.0.saturating_add(grant.session().lifetime.ticks(INPUT_HOLDBACK).unwrap_or(0));
                if input.receipt.qpc.0 <= horizon { return false; }
                let raw = input.sample.producer().raw_input();
                let source = match input.receipt.message_tick_ms {
                    Some(tick) => {
                        let source = coarse_lower(tick.0, &grant.session().lifetime);
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
                                && foreground.focus == grant.expected_focus(visit)
                        }),
                        _ => true,
                    };
                if !in_scope {
                    // No details or event counts from ambiguous input leave this process.
                    grant.input_loss(visit, InputLoss {
                        before: source.unwrap_or(grant.armed_qpc()).min(input.receipt.qpc),
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
}

impl Sink {
    /// Queues `record`, or hands it back when the queue has no room.
    fn offer(&self, record: HelperToCore) -> std::result::Result<Sent, NativeError> {
        let evidence = InputEvidence::try_from(record.clone()).map_err(|_| transport())?;
        Ok(if self.enqueue(evidence)? { Sent::Accepted } else { Sent::Full(record) })
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
