use std::{
    num::NonZeroU64,
    path::Path,
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering::{Acquire, Release}}, mpsc::{self, Receiver as Queue, SyncSender, TryRecvError}},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use screen_fix_protocol::{
    QpcFrequency, QpcTick,
    ipc::{
        EnvironmentChange,
        elevated::{ContextEvidence, GrantedInput, InputEvidence, Query, ReadBinding, ReadEvidence, ReadOutcome, ReadUnavailable,
            runtime::{ApprovalKey, ContextCommand, Control, CoverageLoss, CoverageReason, HeldReadiness, Lease, MAX_APPROVED_EXECUTABLES,
                Observation, Revocation, Status, Transition}, VisitBinding},
    },
};

use crate::{deadline::{Cancellation, Deadline}, win::{Failure, Result}};
use super::{
    Connection, Links, Receiver, Sender, context, notices, outbox::Outbox,
    grant::{ESSENTIAL_PROGRESS, Grant, InputLoss, INPUT_HOLDBACK, await_witness},
    input::{self, Native},
    lifetime::{LEASE, Lifetime, qpc},
    witness::{View, Witness},
};

const COMMANDS: usize = 8;
const POLL: Duration = Duration::from_millis(4);
/// Budget of the provider-backed egress gate of one context record.
const GATE: Duration = Duration::from_millis(100);

struct Configuration {
    approved: Arc<[ApprovalKey]>,
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
    resync_from: Option<QpcTick>,
    resync_expires: Option<Instant>,
}

struct HeldResult {
    read: ReadBinding,
    completed: QpcTick,
    readiness: HeldReadiness,
    accepted: bool,
}

struct Owner<'a> {
    witness: View,
    lifetime: &'a Arc<Lifetime>,
    connection: &'a Arc<Connection>,
    configuration: Configuration,
    frequency: QpcFrequency,
    current: Option<Visit>,
    lease: Option<Lease>,
    grant_sequence: u64,
    epoch: u64,
    source_generation: u64,
    environment: Option<NonZeroU64>,
    cleanup_limit: Option<QpcTick>,
    revisions: (u64, u64),
    status: Sender<Status>,
    observation: Outbox,
    input: SyncSender<input::Item>,
    urgent: Arc<Urgent>,
}

#[derive(Default)]
struct Urgent {
    grant: Mutex<Option<Arc<Grant>>>,
    stopping: AtomicBool,
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
    let mut control = Receiver::<Control>::new(&links.control, &links.connection);
    let mut status = Sender::<Status>::new(&links.control, &links.connection);
    let configuration = configure(&mut control, startup)?;
    await_witness(&witness.view, &links.connection.stop, startup)?;
    lifetime.witness.set(Arc::clone(&witness.progress)).map_err(|_| Failure::Supervisor)?;

    let mut workers = Workers { stop: Arc::clone(&links.connection.stop), owners: Vec::with_capacity(5) };
    let urgent = Arc::new(Urgent::default());
    let urgent_revoke = Arc::clone(&urgent);
    let urgent_lifetime = Arc::clone(lifetime);
    let (commands, incoming) = mpsc::sync_channel(COMMANDS);
    workers.spawn("elevated-control-in", move |cancel| {
        while cancel.check().is_ok() {
            let command = control.receive(Instant::now() + LEASE)?;
            if matches!(&command, Control::Stop { .. } | Control::Revoke { .. } | Control::Environment(_)) {
                let relevant = urgent_revoke.grant.lock().map_err(|_| Failure::Supervisor)?;
                let now = qpc()?;
                match &command {
                    Control::Stop { before_qpc, cleanup_deadline_qpc } if *before_qpc <= now => {
                        urgent_revoke.stopping.store(true, Release);
                        if let Some(grant) = relevant.as_ref() { grant.revoke(); }
                        urgent_lifetime.begin_cleanup(now, *cleanup_deadline_qpc)?;
                    }
                    Control::Revoke { visit, before_qpc } if *before_qpc <= now => {
                        if let Some(grant) = relevant.as_ref().filter(|grant| grant.visit() == *visit) {
                            grant.revoke();
                            urgent_lifetime.enter_cleanup(now)?;
                        }
                    }
                    Control::Environment(revision) if revision.changed_qpc <= now => {
                        if revision.change == EnvironmentChange::SessionEnding {
                            urgent_revoke.stopping.store(true, Release);
                        }
                        if let Some(grant) = relevant.as_ref() {
                            grant.revoke();
                            urgent_lifetime.begin_cleanup(now, revision.cleanup_deadline_qpc)?;
                        }
                    }
                    _ => {}
                }
            }
            commands.try_send(command).map_err(|_| Failure::Protocol)?;
        }
        Ok(())
    })?;

