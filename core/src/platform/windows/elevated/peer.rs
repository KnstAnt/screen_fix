use std::{
    num::NonZeroU64,
    sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}, mpsc::{self, Receiver as Queue, SyncSender, TryRecvError, TrySendError}},
    thread,
    time::{Duration, Instant},
};

use screen_fix_elevated::{Authenticated, Cancellation, Failure, HEARTBEAT, RecorderLanes, Receiver, Result};
use screen_fix_protocol::{AdmissionEpoch, AttemptId, QpcFrequency, QpcTick, SessionId, ipc::elevated::{
    GrantedInput,
    runtime::{ApprovalKey, ContextCommand, Control, Lease, MAX_APPROVED_EXECUTABLES, Observation, Record, Status},
}};

use crate::config::ResolvedProfile;

use super::super::clock;

/// The lease bound: a command or query write fails the connection only when
/// the collector stopped reading for a whole lease, not on a brief stall (E3).
const WRITE_BUDGET: Duration = Duration::from_secs(1);
const IDLE_READ: Duration = Duration::from_secs(86_400);
/// Progress older than this cannot renew the collector lease.
const LEASE_WINDOW: Duration = Duration::from_secs(1);
const CONTROL_QUEUE: usize = 8;
const QUERY_QUEUE: usize = 8;
/// Query slots an optional read never takes, so that a required held read
/// and the cancel of its predecessor always find room (D2).
const REQUIRED_QUERY_SLOTS: usize = 2;
const STATUS_QUEUE: usize = 32;
const INPUT_QUEUE: usize = 256;
const OBSERVATION_QUEUE: usize = 1024;

#[derive(Clone, Copy)]
pub(super) struct Progress {
    pub(super) now: QpcTick,
    pub(super) controller: QpcTick,
    pub(super) pump: QpcTick,
}

pub(super) struct LeaseState {
    one_second: i64,
    heartbeat: i64,
    last: Option<Lease>,
}

impl LeaseState {
    pub(super) fn new(frequency: QpcFrequency) -> Result<Self> {
        let one_second = frequency.ticks(LEASE_WINDOW).ok_or(Failure::Protocol)?;
        let heartbeat = frequency.ticks(HEARTBEAT).ok_or(Failure::Protocol)?.max(1);
        Ok(Self { one_second, heartbeat, last: None })
    }

    pub(super) fn renew(
        &mut self, peer: &Peer, progress: Progress, epoch: Option<AdmissionEpoch>,
        transition: Option<impl FnOnce(Lease) -> Control>,
    ) -> Result<bool> {
        let oldest = progress.now.0.saturating_sub(self.one_second);
        if progress.controller.0 <= oldest || progress.controller > progress.now
            || progress.pump.0 <= oldest || progress.pump > progress.now
            || self.last.is_some_and(|old| old.controller_qpc.0.min(old.pump_qpc.0) <= oldest)
        {
            return Err(Failure::LeaseExpired);
        }
        if let Some(old) = self.last {
            if progress.controller < old.controller_qpc || progress.pump < old.pump_qpc {
                return Err(Failure::LeaseExpired);
            }
            if progress.controller == old.controller_qpc || progress.pump == old.pump_qpc {
                return Ok(false);
            }
            if transition.is_none() && old.epoch == epoch
                && progress.now.0.saturating_sub(old.controller_qpc.0.max(old.pump_qpc.0)) < self.heartbeat
            {
                return Ok(false);
            }
        }
        let sequence = self.last.map_or(Some(1), |old| old.sequence.get().checked_add(1))
            .and_then(NonZeroU64::new).ok_or(Failure::Protocol)?;
        let lease = Lease { sequence, epoch, controller_qpc: progress.controller, pump_qpc: progress.pump };
        let renewal = Control::Renew(lease);
        if let Some(transition) = transition {
            peer.after_renewal(renewal, transition(lease))?;
        } else {
            peer.send(renewal)?;
        }
        self.last = Some(lease);
        Ok(true)
    }

    pub(super) fn due(&self, progress: Progress, epoch: Option<AdmissionEpoch>) -> bool {
        self.last.is_none_or(|last| last.epoch != epoch
            || progress.now.0.saturating_sub(last.controller_qpc.0.max(last.pump_qpc.0)) >= self.heartbeat)
    }
}

