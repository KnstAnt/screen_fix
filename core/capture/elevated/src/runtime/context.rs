use std::{
    collections::VecDeque,
    num::NonZeroU64,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Release}},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use screen_fix_native_observation::{
    LossSpan, hung, identity::{MetadataScrubber, WindowProcess}, ime,
    uia::{self, Client}, uia_commits::{CommitLossReason, CommitScope, Queue as Commits},
    windows::{FocusSnapshot, WindowMetadata, window_dpi},
};
use screen_fix_protocol::{
    QpcTick, RequestId,
    ipc::{FocusAtReceipt, elevated::{
        CommitEvidence, CommitText, ContextEvidence, FocusEvidence, HungEvidence,
        ImeState, PointBinding, Query, ReadBinding, ReadEvidence, ReadOutcome, ReadUnavailable, TextClassification,
        UiaEvidence, VisitBinding, WindowEvidence, WindowInstance,
        runtime::{ApprovalKey, ContextGap, ContextGapCause, ContextProducer, IdentityClaim, Observation},
    }},
};

use crate::{deadline::CLEANUP, win::{Failure, Result}};
use super::{
    AtomicState, State, grant::Grant, outbox::Outbox, lifetime::{ProviderKind, qpc}, witness::{Foreground, Target},
};

const POLL: Duration = Duration::from_millis(4);
const UIA_BUDGET: Duration = Duration::from_millis(500);
const IME_BUDGET: Duration = Duration::from_millis(50);
const NATIVE_BUDGET: Duration = Duration::from_millis(250);
const MAX_SEEN_WINDOWS: usize = 64;

#[derive(Default)]
pub(super) struct SeenWindows(Mutex<VecDeque<WindowProcess>>);

impl SeenWindows {
    pub(super) fn observe(&self, window: WindowProcess) -> Result<()> {
        let mut seen = self.0.lock().map_err(|_| Failure::Supervisor)?;
        seen.retain(|old| old.window != window.window);
        if seen.len() == MAX_SEEN_WINDOWS { seen.pop_front(); }
        seen.push_back(window);
        Ok(())
    }

    fn find(&self, window: WindowInstance) -> Option<WindowProcess> {
        self.0.lock().ok()?.iter().copied().find(|found| WindowInstance::from(*found) == window)
    }
}

/// Who resolves a pending read: its worker, its delivery after hand-off, or
/// the owner after the request deadline (E2). Exactly one result leaves.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadState {
    Running,
    HandedOff,
    TimedOut,
}

impl State for ReadState {
    const ALL: &'static [Self] = &[Self::Running, Self::HandedOff, Self::TimedOut];

    fn raw(self) -> u8 { self as u8 }
}

/// The one pending read each query kind can have.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Focus,
    Pointer,
    ImeFocus,
    ImeKey,
    Window,
    NativeFocus,
    Hung,
    ClipboardFence,
}

impl Slot {
    const ALL: [Self; 8] = [
        Self::Focus, Self::Pointer, Self::ImeFocus, Self::ImeKey,
        Self::Window, Self::NativeFocus, Self::Hung, Self::ClipboardFence,
    ];
    /// The IME worker's queue holds one job per IME slot.
    const IME: usize = {
        let mut count = 0;
        let mut index = 0;
        while index < Self::ALL.len() {
            if Self::ALL[index].ime() { count += 1; }
            index += 1;
        }
        count
    };

    fn of(query: &Query) -> Option<Self> {
        Some(match query {
            Query::Focus { .. } => Self::Focus,
            Query::Pointer { .. } => Self::Pointer,
            Query::ImeFocus { .. } => Self::ImeFocus,
            Query::ImeKey { .. } => Self::ImeKey,
            Query::Window(_) => Self::Window,
            Query::NativeFocus(_) => Self::NativeFocus,
            Query::Hung(_) => Self::Hung,
            Query::ClipboardFence(_) => Self::ClipboardFence,
            Query::Held(_) => return None,
        })
    }

    const fn ime(self) -> bool { matches!(self, Self::ImeFocus | Self::ImeKey) }

    fn index(self) -> usize { self as usize }
}

#[derive(Clone)]
struct Pending {
    read: ReadBinding,
    cancelled: Arc<AtomicBool>,
    state: Arc<AtomicState<ReadState>>,
}

type PendingReads = Arc<Mutex<[Option<Pending>; Slot::ALL.len()]>>;