    let mut context = Receiver::<ContextCommand>::new(&links.context, &links.connection);
    let (queries, query_owner) = mpsc::sync_channel(COMMANDS);
    workers.spawn("elevated-context-in", move |cancel| {
        while cancel.check().is_ok() {
            let command = context.receive(long_wait())?;
            queries.try_send(command).map_err(|_| Failure::Protocol)?;
        }
        Ok(())
    })?;
    let input_pipe = Arc::clone(&links.input);
    let peer = Arc::clone(&links.connection.peer);
    workers.spawn("elevated-input-monitor", move |cancel| {
        let deadline = Deadline { at: long_wait(), cancellation: cancel, grant: None, scope_current: None, peer: Some(&peer) };
        input_pipe.read_message(&mut [0], deadline)?;
        Err(Failure::Protocol)
    })?;

    let (input, outbound) = mpsc::sync_channel::<input::Item>(input::SLOTS);
    let mut writer = Sender::<GrantedInput>::new(&links.input, &links.connection);
    workers.spawn("elevated-input-out", move |cancel| {
        while cancel.check().is_ok() {
            let mut verified = None;
            for item in batch(&outbound) {
                verify_once(&mut verified, &item.grant);
                // One outstanding write; later records wait in the bounded
                // queue, whose producers drop newest with loss accounting.
                // Only a write stalled past the essential coverage budget
                // fails the lane (E3).
                if item.grant.qualify_egress(item.record.visit, false)
                    && writer.send_scoped(&item.record, Instant::now() + ESSENTIAL_PROGRESS, &item.grant, item.record.visit)?
                {
                    match item.record.evidence {
                        InputEvidence::Progress(progress) => item.grant.input_progress(item.record.visit, progress),
                        // A gap carried from the previous binding only counts;
                        // a gap reaching past the floor needs a resync (E3).
                        InputEvidence::Gap(gap) => if let Ok(floor) = item.grant.read_floor(false) && gap.last_qpc > floor {
                            item.grant.input_loss(item.record.visit, InputLoss {
                                before: gap.first_qpc.max(floor), reason: CoverageReason::NativeGap, gap: Some(gap),
                            });
                        },
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    })?;

    let (observation, pending) = Outbox::new();
    let drain_losses = observation.clone();
    let mut context_writer = Sender::<Observation>::new(&links.context, &links.connection);
    workers.spawn("elevated-context-out", move |cancel| {
        while cancel.check().is_ok() {
            drain_losses.flush_losses()?;
            let mut verified = None;
            for mut item in batch(&pending) {
                verify_once(&mut verified, &item.grant);
                if let Some(delivery) = &item.delivery
                    && item.visit == item.grant.visit() && item.grant.read_floor(false).is_ok()
                {
                    let deadline = qpc()?.0.checked_add(item.grant.lifetime.ticks(GATE)?).ok_or(Failure::Supervisor)?;
                    if let Some(_provider) = item.grant.lifetime.provider(super::lifetime::ProviderKind::Egress, QpcTick(deadline)) {
                        delivery.gate(&item.grant, &mut item.evidence)?;
                    } else {
                        return Err(Failure::Supervisor);
                    }
                }
                if item.grant.qualify_egress(item.visit, item.allow_opening)
                    && context_writer.send_scoped(&item.evidence, Instant::now() + ESSENTIAL_PROGRESS, &item.grant, item.visit)?
                {
                    match item.evidence {
                        Observation::Progress { before_qpc, .. } => item.grant.context_progress(item.visit, before_qpc),
                        Observation::Item { evidence: ContextEvidence::Held(read), .. } => {
                            if let Some(end) = read.completed_qpc {
                                item.grant.deliver_held(item.visit, read.binding.request, end);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    })?;
    status.send(&Status::Configured, startup)?;
    let mut owner = Owner {
        witness: witness.view.clone(), lifetime, connection: &links.connection, configuration,
        frequency: links.connection.scope.qpc_frequency, current: None, lease: None,
        grant_sequence: 0, epoch: 0, source_generation: 0, environment: None, cleanup_limit: None, revisions: (0, 0),
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

impl Owner<'_> {
    /// A control write fails the lane only past the lease bound (E3).
    fn send(&mut self, status: &Status) -> Result<()> {
        self.status.send(status, Instant::now() + LEASE)
    }

    fn run(&mut self, commands: Queue<Control>, queries: Queue<ContextCommand>) -> Result<()> {
        loop {
            self.connection.stop.check()?;
            self.lifetime.owner_progress(qpc()?);
            self.witness_update()?;
            for _ in 0..COMMANDS {
                match commands.try_recv() {
                    Ok(command) => { if self.handle(command)? { return Ok(()); } }
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

    fn handle(&mut self, command: Control) -> Result<bool> {
        let now = qpc()?;
        match command {
            Control::Renew(lease) => {
                self.lifetime.renew(self.lease, lease, now)?;
                self.lease = Some(lease);
                if let Some(visit) = &self.current && !visit.grant.stopped() {
                    match visit.grant.renew(lease) {
                        Ok(()) | Err(Failure::ScopeClosed) => {}
                        Err(_) => {
                            let notice = visit.grant.loss(now).unwrap_or((Revocation::Foreground, visit.grant.arm));
                            self.close(Some(notice))?;
                        }
                    }
                }
            }
            Control::Prepare(request) => {
                let visit = request.visit;
                if self.urgent.stopping.load(Acquire) || self.current.is_some() || visit.grant.get() <= self.grant_sequence
                    // The same epoch may prepare again before admission (D-8 A);
                    // grant and source generation still strictly increase.
                    || visit.epoch.0 < self.epoch || visit.source_generation.get() <= self.source_generation
                    || request.expires_qpc <= now
                {
                    self.transition_rejected(visit, Transition::Prepare,
                        if request.expires_qpc <= now { ReadUnavailable::TimedOut } else { ReadUnavailable::Stale })?;
                    return Ok(false);
                }
                self.grant_sequence = visit.grant.get();
                self.epoch = visit.epoch.0;
                self.source_generation = visit.source_generation.get();
                let grant = match Grant::prepare(request, &self.configuration.approved,
                    self.lease.ok_or(Failure::Protocol)?, Arc::clone(&self.connection.peer),
                    self.witness.clone(), Arc::clone(self.lifetime), Arc::clone(&self.connection.stop))
                {
                    Ok(grant) => grant,
                    Err(Failure::TargetChanged | Failure::ScopeClosed | Failure::ProcessIdentity | Failure::TokenIdentity(_) | Failure::PeerExited) => {
                        self.send(&Status::Invalidated { visit, reason: Revocation::Foreground, before_qpc: now })?;
                        self.send(&Status::Closed { visit, before_qpc: now })?;
                        return Ok(false);
                    }
                    // The collector's canonical approval proof failed (E6).
                    Err(Failure::TargetAccess) => {
                        self.send(&Status::Invalidated { visit, reason: Revocation::RequiredAccess, before_qpc: now })?;
                        self.send(&Status::Closed { visit, before_qpc: now })?;
                        return Ok(false);
                    }
                    Err(error) => return Err(error),
                };
                {
                    let mut urgent = self.urgent.grant.lock().map_err(|_| Failure::Supervisor)?;
                    if self.urgent.stopping.load(Acquire) {
                        return Ok(false);
                    }
                    *urgent = Some(Arc::clone(&grant));
                }
                let context = context::Owners::start(&grant, Arc::clone(&self.configuration.approved), self.observation.clone())?;
                let posted = notices::Owner::start(Arc::clone(&grant), self.observation.clone(), Arc::clone(&context.seen));
                let (notices, native) = match posted {
                    Ok(notices) => match Native::start(Arc::clone(&grant), self.frequency, self.configuration.pen_hid, self.input.clone()) {
                        Ok(native) => (notices, native),
                        Err(error) => {
                            grant.revoke();
                            self.lifetime.enter_cleanup(qpc()?)?;
                            let _ = notices.join();
                            let _ = context.stop();
                            self.lifetime.end_cleanup();
                            return Err(error);
                        }
                    },
                    Err(error) => {
                        grant.revoke();
                        self.lifetime.enter_cleanup(qpc()?)?;
                        let _ = context.stop();
                        self.lifetime.end_cleanup();
                        return Err(error);
                    }
                };
                self.current = Some(Visit { last_request: 0, grant, native, context, notices, registered: false, held_result: None, readiness: Instant::now() + crate::deadline::READINESS, resync_from: None, resync_expires: None });
            }
            Control::Activate(activation) => {
                let active = self.lease.filter(|active| active.sequence == activation.lease);
                let current = self.current.as_ref().filter(|visit| visit.registered && visit.grant.visit() == activation.visit);
                let activation_qpc = match current.zip(active).map(|(visit, lease)| visit.grant.activate(activation, lease)) {
                    Some(Ok(tick)) => tick,
                    Some(Err(Failure::Protocol)) => return Err(Failure::Protocol),
                    _ => {
                        self.transition_rejected(activation.visit, Transition::Activate, ReadUnavailable::Stale)?;
                        return Ok(false);
                    }
                };
                if let Some(current) = self.current.as_mut() {
                    current.resync_from = Some(activation.opening_qpc);
                    current.resync_expires = Some(Instant::now() + crate::deadline::READINESS);
                }
                self.send(&Status::Activated { visit: activation.visit, activation_qpc })?;
            }
            Control::RefreshFocus { previous, next, lease } => {
                let active = self.lease.filter(|active| active.sequence == lease);
                let current = self.current.as_mut().filter(|visit| visit.grant.visit() == previous);
                let Some((current, active)) = current.zip(active) else {
                    self.transition_rejected(next, Transition::RefreshFocus, ReadUnavailable::Stale)?;
                    return Ok(false);
                };
                let (focus, resync_after_qpc) = match current.grant.refresh_focus(previous, next, active) {
                    Ok(opened) => opened,
                    Err(Failure::TargetChanged | Failure::ScopeClosed) => {
                        self.transition_rejected(next, Transition::RefreshFocus, ReadUnavailable::Stale)?;
                        return Ok(false);
                    }
                    Err(error) => return Err(error),
                };
                current.held_result = None;
                current.resync_from.get_or_insert(resync_after_qpc);
                current.resync_expires = Some(Instant::now() + crate::deadline::READINESS);
                self.send(&Status::FocusRefreshed { visit: next, focus, resync_after_qpc })?;
            }
            Control::AcceptResync { visit, request, completed_qpc } => {
                let result = self.current.as_mut().filter(|current| current.grant.visit() == visit)
                    .and_then(|current| current.held_result.as_mut())
                    .filter(|held| held.read.request == request && held.completed == completed_qpc && !held.accepted);
                if let Some(result) = result {
                    result.accepted = true;
                } else {
                    self.transition_rejected(visit, Transition::AcceptResync, ReadUnavailable::Stale)?;
                }
            }
            Control::Revoke { visit, before_qpc } if before_qpc.0 > 0 && before_qpc <= now => {
                if self.current.as_ref().is_some_and(|current| current.grant.visit() == visit) {
                    self.close(Some((Revocation::Requested, before_qpc)))?;
                } else {
                    self.transition_rejected(visit, Transition::Revoke, ReadUnavailable::Stale)?;
                }
            }
            Control::Environment(revision) if revision.changed_qpc.0 > 0 && revision.changed_qpc <= now => {
                if self.environment.is_some_and(|old| old >= revision.sequence) {
                    return Err(Failure::Protocol);
                }
                self.environment = Some(revision.sequence);
                if revision.cleanup_deadline_qpc.is_some_and(|deadline| deadline.0 <= 0) {
                    return Err(Failure::Protocol);
                }
                self.cleanup_limit = revision.cleanup_deadline_qpc;
                self.close(Some((Revocation::Environment, revision.changed_qpc)))?;
                if revision.change == EnvironmentChange::SessionEnding {
                    return self.stopped();
                }
            }
            Control::Stop { before_qpc, cleanup_deadline_qpc } if before_qpc.0 > 0 && before_qpc <= now
                && cleanup_deadline_qpc.is_none_or(|deadline| deadline.0 > 0) =>
            {
                self.cleanup_limit = cleanup_deadline_qpc;
                self.close(Some((Revocation::Stopping, before_qpc)))?;
                return self.stopped();
            }
            _ => return Err(Failure::Protocol),
        }
        Ok(false)
    }

    fn stopped(&mut self) -> Result<bool> {
        self.send(&Status::Stopped)?;
        // Keep the original process and lanes live until the Recorder has
        // read the terminal record and closed the connection. Its sibling
        // lanes otherwise race peer-exit against the control acknowledgement.
        let until = Instant::now() + crate::deadline::CLEANUP;
        while self.connection.stop.check().is_ok() && Instant::now() < until {
            thread::park_timeout(POLL);
        }
        Ok(true)
    }

    fn witness_update(&mut self) -> Result<()> {
        let now = qpc()?;
        if let Some(visit) = &self.current && let Some(loss) = visit.grant.loss(now) {
            if loss.0 == Revocation::Focus {
                match visit.grant.suspend_focus() {
                    Ok(true) => {
                        let binding = visit.grant.visit();
                        let before_qpc = visit.grant.certified(binding, loss.1);
                        if let Some(current) = self.current.as_mut() {
                            current.resync_from = Some(before_qpc);
                            current.resync_expires = None;
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
        let observed = self.witness.read()?.ok_or(Failure::Witness)?;
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
            self.close(Some((Revocation::NativeFailure, qpc()?)))?;
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
                        self.close(Some((Revocation::RequiredAccess, qpc()?)))?;
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
            || read.deadline_qpc.0 - read.requested_qpc.0 > self.lifetime.ticks(LEASE)?
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
            return match visit.context.submit(query, secondary) {
                Ok(()) => Ok(()),
                Err(Failure::Busy) => self.read_rejected(read, ReadUnavailable::Unavailable),
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
            Err(Failure::TargetAccess) => return self.close(Some((Revocation::RequiredAccess, now))),
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
        if current.resync_expires.is_some_and(|until| Instant::now() >= until)
            || current.grant.essential_stalled(now)?
        {
            return self.close(Some((Revocation::NativeFailure, now)));
        }
        match current.grant.reopen_input() {
            Ok(Some((previous, visit, loss, resync_after_qpc))) => {
                current.resync_from = Some(loss.before);
                current.resync_expires = Some(Instant::now() + crate::deadline::READINESS);
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
                let before = result.completed.0.checked_add(self.lifetime.ticks(INPUT_HOLDBACK)?).ok_or(Failure::Supervisor)?;
                let first = visit.resync_from.take().unwrap_or(result.completed);
                visit.resync_expires = None;
                self.send(&Status::ResyncAccepted {
                    visit: read.visit, after_qpc: result.completed, held: result.readiness,
                    input_coverage: CoverageLoss {
                        first_qpc: first, before_qpc: QpcTick(before),
                        reason: if result.readiness == HeldReadiness::Unknown { CoverageReason::HeldUnknown } else { CoverageReason::Opening },
                    },
                })
            }
            Err(Failure::ScopeClosed | Failure::TargetChanged | Failure::Cancelled) =>
                self.transition_rejected(read.visit, Transition::AcceptResync, ReadUnavailable::Stale),
            Err(error) => Err(error),
        }
    }

    fn close(&mut self, notice: Option<(Revocation, QpcTick)>) -> Result<()> {
        let limit = self.cleanup_limit.take();
        let Some(visit) = self.current.take() else { return Ok(()) };
        visit.grant.revoke();
        let binding = visit.grant.visit();
        self.urgent.grant.lock().map_err(|_| Failure::Supervisor)?.take();
        self.lifetime.begin_cleanup(qpc()?, limit)?;
        let defer_native_failure = !visit.registered && matches!(notice, Some((Revocation::NativeFailure, _)));
        let notify = if !defer_native_failure && let Some((reason, cut)) = notice {
            let before_qpc = visit.grant.certified(binding, cut);
            self.send(&Status::Invalidated { visit: binding, reason, before_qpc })
        } else { Ok(()) };
        let failure = visit.native.join();
        let context = visit.context.stop();
        let notices = visit.notices.join();
        if !self.urgent.stopping.load(Acquire) {
            self.lifetime.end_cleanup();
        }
        notify?;
        let failure = failure?;
        if notice.is_some() && let Some(failure) = failure {
            self.send(&Status::Failed(failure))?;
            return Err(Failure::Transport);
        }
        context?;
        notices?;
        if let Some((reason, cut)) = notice {
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

fn long_wait() -> Instant { Instant::now() + Duration::from_secs(86_400) }

/// The records of one egress pass: the first waits up to a poll interval, the
/// rest are already queued.
fn batch<T>(queue: &Queue<T>) -> impl Iterator<Item = T> + '_ {
    queue.recv_timeout(POLL).ok().into_iter().chain(std::iter::from_fn(move || queue.try_recv().ok())).take(input::SLOTS)
}

/// Token facts are queried once per egress batch and grant; per-record checks
/// read the result latched in the grant (E3).
fn verify_once(verified: &mut Option<Arc<Grant>>, grant: &Arc<Grant>) {
    if verified.as_ref().is_none_or(|last| !Arc::ptr_eq(last, grant)) {
        // A failure is latched; `qualify_egress` then refuses the records.
        let _ = grant.verify_token();
        *verified = Some(Arc::clone(grant));
    }
}

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