pub(super) struct Peer {
    connection: Arc<Connection>,
    command: SyncSender<Commands>,
    query: SyncSender<(ContextCommand, Instant)>,
    /// Queries sent and not yet taken by the context writer.
    queued_queries: Arc<AtomicUsize>,
    status: Queue<Status>,
    /// `None` is an input record whose payload did not decode (E3).
    input: Queue<Option<GrantedInput>>,
    observation: Queue<(QpcTick, Observation)>,
}

struct Commands {
    first: Control,
    then: Option<Control>,
    until: Instant,
}

struct Connection {
    cancel: Arc<Cancellation>,
    failure: Mutex<Option<Failure>>,
}

impl Connection {
    fn record(&self, failure: Failure) {
        let mut stored = self.failure.lock().unwrap_or_else(|poison| poison.into_inner());
        if stored.is_none() {
            *stored = Some(failure);
        }
        self.cancel.cancel();
    }

    fn check(&self) -> Result<()> {
        let failure = self.failure.lock().unwrap_or_else(|poison| poison.into_inner());
        (*failure).map_or_else(|| self.cancel.check(), Err)
    }
}

impl Peer {
    pub(super) fn start(
        authenticated: Authenticated,
        attempt: &AttemptId,
        session: &SessionId,
        frequency: QpcFrequency,
        profile: &ResolvedProfile,
        until: Instant,
    ) -> Result<Self> {
        let RecorderLanes { scope, cancel, mut command, status, input, mut query, observation } = authenticated.into_runtime()?;
        if scope.attempt != *attempt || scope.session != *session || scope.qpc_frequency != frequency {
            return Err(Failure::Protocol);
        }
        let approved = profile.approved_identities();
        if approved.is_empty() || approved.len() > MAX_APPROVED_EXECUTABLES {
            return Err(Failure::Protocol);
        }
        let approved_count = approved.len() as u8;
        let pen_hid = profile.pen_hid();
        let keys = approved.iter().map(|entry| ApprovalKey::new(entry.executable()).map_err(|_| Failure::Protocol))
            .collect::<Result<Vec<_>>>()?;
        let connection = Arc::new(Connection { cancel: Arc::clone(&cancel), failure: Mutex::new(None) });
        let (commands, command_queue) = mpsc::sync_channel::<Commands>(CONTROL_QUEUE);
        let (queries, query_queue) = mpsc::sync_channel::<(ContextCommand, Instant)>(QUERY_QUEUE);
        let queued_queries = Arc::new(AtomicUsize::new(0));
        let taken_queries = Arc::clone(&queued_queries);
        let result = (|| {
            let status = read_lane(&connection, status, STATUS_QUEUE, "elevated-control-read", Some(until), false,
                |record| record.ok_or(Failure::Protocol))?;
            // An undecodable input record is counted, not transport-fatal (§6.2).
            let input = read_lane(&connection, input, INPUT_QUEUE, "elevated-input-read", None, true, Ok)?;
            // An undecodable context record stays a protocol failure: context
            // records carry per-producer positions, and a skipped record would
            // break the position continuity that ingress verifies.
            let observation = read_lane(&connection, observation, OBSERVATION_QUEUE, "elevated-context-read", None, true,
                |record| Ok((clock::qpc().map_err(|_| Failure::Supervisor)?, record.ok_or(Failure::Protocol)?)))?;
            spawn(&connection, "elevated-control-write", move || {
                command.send(&Control::Configure { approved_count, pen_hid }, until)?;
                for key in keys {
                    command.send(&Control::Approval(key), until)?;
                }
                command.send(&Control::SealConfiguration, until)?;
                for batch in command_queue {
                    command.send(&batch.first, batch.until)?;
                    if let Some(next) = batch.then {
                        command.send(&next, batch.until)?;
                    }
                }
                Ok(())
            })?;
            spawn(&connection, "elevated-context-write", move || {
                for (request, until) in query_queue {
                    taken_queries.fetch_sub(1, Ordering::AcqRel);
                    query.send(&request, until)?;
                }
                Ok(())
            })?;
            Ok(Self { connection, command: commands, query: queries, queued_queries, status, input, observation })
        })();
        if result.is_err() {
            cancel.cancel();
        }
        result
    }

