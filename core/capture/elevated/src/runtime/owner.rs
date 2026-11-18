use std::{
    num::NonZeroU64,
    path::Path,
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering::{Acquire, Release}}, mpsc::{self, Receiver as Queue, SyncSender, TryRecvError}},
    thread::{self, JoinHandle},
    time::Instant,
};

use screen_fix_protocol::{
    AdmissionEpoch, QpcTick, RequestId,
    ipc::{
        EnvironmentChange,
        elevated::{ContextEvidence, Query, ReadBinding, ReadEvidence, ReadOutcome, ReadUnavailable,
            runtime::{Activation, ApprovalKey, ContextCommand, Control, CoverageLoss, CoverageReason, EnvironmentRevision,
                GrantRequest, HeldReadiness, Lease, MAX_APPROVED_EXECUTABLES, Revocation, Status, Transition},
            VisitBinding},
    },
};

use crate::{deadline::{CLEANUP, Cancellation, Deadline, READINESS}, process::Process, pipe::Pipe, win::{Failure, Result}};
use super::{
    Connection, Links, Receiver, Sender, context, notices, outbox::Outbox,
    egress::{context_egress, input_egress},
    grant::{Cut, Grant, Reopened, Session, await_witness},
    input::{self, Native},
    lifetime::{Lifetime, qpc},
    timing::{INPUT_HOLDBACK, LEASE, LONG_WAIT, POLL},
    witness::Witness,
};

const COMMANDS: usize = 8;

struct Configuration {
    approved: Box<[ApprovalKey]>,
    pen_hid: bool,
}

struct Visit {
    grant: Arc<Grant>,
    native: Native,
    context: context::Owners,
    notices: notices::Owner,
    registered: bool,
    held_result: Option<HeldResult>,
    last_request: u64,
    readiness: Instant,
    resync: Option<Resync>,
}

/// A resync in progress: the first QPC its coverage report must include,
/// and the instant by which an expiring resync must be accepted.
#[derive(Clone, Copy)]
struct Resync {
    from: QpcTick,
    expires: Option<Instant>,
}

impl Resync {
    /// A resync that must be accepted within the readiness bound.
    fn bounded(from: QpcTick) -> Self {
        Self { from, expires: Some(readiness()) }
    }
}

struct HeldResult {
    read: ReadBinding,
    completed: QpcTick,
    readiness: HeldReadiness,
    accepted: bool,
}

/// Whether the owner loop goes on after a control command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stopped,
}

struct Owner<'a> {
    session: Arc<Session>,
    connection: &'a Arc<Connection>,
    pen_hid: bool,
    current: Option<Visit>,
    lease: Option<Lease>,
    grant_sequence: u64,
    epoch: AdmissionEpoch,
    source_generation: u64,
    environment: Option<NonZeroU64>,
    cleanup_limit: Option<QpcTick>,
    revisions: (u64, u64),
    status: Sender<Status>,
    observation: Outbox,
    input: SyncSender<input::Item>,
    urgent: Arc<Urgent>,
}

/// The state the control reader shares with the owner to apply a due Stop,
/// Revoke or Environment change ahead of the command queue.
#[derive(Default)]
struct Urgent {
    grant: Mutex<Option<Arc<Grant>>>,
    stopping: AtomicBool,
}