#[derive(Default)]
struct Proof {
    target: Option<Arc<Target>>,
    focus: Option<FocusAtReceipt>,
    point: Option<PointBinding>,
    automation: Option<(Arc<AtomicU64>, u64)>,
}

pub(super) enum Delivery {
    Read(ReadDelivery),
    Commit(CommitDelivery),
}

pub(super) struct ReadDelivery {
    pending: Pending,
    slots: PendingReads,
    slot: Slot,
    proof: Proof,
}

pub(super) struct CommitDelivery {
    visit: VisitBinding,
    focus: FocusAtReceipt,
    revision: Arc<AtomicU64>,
    expected_revision: u64,
}

impl Delivery {
    pub(super) fn gate(&self, grant: &Grant, record: &mut Observation) -> Result<()> {
        let Observation::Item { producer, position, evidence } = record else { return Ok(()); };
        match self {
            Self::Read(read) => {
                let now = qpc()?;
                let status = if read.pending.cancelled.load(Acquire) {
                    Some(ReadUnavailable::Stale)
                } else if now >= read.pending.read.deadline_qpc {
                    Some(ReadUnavailable::TimedOut)
                } else if !read.proof.check(grant, read.pending.read.target) {
                    Some(ReadUnavailable::Stale)
                } else { None };
                if let Some(status) = status {
                    let (started_qpc, completed_qpc) = read_times(evidence);
                    *evidence = ContextEvidence::Rejected(ReadEvidence {
                        binding: read.pending.read, started_qpc, completed_qpc,
                        outcome: ReadOutcome::Unavailable(status),
                    });
                }
            }
            Self::Commit(commit) => {
                if commit.visit != grant.visit() || commit.revision.load(Acquire) != commit.expected_revision
                    || grant.native_focus() != Some(commit.focus) || commit.focus.password_style != Some(false)
                {
                    let ContextEvidence::Commit(text) = evidence else { return Err(Failure::Protocol); };
                    *record = Observation::Gap(ContextGap {
                        visit: commit.visit, producer: *producer, cause: ContextGapCause::Stale,
                        first_position: *position, last_position: *position,
                        first_qpc: text.receipt_qpc, last_qpc: text.receipt_qpc,
                    });
                }
            }
        }
        Ok(())
    }
}

impl Drop for ReadDelivery {
    fn drop(&mut self) { clear_slot(&self.slots, self.slot, &self.pending); }
}

impl Proof {
    fn check(&self, grant: &Grant, intended: WindowInstance) -> bool {
        self.target.as_ref().is_none_or(|target| target.recheck(&grant.parent).is_ok())
            && self.focus.is_none_or(|focus| grant.native_focus() == Some(focus))
            && self.point.is_none_or(|point| uia::window_at(point.point).is_some_and(|hit| {
                WindowInstance::from(hit) == intended && uia::window_root(hit).map(WindowInstance::from) == Some(point.root)
            }))
            && self.automation.as_ref().is_none_or(|(revision, expected)| revision.load(Acquire) == *expected)
    }
}

fn clear_slot(slots: &PendingReads, slot: Slot, pending: &Pending) {
    if let Ok(mut slots) = slots.lock() && slots[slot.index()].as_ref().is_some_and(|current| {
        current.read == pending.read && Arc::ptr_eq(&current.cancelled, &pending.cancelled)
    }) {
        slots[slot.index()] = None;
    }
}

fn read_times(evidence: &ContextEvidence) -> (Option<QpcTick>, Option<QpcTick>) {
    match evidence {
        ContextEvidence::Window(read) => (read.started_qpc, read.completed_qpc),
        ContextEvidence::NativeFocus(read) => (read.started_qpc, read.completed_qpc),
        ContextEvidence::HungRead(read) => (read.started_qpc, read.completed_qpc),
        ContextEvidence::FocusUia(read) | ContextEvidence::PointerUia { read, .. } =>
            (read.started_qpc, read.completed_qpc),
        ContextEvidence::ImeFocus(read) | ContextEvidence::ImeKey { read, .. } =>
            (read.started_qpc, read.completed_qpc),
        ContextEvidence::ClipboardFence(read) | ContextEvidence::Rejected(read) =>
            (read.started_qpc, read.completed_qpc),
        _ => (None, None),
    }
}

struct Job {
    query: Query,
    secondary: Option<IdentityClaim>,
    pending: Pending,
    slot: Slot,
}

pub(super) struct Owners {
    uia: SyncSender<Job>,
    ime: SyncSender<Job>,
    pending: PendingReads,
    workers: [JoinHandle<Result<()>>; 2],
    pub(super) seen: Arc<SeenWindows>,
}