    pub(super) fn send(&self, command: Control) -> Result<()> {
        self.send_commands(command, None)
    }

    /// A transition must reference the renewal immediately preceding it.
    pub(super) fn after_renewal(&self, renewal: Control, command: Control) -> Result<()> {
        self.send_commands(renewal, Some(command))
    }

    fn send_commands(&self, first: Control, then: Option<Control>) -> Result<()> {
        self.connection.check()?;
        let batch = Commands { first, then, until: Instant::now() + WRITE_BUDGET };
        self.command.try_send(batch).map_err(|_| Failure::Transport)
    }

    /// Queue a required query; a full queue fails the connection.
    pub(super) fn query(&self, request: ContextCommand) -> Result<()> {
        self.connection.check()?;
        self.send_query(request).map_err(|_| Failure::Transport)
    }

    /// Queue an optional query outside the slots reserved for required ones.
    /// Returns false when there is no room for it now.
    pub(super) fn try_query(&self, request: ContextCommand) -> Result<bool> {
        self.connection.check()?;
        if self.queued_queries.load(Ordering::Acquire) >= QUERY_QUEUE - REQUIRED_QUERY_SLOTS {
            return Ok(false);
        }
        match self.send_query(request) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(Failure::Transport),
        }
    }

    /// Count the query in before the writer can take it out.
    fn send_query(&self, request: ContextCommand) -> std::result::Result<(), TrySendError<(ContextCommand, Instant)>> {
        self.queued_queries.fetch_add(1, Ordering::AcqRel);
        self.query.try_send((request, Instant::now() + WRITE_BUDGET))
            .inspect_err(|_| { self.queued_queries.fetch_sub(1, Ordering::AcqRel); })
    }

    pub(super) fn status(&self) -> Result<Option<Status>> { poll(&self.status, &self.connection) }

    /// No lane failed and the connection was not cancelled.
    pub(super) fn live(&self) -> bool { self.connection.check().is_ok() }

    pub(super) fn input(&self) -> Result<Option<Option<GrantedInput>>> { poll(&self.input, &self.connection) }

    pub(super) fn observation(&self) -> Result<Option<(QpcTick, Observation)>> { poll(&self.observation, &self.connection) }
}

impl Drop for Peer {
    fn drop(&mut self) { self.connection.cancel.cancel(); }
}

fn spawn(
    connection: &Arc<Connection>,
    name: &'static str,
    work: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    let connection = Arc::clone(connection);
    thread::Builder::new().name(name.into()).spawn(move || {
        if let Err(failure) = work() {
            connection.record(failure);
        }
    }).map(|_| ()).map_err(|_| Failure::Supervisor)
}

fn read_lane<T: Record + Send + 'static, U: Send + 'static>(
    connection: &Arc<Connection>, mut source: Receiver<T>, capacity: usize, name: &'static str,
    mut first_deadline: Option<Instant>, backpressure: bool, received: impl Fn(Option<T>) -> Result<U> + Send + 'static,
) -> Result<Queue<U>> {
    let (send, receive) = mpsc::sync_channel(capacity);
    let live = Arc::clone(connection);
    spawn(connection, name, move || loop {
        // An idle lane is valid. Connection cancellation, peer exit and the
        // controller's stage/coverage deadlines remain independently active.
        let deadline = first_deadline.take().unwrap_or_else(|| Instant::now() + IDLE_READ);
        let mut record = received(source.receive_lossy(deadline)?)?;
        loop {
            match send.try_send(record) {
                Ok(()) => break,
                Err(TrySendError::Full(pending)) if backpressure => {
                    // One blocked data owner holds only its current record.
                    // The high producer remains responsible for ordered loss.
                    live.check()?;
                    record = pending;
                    thread::park_timeout(Duration::from_millis(2));
                }
                Err(_) => return Err(Failure::Transport),
            }
        }
    })?;
    Ok(receive)
}

fn poll<T>(queue: &Queue<T>, connection: &Connection) -> Result<Option<T>> {
    match queue.try_recv() {
        Ok(value) => Ok(Some(value)),
        Err(TryRecvError::Empty) => connection.check().map(|()| None),
        Err(TryRecvError::Disconnected) => connection.check().and(Err(Failure::Transport)),
    }
}