impl Urgent {
    fn preempt(&self, command: &Control, lifetime: &Lifetime) -> Result<()> {
        if !matches!(command, Control::Stop { .. } | Control::Revoke { .. } | Control::Environment(_)) {
            return Ok(());
        }
        let relevant = self.grant.lock().map_err(|_| Failure::Supervisor)?;
        let now = qpc()?;
        if !urgent_due(command, now) { return Ok(()); }
        match command {
            Control::Stop { cleanup_deadline_qpc, .. } => {
                self.stopping.store(true, Release);
                if let Some(grant) = relevant.as_ref() { grant.revoke(); }
                lifetime.begin_cleanup(now, *cleanup_deadline_qpc)?;
            }
            Control::Revoke { visit, .. } => {
                if let Some(grant) = relevant.as_ref().filter(|grant| grant.visit() == *visit) {
                    grant.revoke();
                    lifetime.enter_cleanup(now)?;
                }
            }
            Control::Environment(revision) => {
                if revision.change == EnvironmentChange::SessionEnding {
                    self.stopping.store(true, Release);
                }
                if let Some(grant) = relevant.as_ref() {
                    grant.revoke();
                    lifetime.begin_cleanup(now, revision.cleanup_deadline_qpc)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// A Stop, Revoke or Environment change whose effective QPC has passed. The
/// owner additionally requires that QPC to be positive; the urgent path does
/// not.
fn urgent_due(command: &Control, now: QpcTick) -> bool {
    match command {
        Control::Stop { before_qpc, .. } | Control::Revoke { before_qpc, .. } => *before_qpc <= now,
        Control::Environment(revision) => revision.changed_qpc <= now,
        _ => false,
    }
}

/// The workers of a visit that is still starting. Unless disarmed, dropping
/// it revokes the grant and joins the started workers, notices first, under
/// the cleanup deadline.
struct VisitStart {
    grant: Arc<Grant>,
    /// `None` once disarmed.
    context: Option<context::Owners>,
    notices: Option<notices::Owner>,
}

impl VisitStart {
    /// Hands the started workers over; the grant is no longer abandoned.
    fn disarm(mut self) -> Result<(context::Owners, notices::Owner)> {
        self.context.take().zip(self.notices.take()).ok_or(Failure::Supervisor)
    }
}

impl Drop for VisitStart {
    fn drop(&mut self) {
        let Some(context) = self.context.take() else { return };
        let notices = self.notices.take();
        // The startup error is reported; a cleanup deadline that cannot be
        // set fails the collector all the same.
        let _ = self.grant.abandon(|| {
            if let Some(notices) = notices { let _ = notices.join(); }
            let _ = context.stop();
        });
    }
}

pub(crate) fn run(links: Links, lifetime: &Arc<Lifetime>, startup: Instant) -> Result<()> {
    let cancel = Arc::clone(&links.connection.stop);
    let witness = Witness::start(links.connection.peer.session()?, &cancel, Arc::clone(lifetime))?;
    let result = run_with_witness(links, lifetime, startup, &witness);
    lifetime.stop();
    cancel.cancel();
    result.and(witness.join())
}

fn run_with_witness(links: Links, lifetime: &Arc<Lifetime>, startup: Instant, witness: &Witness) -> Result<()> {
    // Configuration is bounded by readiness; afterwards a silent control lane
    // means no renewal arrived within the lease.
    let mut control = Receiver::<Control>::new(&links.control, &links.connection, Failure::ReadinessDeadline);
    let mut status = Sender::<Status>::new(&links.control, &links.connection, Failure::Transport);
    let Configuration { approved, pen_hid } = configure(&mut control, startup)?;
    control.0.expired = Failure::LeaseExpired;
    await_witness(&witness.view, &links.connection.stop, startup)?;
    lifetime.witness.set(Arc::clone(&witness.progress)).map_err(|_| Failure::Supervisor)?;
    let session = Arc::new(Session {
        peer: Arc::clone(&links.connection.peer), witness: witness.view.clone(), lifetime: Arc::clone(lifetime),
        stop: Arc::clone(&links.connection.stop), approved,
    });

    let mut workers = Workers { stop: Arc::clone(&links.connection.stop), owners: Vec::with_capacity(5) };
    let urgent = Arc::new(Urgent::default());
    let urgent_reader = Arc::clone(&urgent);
    let reader_lifetime = Arc::clone(lifetime);
    let (commands, incoming) = mpsc::sync_channel(COMMANDS);
    workers.spawn("elevated-control-in", move |cancel| {
        control_reader(cancel, control, commands, &urgent_reader, &reader_lifetime)
    })?;

    let context = Receiver::<ContextCommand>::new(&links.context, &links.connection, Failure::Transport);
    let (queries, query_owner) = mpsc::sync_channel(COMMANDS);
    workers.spawn("elevated-context-in", move |cancel| context_reader(cancel, context, queries))?;
    let input_pipe = Arc::clone(&links.input);
    let peer = Arc::clone(&links.connection.peer);
    workers.spawn("elevated-input-monitor", move |cancel| input_monitor(cancel, &input_pipe, &peer))?;

    let (input, outbound) = mpsc::sync_channel::<input::Item>(input::SLOTS);
    let writer = Sender::new(&links.input, &links.connection, Failure::Transport);
    workers.spawn("elevated-input-out", move |cancel| input_egress(cancel, outbound, writer))?;

    let (observation, pending) = Outbox::new();
    let losses = observation.clone();
    let context_writer = Sender::new(&links.context, &links.connection, Failure::Transport);
    workers.spawn("elevated-context-out", move |cancel| context_egress(cancel, pending, context_writer, losses))?;
    status.send(&Status::Configured, startup)?;
    let mut owner = Owner {
        session, connection: &links.connection, pen_hid, current: None, lease: None,
        grant_sequence: 0, epoch: AdmissionEpoch(0), source_generation: 0, environment: None, cleanup_limit: None, revisions: (0, 0),
        status, observation, input, urgent,
    };
    let result = owner.run(incoming, query_owner);
    if owner.current.is_some() {
        if result.is_err() {
            owner.connection.stop.cancel();
        }
        let _ = owner.close(None);
    }
    result
}

/// Reads the control lane. A due Stop, Revoke or Environment change is
/// applied to the current grant before the command is queued for the owner.
fn control_reader(
    cancel: &Cancellation, mut control: Receiver<Control>, commands: SyncSender<Control>, urgent: &Urgent,
    lifetime: &Lifetime,
) -> Result<()> {
    while cancel.check().is_ok() {
        let command = control.receive(Instant::now() + LEASE)?;
        urgent.preempt(&command, lifetime)?;
        commands.try_send(command).map_err(|_| Failure::Protocol)?;
    }
    Ok(())
}

fn context_reader(
    cancel: &Cancellation, mut context: Receiver<ContextCommand>, queries: SyncSender<ContextCommand>,
) -> Result<()> {
    while cancel.check().is_ok() {
        let command = context.receive(long_wait())?;
        queries.try_send(command).map_err(|_| Failure::Protocol)?;
    }
    Ok(())
}

/// The Recorder never writes to the input lane. This read ends only with a
/// lane failure, the peer's exit, cancellation, or a protocol violation.
fn input_monitor(cancel: &Cancellation, pipe: &Pipe, peer: &Process) -> Result<()> {
    pipe.read_message(&mut [0], Deadline::new(long_wait(), Failure::Transport, cancel).peer(peer))?;
    Err(Failure::Protocol)
}

impl Owner<'_> {
    /// A control write fails the lane only past the cleanup bound, the same
    /// bound within which the supervisor requires owner progress (E3).
    fn send(&mut self, status: &Status) -> Result<()> {
        self.status.send(status, Instant::now() + CLEANUP)
    }

    fn run(&mut self, commands: Queue<Control>, queries: Queue<ContextCommand>) -> Result<()> {
        loop {
            self.connection.stop.check()?;
            self.session.lifetime.owner_progress(qpc()?);
            self.witness_update()?;
            for _ in 0..COMMANDS {
                match commands.try_recv() {
                    Ok(command) => { if self.handle(command)? == Flow::Stopped { return Ok(()); } }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(Failure::Transport),
                }
            }
            self.native_ready()?;
            self.input_health()?;
            self.finish_held()?;
            for _ in 0..COMMANDS {
                match queries.try_recv() {
                    Ok(command) => self.query(command)?,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(Failure::Transport),
                }
            }
            self.expire_reads()?;
            thread::park_timeout(POLL);
        }
    }

    fn handle(&mut self, command: Control) -> Result<Flow> {
        let now = qpc()?;
        let due = urgent_due(&command, now);
        match command {
            Control::Renew(lease) => self.renew(lease, now)?,
            Control::Prepare(request) => self.prepare(request, now)?,
            Control::Activate(activation) => self.activate(activation)?,
            Control::RefreshFocus { previous, next, lease } => self.refresh_focus(previous, next, lease)?,
            Control::AcceptResync { visit, request, completed_qpc } => self.accept_resync(visit, request, completed_qpc)?,
            Control::Revoke { visit, before_qpc } if before_qpc.0 > 0 && due => self.revoke(visit, before_qpc)?,
            Control::Environment(revision) if revision.changed_qpc.0 > 0 && due => return self.environment(revision),
            Control::Stop { before_qpc, cleanup_deadline_qpc } if before_qpc.0 > 0 && due
                && cleanup_deadline_qpc.is_none_or(|deadline| deadline.0 > 0) =>
            {
                return self.stop(before_qpc, cleanup_deadline_qpc);
            }
            _ => return Err(Failure::Protocol),
        }
        Ok(Flow::Continue)
    }

    fn renew(&mut self, lease: Lease, now: QpcTick) -> Result<()> {
        self.session.lifetime.renew(self.lease, lease, now)?;
        self.lease = Some(lease);
        if let Some(visit) = &self.current && !visit.grant.stopped() {
            match visit.grant.renew(lease) {
                Ok(()) | Err(Failure::ScopeClosed) => {}
                Err(_) => {
                    let notice = visit.grant.loss(now)
                        .unwrap_or(Cut { reason: Revocation::Foreground, before: visit.grant.armed_qpc() });
                    self.close(Some(notice))?;
                }
            }
        }
        Ok(())
    }

    fn prepare(&mut self, request: GrantRequest, now: QpcTick) -> Result<()> {
        let visit = request.visit;
        if self.urgent.stopping.load(Acquire) || self.current.is_some() || visit.grant.get() <= self.grant_sequence
            // The same epoch may prepare again before admission (D-8 A);
            // grant and source generation still strictly increase.
            || visit.epoch < self.epoch || visit.source_generation.get() <= self.source_generation
            || request.expires_qpc <= now
        {
            return self.transition_rejected(visit, Transition::Prepare,
                if request.expires_qpc <= now { ReadUnavailable::TimedOut } else { ReadUnavailable::Stale });
        }
        self.grant_sequence = visit.grant.get();
        self.epoch = visit.epoch;
        self.source_generation = visit.source_generation.get();
        let grant = match Grant::prepare(request, self.lease.ok_or(Failure::Protocol)?, Arc::clone(&self.session)) {
            Ok(grant) => grant,
            Err(Failure::TargetChanged | Failure::ScopeClosed | Failure::ProcessIdentity | Failure::TokenIdentity(_) | Failure::PeerExited) =>
                return self.refuse(visit, Revocation::Foreground, now),
            // The collector's canonical approval proof failed (E6).
            Err(Failure::TargetAccess) => return self.refuse(visit, Revocation::RequiredAccess, now),
            Err(error) => return Err(error),
        };
        let stopping = {
            let mut urgent = self.urgent.grant.lock().map_err(|_| Failure::Supervisor)?;
            let stopping = self.urgent.stopping.load(Acquire);
            if !stopping { *urgent = Some(Arc::clone(&grant)); }
            stopping
        };
        if stopping {
            // Every Prepare is acknowledged exactly once.
            return self.transition_rejected(visit, Transition::Prepare, ReadUnavailable::Stale);
        }
        let context = context::Owners::start(&grant, self.observation.clone())?;
        let seen = Arc::clone(&context.seen);
        let mut start = VisitStart { grant: Arc::clone(&grant), context: Some(context), notices: None };
        start.notices = Some(notices::Owner::start(Arc::clone(&grant), self.observation.clone(), seen)?);
        let native = Native::start(
            Arc::clone(&grant), self.connection.scope.qpc_frequency, self.pen_hid, self.input.clone(),
        )?;
        let (context, notices) = start.disarm()?;
        self.current = Some(Visit {
            last_request: 0, grant, native, context, notices, registered: false, held_result: None,
            readiness: readiness(), resync: None,
        });
        Ok(())
    }

    /// Answers a Prepare whose grant could not be proven.
    fn refuse(&mut self, visit: VisitBinding, reason: Revocation, now: QpcTick) -> Result<()> {
        self.send(&Status::Invalidated { visit, reason, before_qpc: now })?;
        self.send(&Status::Closed { visit, before_qpc: now })
    }

    fn activate(&mut self, activation: Activation) -> Result<()> {
        let active = self.lease.filter(|active| active.sequence == activation.lease);
        let current = self.current.as_ref().filter(|visit| visit.registered && visit.grant.visit() == activation.visit);
        let activation_qpc = match current.zip(active).map(|(visit, lease)| visit.grant.activate(activation, lease)) {
            Some(Ok(tick)) => tick,
            Some(Err(Failure::Protocol)) => return Err(Failure::Protocol),
            _ => return self.transition_rejected(activation.visit, Transition::Activate, ReadUnavailable::Stale),
        };
        if let Some(current) = self.current.as_mut() {
            current.resync = Some(Resync::bounded(activation.opening_qpc));
        }
        self.send(&Status::Activated { visit: activation.visit, activation_qpc })
    }

    fn refresh_focus(&mut self, previous: VisitBinding, next: VisitBinding, lease: NonZeroU64) -> Result<()> {
        let active = self.lease.filter(|active| active.sequence == lease);
        let current = self.current.as_mut().filter(|visit| visit.grant.visit() == previous);
        let Some((current, active)) = current.zip(active) else {
            return self.transition_rejected(next, Transition::RefreshFocus, ReadUnavailable::Stale);
        };
        let (focus, resync_after_qpc) = match current.grant.refresh_focus(previous, next, active) {
            Ok(opened) => opened,
            Err(Failure::TargetChanged | Failure::ScopeClosed) =>
                return self.transition_rejected(next, Transition::RefreshFocus, ReadUnavailable::Stale),
            Err(error) => return Err(error),
        };
        current.held_result = None;
        // A resync not yet accepted keeps its earlier start.
        let from = current.resync.map_or(resync_after_qpc, |resync| resync.from);
        current.resync = Some(Resync::bounded(from));
        self.send(&Status::FocusRefreshed { visit: next, focus, resync_after_qpc })
    }

    fn accept_resync(&mut self, visit: VisitBinding, request: RequestId, completed_qpc: QpcTick) -> Result<()> {
        let result = self.current.as_mut().filter(|current| current.grant.visit() == visit)
            .and_then(|current| current.held_result.as_mut())
            .filter(|held| held.read.request == request && held.completed == completed_qpc && !held.accepted);
        if let Some(result) = result {
            result.accepted = true;
            Ok(())
        } else {
            self.transition_rejected(visit, Transition::AcceptResync, ReadUnavailable::Stale)
        }
    }

    fn revoke(&mut self, visit: VisitBinding, before_qpc: QpcTick) -> Result<()> {
        if self.current.as_ref().is_some_and(|current| current.grant.visit() == visit) {
            self.close(Some(Cut { reason: Revocation::Requested, before: before_qpc }))
        } else {
            self.transition_rejected(visit, Transition::Revoke, ReadUnavailable::Stale)
        }
    }

    fn environment(&mut self, revision: EnvironmentRevision) -> Result<Flow> {
        if self.environment.is_some_and(|old| old >= revision.sequence) {
            return Err(Failure::Protocol);
        }
        self.environment = Some(revision.sequence);
        if revision.cleanup_deadline_qpc.is_some_and(|deadline| deadline.0 <= 0) {
            return Err(Failure::Protocol);
        }
        self.cleanup_limit = revision.cleanup_deadline_qpc;
        self.close(Some(Cut { reason: Revocation::Environment, before: revision.changed_qpc }))?;
        if revision.change == EnvironmentChange::SessionEnding {
            return self.stopped();
        }
        Ok(Flow::Continue)
    }

    fn stop(&mut self, before_qpc: QpcTick, cleanup_deadline_qpc: Option<QpcTick>) -> Result<Flow> {
        self.cleanup_limit = cleanup_deadline_qpc;
        self.close(Some(Cut { reason: Revocation::Stopping, before: before_qpc }))?;
        self.stopped()
    }

    fn stopped(&mut self) -> Result<Flow> {
        self.send(&Status::Stopped)?;
        // Keep the original process and lanes live until the Recorder has
        // read the terminal record and closed the connection. Its sibling
        // lanes otherwise race peer-exit against the control acknowledgement.
        self.connection.stop.wait(CLEANUP);
        Ok(Flow::Stopped)
    }

    fn witness_update(&mut self) -> Result<()> {
        let now = qpc()?;
        if let Some(visit) = &self.current && let Some(loss) = visit.grant.loss(now) {
            if loss.reason == Revocation::Focus {
                match visit.grant.suspend_focus() {
                    Ok(true) => {
                        let binding = visit.grant.visit();
                        let before_qpc = visit.grant.certified(binding, loss.before);
                        if let Some(current) = self.current.as_mut() {
                            // A suspended visit waits for its refresh without a bound.
                            current.resync = Some(Resync { from: before_qpc, expires: None });
                        }
                        self.send(&Status::FocusSuspended { visit: binding, before_qpc })?;
                    }
                    Ok(false) => {}
                    Err(_) => self.close(Some(loss))?,
                }
            } else {
                self.close(Some(loss))?;
            }
        }
        let observed = self.session.witness.read()?.ok_or(Failure::Witness)?;
        let next = (observed.revision, observed.focus_revision);
        if self.revisions != next {
            self.send(&Status::Witness {
                revision: next.0, focus_revision: next.1, safe: observed.safe, observed_qpc: observed.observed,
            })?;
            self.revisions = next;
        }
        Ok(())
    }

    fn native_ready(&mut self) -> Result<()> {
        let Some(visit) = self.current.as_mut() else { return Ok(()) };
        if visit.grant.stopped() { return Ok(()); }
        if visit.native.ended() || visit.context.ended() || visit.notices.ended() {
            self.close(Some(Cut { reason: Revocation::NativeFailure, before: qpc()? }))?;
            return Err(Failure::Transport);
        }
        if visit.registered { return Ok(()); }
        if !visit.notices.ready() && Instant::now() < visit.readiness { return Ok(()); }
        if !visit.notices.ready() { return Err(Failure::ReadinessDeadline); }
        match visit.native.ready.try_recv() {
            Ok(registration) => {
                // The Arm QPC follows hook and Raw Input registration and
                // precedes the held probe (E4).
                let arm_qpc = qpc()?;
                let focus = visit.grant.observe_readiness_focus()?;
                let (_, _, held) = match input::held(&visit.grant) {
                    Ok(result) => result,
                    Err(Failure::TargetAccess) => {
                        self.close(Some(Cut { reason: Revocation::RequiredAccess, before: qpc()? }))?;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                visit.grant.arm()?;
                visit.registered = true;
                let status = Status::Armed {
                    visit: visit.grant.visit(), arm_qpc, registration, focus,
                    held: if held.is_some() { HeldReadiness::Known } else { HeldReadiness::Unknown },
                };
                self.send(&status)
            }
            Err(TryRecvError::Empty) if Instant::now() < visit.readiness => Ok(()),
            _ => Err(Failure::Transport),
        }
    }

    fn transition_rejected(&mut self, visit: VisitBinding, transition: Transition, reason: ReadUnavailable) -> Result<()> {
        self.send(&Status::TransitionRejected { visit, transition, reason })
    }

    fn read_rejected(&mut self, read: ReadBinding, reason: ReadUnavailable) -> Result<()> {
        self.send(&Status::ReadRejected { read, reason })
    }

    fn query(&mut self, command: ContextCommand) -> Result<()> {
        let (query, secondary) = match command {
            ContextCommand::Read { query, secondary } => (query, secondary),
            ContextCommand::Cancel { visit: binding, request } => {
                if let Some(current) = self.current.as_mut().filter(|current| current.grant.visit() == binding) {
                    if current.held_result.as_ref().is_some_and(|held| held.read.request == request) {
                        current.held_result.take();
                    }
                    return current.context.cancel(binding, request);
                }
                return Ok(());
            }
        };
        let read = query.binding();
        let now = qpc()?;
        if read.request.0 == 0 || read.requested_qpc.0 <= 0 || read.requested_qpc > now
            || read.deadline_qpc <= read.requested_qpc
            || read.deadline_qpc.0 - read.requested_qpc.0 > self.session.lifetime.ticks(LEASE)?
        {
            return Err(Failure::Protocol);
        }
        let Some(visit) = self.current.as_mut().filter(|visit| {
            read.visit == visit.grant.visit() && !visit.grant.stopped()
        }) else {
            return self.read_rejected(read, ReadUnavailable::Stale);
        };
        if read.request.0 <= visit.last_request {
            return self.read_rejected(read, ReadUnavailable::Stale);
        }
        visit.last_request = read.request.0;
        if read.deadline_qpc <= now {
            return self.read_rejected(read, ReadUnavailable::TimedOut);
        }
        let held = matches!(query, Query::Held(_));
        let Ok(floor) = visit.grant.read_floor(held) else {
            return self.read_rejected(read, ReadUnavailable::Stale);
        };
        if read.requested_qpc < floor {
            return self.read_rejected(read, ReadUnavailable::Stale);
        }
        if !held {
            let submitted = visit.context.submit(query, secondary);
            // A revoke can stop the read workers after the scope check above;
            // the read is then stale, not a transport fault (§3.2).
            let stopped = visit.grant.stopped();
            return match submitted {
                Ok(()) => Ok(()),
                Err(Failure::Busy) => self.read_rejected(read, ReadUnavailable::Unavailable),
                Err(Failure::Transport) if stopped => self.read_rejected(read, ReadUnavailable::Stale),
                Err(error) => Err(error),
            };
        }
        if read.target != read.visit.foreground || secondary.is_some() { return Err(Failure::Protocol); }
        if visit.held_result.is_some() {
            return self.read_rejected(read, ReadUnavailable::Unavailable);
        }
        let (start, end, result) = match input::held(&visit.grant) {
            Ok(result) => result,
            Err(Failure::TargetChanged | Failure::ScopeClosed | Failure::Cancelled) =>
                return self.read_rejected(read, ReadUnavailable::Stale),
            Err(Failure::TargetAccess) => return self.close(Some(Cut { reason: Revocation::RequiredAccess, before: now })),
            Err(error) => return Err(error),
        };
        if end >= read.deadline_qpc {
            return self.read_rejected(read, ReadUnavailable::TimedOut);
        }
        let readiness = if result.is_some() { HeldReadiness::Known } else { HeldReadiness::Unknown };
        let outcome = result.map_or(ReadOutcome::Unavailable(ReadUnavailable::Unavailable), ReadOutcome::Observed);
        let message = ContextEvidence::Held(ReadEvidence {
            binding: read, started_qpc: Some(start), completed_qpc: Some(end), outcome,
        });
        if self.observation.publish(&visit.grant, read.visit, message, true)? {
            if visit.grant.opening() {
                visit.held_result = Some(HeldResult { read, completed: end, readiness, accepted: false });
            }
        } else {
            return self.read_rejected(read, ReadUnavailable::Unavailable);
        }
        Ok(())
    }

    /// A provider call still running at its request deadline gets its typed
    /// timeout now; the grant stays valid (E2).
    fn expire_reads(&mut self) -> Result<()> {
        let Some(visit) = &self.current else { return Ok(()) };
        let now = qpc()?;
        for read in visit.context.expire(now)? {
            self.observation.publish(&visit.grant, read.visit, ContextEvidence::Rejected(ReadEvidence {
                binding: read, started_qpc: None, completed_qpc: Some(now),
                outcome: ReadOutcome::Unavailable(ReadUnavailable::TimedOut),
            }), false)?;
        }
        Ok(())
    }

    fn input_health(&mut self) -> Result<()> {
        let Some(current) = self.current.as_mut() else { return Ok(()) };
        let now = qpc()?;
        if current.resync.and_then(|resync| resync.expires).is_some_and(|until| Instant::now() >= until)
            || current.grant.essential_stalled(now)?
        {
            return self.close(Some(Cut { reason: Revocation::NativeFailure, before: now }));
        }
        match current.grant.reopen_input() {
            Ok(Some(Reopened { previous, visit, loss, resync_after_qpc })) => {
                current.resync = Some(Resync::bounded(loss.before));
                current.held_result = None;
                self.send(&Status::ResyncRequired {
                    previous, visit, first_uncovered: loss.before, resync_after_qpc,
                    cause: loss.reason, native_gap: loss.gap,
                })
            }
            Ok(None) | Err(Failure::TargetChanged | Failure::ScopeClosed) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn finish_held(&mut self) -> Result<()> {
        let Some(visit) = self.current.as_mut() else { return Ok(()) };
        let Some(result) = &visit.held_result else { return Ok(()) };
        let read = result.read;
        let expired = qpc()? >= read.deadline_qpc;
        if !result.accepted || !visit.grant.held_delivered(read.visit, read.request, result.completed) {
            if expired {
                visit.held_result.take();
                return self.read_rejected(read, ReadUnavailable::TimedOut);
            }
            return Ok(());
        }
        let Some(result) = visit.held_result.take() else { return Err(Failure::Supervisor) };
        if expired {
            return self.read_rejected(read, ReadUnavailable::TimedOut);
        }
        match visit.grant.finish_opening(result.completed) {
            Ok(()) => {
                let before = self.session.lifetime.after(result.completed, INPUT_HOLDBACK)?;
                let first = visit.resync.take().map_or(result.completed, |resync| resync.from);
                self.send(&Status::ResyncAccepted {
                    visit: read.visit, after_qpc: result.completed, held: result.readiness,
                    input_coverage: CoverageLoss {
                        first_qpc: first, before_qpc: before,
                        reason: if result.readiness == HeldReadiness::Unknown { CoverageReason::HeldUnknown } else { CoverageReason::Opening },
                    },
                })
            }
            Err(Failure::ScopeClosed | Failure::TargetChanged | Failure::Cancelled) =>
                self.transition_rejected(read.visit, Transition::AcceptResync, ReadUnavailable::Stale),
            Err(error) => Err(error),
        }
    }

    fn close(&mut self, notice: Option<Cut>) -> Result<()> {
        let limit = self.cleanup_limit.take();
        let Some(visit) = self.current.take() else { return Ok(()) };
        visit.grant.revoke();
        let binding = visit.grant.visit();
        self.urgent.grant.lock().map_err(|_| Failure::Supervisor)?.take();
        self.session.lifetime.begin_cleanup(qpc()?, limit)?;
        let defer_native_failure = !visit.registered
            && matches!(notice, Some(Cut { reason: Revocation::NativeFailure, .. }));
        let notify = if !defer_native_failure && let Some(Cut { reason, before: cut }) = notice {
            let before_qpc = visit.grant.certified(binding, cut);
            self.send(&Status::Invalidated { visit: binding, reason, before_qpc })
        } else { Ok(()) };
        let failure = visit.native.join();
        let context = visit.context.stop();
        let notices = visit.notices.join();
        if !self.urgent.stopping.load(Acquire) {
            self.session.lifetime.end_cleanup();
        }
        notify?;
        let failure = failure?;
        if notice.is_some() && let Some(failure) = failure {
            self.send(&Status::Failed(failure))?;
            return Err(Failure::Transport);
        }
        context?;
        notices?;
        if let Some(Cut { reason, before: cut }) = notice {
            let before_qpc = visit.grant.certified(binding, cut);
            if defer_native_failure {
                self.send(&Status::Invalidated { visit: binding, reason, before_qpc })?;
            }
            self.send(&Status::Closed { visit: binding, before_qpc })?;
        }
        Ok(())
    }
}

fn configure(control: &mut Receiver<Control>, until: Instant) -> Result<Configuration> {
    let Control::Configure { approved_count, pen_hid } = control.receive(until)? else {
        return Err(Failure::Protocol);
    };
    let count = usize::from(approved_count);
    if !(1..=MAX_APPROVED_EXECUTABLES).contains(&count) {
        return Err(Failure::Protocol);
    }
    let mut approved = Vec::with_capacity(count);
    for _ in 0..count {
        let Control::Approval(key) = control.receive(until)? else {
            return Err(Failure::Protocol);
        };
        if !Path::new(key.as_str()).is_absolute() || key.as_str().contains('\0') || approved.contains(&key) {
            return Err(Failure::Protocol);
        }
        approved.push(key);
    }
    if control.receive(until)? != Control::SealConfiguration {
        return Err(Failure::Protocol);
    }
    Ok(Configuration { approved: approved.into(), pen_hid })
}

fn long_wait() -> Instant { Instant::now() + LONG_WAIT }

fn readiness() -> Instant { Instant::now() + READINESS }

struct Workers {
    stop: Arc<Cancellation>,
    owners: Vec<JoinHandle<()>>,
}

impl Workers {
    fn spawn(&mut self, name: &str, action: impl FnOnce(&Cancellation) -> Result<()> + Send + 'static) -> Result<()> {
        let cancel = Arc::clone(&self.stop);
        let owner = thread::Builder::new().name(name.into()).spawn(move || {
            if action(&cancel).is_err() { cancel.cancel(); }
        }).map_err(|_| Failure::Supervisor)?;
        self.owners.push(owner);
        Ok(())
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.stop.cancel();
        for owner in self.owners.drain(..) { let _ = owner.join(); }
    }
}