impl Owners {
    pub(super) fn start(
        grant: &Arc<Grant>, approved: Arc<[ApprovalKey]>, send: Outbox,
    ) -> Result<Self> {
        let pending = Arc::new(Mutex::new(std::array::from_fn(|_| None)));
        let seen = Arc::new(SeenWindows::default());
        let (uia, uia_in) = mpsc::sync_channel(Slot::ALL.len() - Slot::IME);
        let (ime, ime_in) = mpsc::sync_channel(Slot::IME);
        let common = Worker {
            grant: Arc::clone(grant), approved, send, pending: Arc::clone(&pending), seen: Arc::clone(&seen),
        };
        let uia_service = common.clone();
        let ui_worker = thread::Builder::new().name("elevated-context-uia".into())
            .spawn(move || uia_service.run_uia(uia_in)).map_err(|_| Failure::Supervisor)?;
        let ime_worker = match thread::Builder::new().name("elevated-context-ime".into())
            .spawn(move || common.run_ime(ime_in))
        {
            Ok(worker) => worker,
            Err(_) => {
                grant.abandon(|| { let _ = ui_worker.join(); })?;
                return Err(Failure::Supervisor);
            }
        };
        Ok(Self { uia, ime, pending, workers: [ui_worker, ime_worker], seen })
    }

    pub(super) fn ended(&self) -> bool { self.workers.iter().any(JoinHandle::is_finished) }

    pub(super) fn stop(self) -> Result<()> {
        let [uia, ime] = self.workers;
        let uia = uia.join().map_err(|_| Failure::Supervisor).and_then(|result| result);
        let ime = ime.join().map_err(|_| Failure::Supervisor).and_then(|result| result);
        uia.and(ime)
    }

    pub(super) fn submit(&self, query: Query, secondary: Option<IdentityClaim>) -> Result<()> {
        let read = query.binding();
        let slot = Slot::of(&query).ok_or(Failure::Protocol)?;
        let mut pending = self.pending.lock().map_err(|_| Failure::Supervisor)?;
        // A call still running past its request deadline blocks its provider
        // kind until it returns (E2).
        let blocked = Slot::ALL.iter().zip(pending.iter()).any(|(kind, item)| item.as_ref().is_some_and(|item| {
            kind.ime() == slot.ime() && item.state.load() == ReadState::TimedOut
        }));
        // Request ids strictly increase per visit (`Owner::query`), so an
        // occupied slot is the only conflict.
        if blocked || pending[slot.index()].is_some() {
            return Err(Failure::Busy);
        }
        let current = Pending {
            read, cancelled: Arc::new(AtomicBool::new(false)), state: Arc::new(AtomicState::new(ReadState::Running)),
        };
        pending[slot.index()] = Some(current.clone());
        let lane = if slot.ime() { &self.ime } else { &self.uia };
        if lane.try_send(Job { query, secondary, pending: current, slot }).is_err() {
            // The worker has exited; the owner decides whether the grant
            // was revoked meanwhile.
            pending[slot.index()] = None;
            return Err(Failure::Transport);
        }
        Ok(())
    }

    /// Resolves every read still unresolved at its request deadline as timed
    /// out (E2). The worker discards its late return, and the slot stays
    /// occupied until the call actually returns.
    pub(super) fn expire(&self, now: QpcTick) -> Result<Vec<ReadBinding>> {
        let pending = self.pending.lock().map_err(|_| Failure::Supervisor)?;
        Ok(pending.iter().flatten().filter(|item| {
            item.read.deadline_qpc <= now && item.state.transition(ReadState::Running, ReadState::TimedOut)
        }).map(|item| {
            item.cancelled.store(true, Release);
            item.read
        }).collect())
    }

    pub(super) fn cancel(&self, visit: VisitBinding, request: RequestId) -> Result<()> {
        let pending = self.pending.lock().map_err(|_| Failure::Supervisor)?;
        if let Some(found) = pending.iter().flatten().find(|item| item.read.visit == visit && item.read.request == request) {
            found.cancelled.store(true, Release);
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    grant: Arc<Grant>,
    approved: Arc<[ApprovalKey]>,
    send: Outbox,
    pending: PendingReads,
    seen: Arc<SeenWindows>,
}

impl Worker {
    fn run_uia(&self, queue: Receiver<Job>) -> Result<()> {
        let mut uia = Uia::new();
        let scrubber = MetadataScrubber::new();
        let result = (|| {
            while !self.grant.stopped() {
                self.commits(&mut uia)?;
                let Ok(job) = queue.recv_timeout(POLL) else { continue };
                let processed = self.read(&job, &mut uia, &scrubber);
                self.complete(&job)?;
                processed?;
            }
            Ok(())
        })();
        uia.desired.store(0, Release);
        if let Some(client) = uia.client.take() {
            let _guard = self.grant.lifetime.teardown(qpc()?)?;
            drop(client);
        }
        result
    }

    fn run_ime(&self, queue: Receiver<Job>) -> Result<()> {
        while !self.grant.stopped() {
            let Ok(job) = queue.recv_timeout(POLL) else { continue };
            let result = self.ime(&job);
            self.complete(&job)?;
            result?;
        }
        Ok(())
    }

    fn complete(&self, job: &Job) -> Result<()> {
        if job.pending.state.load() != ReadState::HandedOff {
            clear_slot(&self.pending, job.slot, &job.pending);
        }
        Ok(())
    }

    fn live(&self, pending: &Pending) -> bool {
        !pending.cancelled.load(Acquire) && pending.read.visit == self.grant.visit()
            && self.grant.read_open(false).is_ok_and(|now| now < pending.read.deadline_qpc)
    }

    fn read(&self, job: &Job, uia: &mut Uia, scrubber: &MetadataScrubber) -> Result<()> {
        if job.pending.cancelled.load(Acquire) || job.pending.read.visit != self.grant.visit()
            || self.grant.read_floor(false).is_err() { return Ok(()) }
        match job.query {
            Query::Focus { expected, .. } => self.uia(job, uia, scrubber, Some(expected)),
            Query::Pointer { .. } => self.uia(job, uia, scrubber, None),
            Query::NativeFocus(_)
            | Query::Window(_)
            | Query::Hung(_)
            | Query::ClipboardFence(_) => self.native(job, uia, scrubber),
            _ => Err(Failure::Protocol),
        }
    }

    fn uia(&self, job: &Job, uia: &mut Uia, scrubber: &MetadataScrubber, expected: Option<FocusAtReceipt>) -> Result<()> {
        let read = job.pending.read;
        let now = qpc()?;
        let build = |evidence: ReadEvidence<UiaEvidence>| match job.query {
            Query::Pointer { point, .. } => ContextEvidence::PointerUia { point, read: evidence },
            _ => ContextEvidence::FocusUia(evidence),
        };
        let send_error = |reason| self.publish(job, build(unavailable(read, now, reason)));
        if !self.live(&job.pending) {
            return send_error(ReadUnavailable::TimedOut);
        }
        let (window, secondary, focus, query) = match self.uia_query(job, expected) {
            Ok(bound) => bound,
            // An unapproved or unverifiable element is outside the approval.
            Err(Failure::TargetAccess | Failure::TokenIdentity(_) | Failure::ProcessIdentity) =>
                return send_error(ReadUnavailable::OutsideApproval),
            Err(Failure::TargetChanged | Failure::ScopeClosed | Failure::LeaseExpired | Failure::Cancelled
                | Failure::PeerExited) => return send_error(ReadUnavailable::Stale),
            Err(error) => return Err(error),
        };
        let Some(limited) = self.deadline(read, UIA_BUDGET)? else {
            return send_error(ReadUnavailable::TimedOut);
        };
        let Some(_call) = self.call(ProviderKind::Read, limited.0) else {
            return send_error(ReadUnavailable::TimedOut);
        };
        if expected.is_some() {
            // A new focus read replaces the subscription; its pending commits
            // and losses are delivered or counted first (E3).
            self.commits(uia)?;
        }
        let expected_revision = uia.revision.load(Acquire);
        let invalid = |_: uia::Request| {
            let current = self.live(&job.pending) && uia.revision.load(Acquire) == expected_revision
                && secondary.as_ref().is_none_or(|target| target.recheck(&self.grant.parent).is_ok())
                && expected.is_none_or(|expected| FocusSnapshot::read(window).is_some_and(|now| now.focus == expected));
            (!current).then_some(ReadUnavailable::Stale)
        };
        if uia.client.is_none() {
            uia.client = Client::open_scoped(Arc::clone(&uia.revision)).ok();
        }
        let Some(client) = uia.client.as_mut() else { return send_error(ReadUnavailable::Unavailable) };
        let native = uia::Request {
            id: read.request, generation: read.visit.focus_generation, focus_revision: expected_revision,
            expires: limited.1, query,
        };
        if invalid(native).is_some() { return send_error(ReadUnavailable::Stale); }
        if expected.is_some() {
            uia.desired.store(0, Release);
            client.clear_subscription();
            uia.subscription = None;
            uia.editable = None;
        }
        let result = client.read(native, scrubber, &invalid);
        let (dpi, dpi_awareness) = if invalid(native).is_none() && result.element.is_some() {
            window_dpi(window)
        } else { (None, None) };
        let mut editable = false;
        let outcome = if result.completed.is_none_or(|end| end >= read.deadline_qpc) {
            ReadOutcome::Unavailable(ReadUnavailable::TimedOut)
        } else if invalid(native).is_some() {
            ReadOutcome::Unavailable(ReadUnavailable::Stale)
        } else if let Some(element) = result.element {
            editable = result.target == TextClassification::Editable
                && focus.is_some_and(|focus| focus.focus.password_style == Some(false));
            ReadOutcome::Observed(UiaEvidence { target: result.target, dpi, dpi_awareness,
                element: element.try_into().map_err(|_| Failure::Protocol)? })
        } else {
            // A completed read without an element is reported as unavailable.
            ReadOutcome::Unavailable(result.status.err().unwrap_or(ReadUnavailable::Unavailable))
        };
        let completed = result.completed;
        let returned = self.publish_item(job, build(ReadEvidence {
            binding: read, started_qpc: result.started, completed_qpc: completed, outcome,
        }), Proof {
            target: secondary.clone(), focus: expected,
            point: match job.query { Query::Pointer { point, .. } => Some(point), _ => None },
            automation: Some((Arc::clone(&uia.revision), expected_revision)),
        })?;
        if editable && returned && let Some(focus) = focus {
            uia.editable = completed.map(|_| (read.visit, focus.focus, expected_revision));
            uia.desired.store(read.visit.focus_generation.0, Release);
            let scope = self.commit_scope(window, focus.focus, read.visit, Arc::clone(&uia.revision), expected_revision);
            client.finish_scoped_read(true, &invalid, &uia.desired, &uia.revision, &uia.commits, Some(scope));
            uia.subscription = Some((read, focus.focus, expected_revision));
        }
        Ok(())
    }

    fn uia_query(
        &self, job: &Job, expected: Option<FocusAtReceipt>,
    ) -> Result<(WindowProcess, Option<Arc<Target>>, Option<FocusSnapshot>, uia::Query)> {
        let read = job.pending.read;
        if let Query::Pointer { point, .. } = job.query {
            if point.action_qpc.is_some_and(|action| {
                    self.grant.read_floor(false).map_or(true, |floor| action <= floor) || action > read.requested_qpc
                }) {
                return Err(Failure::TargetChanged);
            }
            let window = uia::window_at(point.point).ok_or(Failure::TargetChanged)?;
            let root = uia::window_root(window).filter(|root| WindowInstance::from(*root) == point.root)
                .ok_or(Failure::TargetChanged)?;
            let secondary = self.resolve(window, job)?;
            return Ok((window, secondary, None, uia::Query::Pointer { point: point.point, window, root }));
        }
        let window = self.foreground();
        if read.target != WindowInstance::from(window) || job.secondary.is_some() {
            return Err(Failure::TargetChanged);
        }
        let focus = FocusSnapshot::read(window).filter(|read| Some(read.focus) == expected)
            .ok_or(Failure::TargetChanged)?;
        Ok((window, None, Some(focus), uia::Query::Focus { foreground: window, native: Some(focus) }))
    }

    fn native(&self, job: &Job, uia: &mut Uia, scrubber: &MetadataScrubber) -> Result<()> {
        let read = job.pending.read;
        let start = qpc()?;
        let limited = self.deadline(read, NATIVE_BUDGET).ok().flatten();
        let _call = limited.and_then(|(until, _)| self.call(ProviderKind::Read, until));
        let window = self.foreground();
        let live = self.live(&job.pending) && _call.is_some();
        let details = if live && read.target == WindowInstance::from(window) && job.secondary.is_none() {
            Some((window, None))
        } else if live && matches!(job.query, Query::Window(_) | Query::Hung(_)) {
            self.seen.find(read.target).and_then(|local| self.resolve(local, job).ok().map(|target| (local, target)))
        } else { None };
        let post = || self.live(&job.pending) && details.as_ref().is_some_and(|(local, target)| {
            WindowProcess::read(local.hwnd()) == Some(*local)
                && target.as_ref().is_none_or(|target| target.recheck(&self.grant.parent).is_ok())
        });
        let outcome = match job.query {
            Query::NativeFocus(_) => {
                let snapshot = details.as_ref().filter(|_| post()).and_then(|(window, _)| FocusSnapshot::read(*window));
                ContextEvidence::NativeFocus(observed(read, start, snapshot.filter(|snapshot| post()
                    && self.grant.native_focus() == Some(snapshot.focus)).map(FocusEvidence::from))?)
            }
            Query::Window(_) => {
                let metadata = details.as_ref().filter(|_| post()).and_then(|(window, _)| WindowMetadata::read(*window, scrubber));
                let value = metadata.filter(|_| post()).and_then(|value| WindowEvidence::try_from(value).ok());
                ContextEvidence::Window(observed(read, start, value)?)
            }
            Query::Hung(_) => {
                let info = details.as_ref().filter(|_| post()).map(|(window, _)| HungEvidence {
                    window: (*window).into(), direct: hung::is_hung(window.hwnd().0), ghost: hung::is_ghost(window.hwnd().0),
                });
                ContextEvidence::HungRead(observed(read, start, info.filter(|_| post()))?)
            }
            Query::ClipboardFence(_) => {
                let editable = limited.is_some_and(|(_, expires)| {
                    post() && self.editable_fence(job, uia, window, expires) && post()
                });
                ContextEvidence::ClipboardFence(observed(read, start, editable.then_some(()))?)
            }
            _ => return Err(Failure::Protocol),
        };
        let clipboard = match job.query {
            Query::ClipboardFence(_) => uia.editable.map(|(_, focus, revision)| (focus, revision)),
            _ => None,
        };
        let proof = Proof {
            target: details.and_then(|(_, target)| target),
            focus: match &outcome {
                ContextEvidence::NativeFocus(evidence) => match &evidence.outcome {
                    ReadOutcome::Observed(snapshot) => Some(snapshot.focus),
                    ReadOutcome::Unavailable(_) => None,
                },
                _ => clipboard.map(|(focus, _)| focus),
            },
            automation: clipboard.map(|(_, revision)| (Arc::clone(&uia.revision), revision)),
            point: None,
        };
        self.publish_item(job, outcome, proof).map(|_| ())
    }

    fn editable_fence(&self, job: &Job, uia: &mut Uia, window: WindowProcess, expires: Instant) -> bool {
        let read = job.pending.read;
        let Some((visit, expected, revision)) = uia.editable else { return false; };
        if visit != read.visit || !self.safe_focus(window, expected) { return false; }
        let Some(focus) = FocusSnapshot::read(window).filter(|focus| focus.focus == expected) else { return false; };
        let current_revision = &uia.revision;
        let invalid = |_: uia::Request| {
            (!self.live(&job.pending) || current_revision.load(Acquire) != revision || !self.safe_focus(window, expected))
                .then_some(ReadUnavailable::Stale)
        };
        let request = uia::Request { id: read.request, generation: visit.focus_generation, focus_revision: revision,
            expires, query: uia::Query::Focus { foreground: window, native: Some(focus) } };
        uia.client.as_mut().is_some_and(|client| client.editable_focus(request, &invalid)) && invalid(request).is_none()
    }

    fn ime(&self, job: &Job) -> Result<()> {
        let (expected, key) = match job.query {
            Query::ImeFocus { expected, .. } => (expected, None),
            Query::ImeKey { expected, key, key_qpc, .. } => (expected, Some((key, key_qpc))),
            _ => return Err(Failure::Protocol),
        };
        let mut value = None;
        let start = qpc()?;
        let read = job.pending.read;
        if self.live(&job.pending) && read.target == self.grant.visit().foreground && job.secondary.is_none()
            && key.is_none_or(|(_, key_qpc)| self.grant.read_floor(false).is_ok_and(|floor| key_qpc > floor))
        {
            let foreground = self.foreground();
            if let Some(focus) = FocusSnapshot::read(foreground).filter(|focus| focus.focus == expected)
                && let Ok(Some((until, expires))) = self.deadline(read, IME_BUDGET)
                && let Some(_call) = self.call(ProviderKind::Ime, until)
            {
                let state = match ime::has_ime(Some(focus)) {
                    Some(true) => ime::read_scoped(foreground, Some(focus), expires, || self.live(&job.pending)),
                    Some(false) => ImeState::NotIme,
                    None => ImeState::Unknown,
                };
                if self.live(&job.pending) { value = Some(state); }
            }
        }
        let read = observed(read, start, value)?;
        self.publish_item(job, match key {
            Some((key, key_qpc)) => ContextEvidence::ImeKey { key, key_qpc, read },
            None => ContextEvidence::ImeFocus(read),
        }, Proof { focus: Some(expected), ..Proof::default() }).map(|_| ())
    }

    fn commits(&self, uia: &mut Uia) -> Result<()> {
        let Some((read, focus, revision)) = uia.subscription else { return Ok(()) };
        let now = qpc()?;
        // Commit losses reach the Recorder per reason (E3).
        let lose = |cause, span: LossSpan| match NonZeroU64::new(span.count) {
            Some(count) => self.send.lost(&self.grant, read.visit, ContextProducer::TextEdit, cause, span.first, span.last, count),
            None => Ok(()),
        };
        for loss in uia.commits.take_loss() {
            lose(match loss.reason {
                CommitLossReason::Capacity => ContextGapCause::Overflow,
                CommitLossReason::TooLong => ContextGapCause::CommitTooLong,
                CommitLossReason::Parts => ContextGapCause::CommitParts,
                CommitLossReason::Rejected => ContextGapCause::CommitRejected,
                CommitLossReason::Stale => ContextGapCause::Stale,
            }, loss.span)?;
        }
        if !self.commit_current(read.visit, focus, &uia.revision, revision) {
            uia.desired.store(0, Release);
            uia.subscription = None;
            uia.editable = None;
            for commit in uia.commits.take() {
                lose(ContextGapCause::Stale, LossSpan::at(commit.receipt))?;
            }
            if let Some(client) = uia.client.as_mut() {
                let _call = self.grant.lifetime.teardown(now)?;
                client.clear_subscription();
            }
            return Ok(());
        }
        for commit in uia.commits.take() {
            if commit.generation != read.visit.focus_generation || commit.revision != revision
                || !self.commit_current(read.visit, focus, &uia.revision, revision)
            {
                lose(ContextGapCause::Stale, LossSpan::at(commit.receipt))?;
                continue;
            }
            let Ok(text) = CommitText::new(&commit.text) else {
                lose(ContextGapCause::CommitTooLong, LossSpan::at(commit.receipt))?;
                continue;
            };
            self.send.publish_checked(&self.grant, read.visit, ContextEvidence::Commit(CommitEvidence {
                binding: read, receipt_qpc: commit.receipt, text, valid: commit.valid,
            }), false, Some(Delivery::Commit(CommitDelivery {
                visit: read.visit, focus, revision: Arc::clone(&uia.revision), expected_revision: revision,
            })))?;
        }
        Ok(())
    }

    fn commit_scope(
        &self, foreground: WindowProcess, focus: FocusAtReceipt, visit: VisitBinding,
        revision: Arc<AtomicU64>, expected: u64,
    ) -> CommitScope {
        let proof = self.clone();
        let sender_proof = self.clone();
        let sender_revision = Arc::clone(&revision);
        CommitScope {
            check_sender: Arc::new(move |sender, receipt| {
                let Ok(deadline) = sender_proof.grant.lifetime.after(receipt, NATIVE_BUDGET) else { return false };
                let Some(_call) = sender_proof.grant.lifetime.provider(ProviderKind::Callback, deadline) else { return false };
                sender_proof.commit_current(visit, focus, &sender_revision, expected)
                    && uia::commit_sender(sender, foreground)
                    && sender_proof.commit_current(visit, focus, &sender_revision, expected)
                    // A late return can fall between watchdog samples.
                    && qpc().is_ok_and(|now| now < deadline)
            }),
            is_current: Arc::new(move || proof.callback_current(visit, &revision, expected)),
        }
    }

    fn callback_current(&self, visit: VisitBinding, revision: &AtomicU64, expected: u64) -> bool {
        revision.load(Acquire) == expected && self.grant.hook_open() && !self.grant.stopped()
            && self.grant.visit() == visit
    }

    fn commit_current(&self, visit: VisitBinding, focus: FocusAtReceipt, revision: &AtomicU64, expected: u64) -> bool {
        visit == self.grant.visit() && revision.load(Acquire) == expected
            && self.grant.read_open(false).is_ok() && self.safe_focus(self.foreground(), focus)
    }

    fn safe_focus(&self, foreground: WindowProcess, focus: FocusAtReceipt) -> bool {
        FocusSnapshot::read(foreground).is_some_and(|current| {
            !current.hung && current.focus == focus && current.focus.password_style == Some(false)
        })
    }

    fn resolve(&self, local: WindowProcess, job: &Job) -> Result<Option<Arc<Target>>> {
        if job.pending.read.target != WindowInstance::from(local) {
            return Err(Failure::TargetChanged);
        }
        if local == self.foreground() && job.secondary.is_none() {
            return Ok(None);
        }
        let claim = job.secondary.as_ref().ok_or(Failure::TargetChanged)?;
        let actual = Foreground::of(local.hwnd().0).ok_or(Failure::TargetChanged)?;
        Ok(Some(Arc::new(self.grant.prove_other(actual, claim, &self.approved)?)))
    }

    fn foreground(&self) -> WindowProcess {
        self.grant.visit().foreground.into()
    }

    /// Registers a provider call with the supervisor. Its slot deadline is
    /// the request deadline plus the cleanup bound (E2): a call returning in
    /// between is stale, one still running at the slot deadline defeats
    /// cancellation and the supervisor terminates the collector.
    fn call(&self, kind: ProviderKind, request: QpcTick) -> Option<super::lifetime::Provider<'_>> {
        let slot = self.grant.lifetime.after(request, CLEANUP).ok()?;
        self.grant.lifetime.provider(kind, slot)
    }

    /// The provider call deadline: the request deadline capped by `cap`, as
    /// a QPC tick and an instant; `None` when no time remains.
    fn deadline(&self, read: ReadBinding, cap: Duration) -> Result<Option<(QpcTick, Instant)>> {
        let start = qpc()?;
        let capped = read.deadline_qpc.min(self.grant.lifetime.after(start, cap)?);
        let Some(remaining) = capped.0.checked_sub(start.0).filter(|value| *value > 0) else { return Ok(None) };
        Ok(Some((capped, self.grant.lifetime.instant_of(remaining)?)))
    }

    fn publish(&self, job: &Job, result: ContextEvidence) -> Result<()> {
        self.publish_item(job, result, Proof::default()).map(|_| ())
    }

    fn publish_item(&self, job: &Job, result: ContextEvidence, proof: Proof) -> Result<bool> {
        if job.pending.cancelled.load(Acquire) || job.pending.read.visit != self.grant.visit()
            || !job.pending.state.transition(ReadState::Running, ReadState::HandedOff)
        {
            return Ok(false);
        }
        // A call returning past its request deadline is stale (E2).
        let now = qpc()?;
        let result = if now >= job.pending.read.deadline_qpc {
            ContextEvidence::Rejected(unavailable(job.pending.read, now, ReadUnavailable::TimedOut))
        } else { result };
        let delivery = Delivery::Read(ReadDelivery {
            pending: job.pending.clone(), slots: Arc::clone(&self.pending), slot: job.slot, proof,
        });
        self.send.publish_checked(&self.grant, job.pending.read.visit, result, false, Some(delivery))
    }
}

struct Uia {
    client: Option<Client>,
    revision: Arc<AtomicU64>,
    desired: Arc<AtomicU64>,
    commits: Arc<Commits>,
    subscription: Option<(ReadBinding, FocusAtReceipt, u64)>,
    editable: Option<(VisitBinding, FocusAtReceipt, u64)>,
}

impl Uia {
    fn new() -> Self {
        Self {
            client: None, revision: Arc::new(AtomicU64::new(1)), desired: Arc::new(AtomicU64::new(0)),
            commits: Arc::new(Commits::new()), subscription: None, editable: None,
        }
    }
}

fn unavailable<T>(read: ReadBinding, now: QpcTick, status: ReadUnavailable) -> ReadEvidence<T> {
    ReadEvidence { binding: read, started_qpc: None, completed_qpc: Some(now), outcome: ReadOutcome::Unavailable(status) }
}

fn observed<T>(read: ReadBinding, start: QpcTick, value: Option<T>) -> Result<ReadEvidence<T>> {
    let end = qpc()?;
    let outcome = if end >= read.deadline_qpc {
        ReadOutcome::Unavailable(ReadUnavailable::TimedOut)
    } else {
        value.map_or(ReadOutcome::Unavailable(ReadUnavailable::Unavailable), ReadOutcome::Observed)
    };
    Ok(ReadEvidence { binding: read, started_qpc: Some(start), completed_qpc: Some(end), outcome })
}
