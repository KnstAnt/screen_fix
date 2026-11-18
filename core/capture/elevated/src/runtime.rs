pub(crate) mod lifetime;
mod context;
mod egress;
mod grant;
mod input;
mod notices;
mod outbox;
mod owner;
mod timing;
mod witness;

pub(crate) use owner::run;
pub use timing::HEARTBEAT;
pub use witness::normal_capture_desktop;

use std::{marker::PhantomData, sync::{Arc, atomic::{AtomicU8, Ordering::{AcqRel, Acquire, Release}}}, time::Instant};

use screen_fix_protocol::ipc::{PacketError, elevated::{
    GrantedInput, VisitBinding,
    attachment::{ConnectionId, HEADER_BYTES, Scope},
    runtime::{self, ContextCommand, Control, Observation, Record, Status},
}};

use crate::{
    deadline::{Cancellation, Deadline},
    pipe::Pipe,
    process::Process,
    secret::erase,
    win::{Failure, Result},
};

/// A `repr(u8)` state machine whose value lives in an atomic.
trait State: Copy + PartialEq + 'static {
    const ALL: &'static [Self];
    fn raw(self) -> u8;
}

struct AtomicState<S>(AtomicU8, PhantomData<S>);

impl<S: State> AtomicState<S> {
    fn new(state: S) -> Self { Self(AtomicU8::new(state.raw()), PhantomData) }

    fn load(&self) -> S {
        let raw = self.0.load(Acquire);
        // Only `S` values are ever stored.
        S::ALL.iter().copied().find(|state| state.raw() == raw).unwrap_or(S::ALL[0])
    }

    fn store(&self, state: S) { self.0.store(state.raw(), Release); }

    fn transition(&self, from: S, to: S) -> bool {
        self.0.compare_exchange(from.raw(), to.raw(), AcqRel, Acquire).is_ok()
    }
}

struct Connection {
    scope: Scope,
    id: ConnectionId,
    peer: Arc<Process>,
    stop: Arc<Cancellation>,
}

pub(crate) struct Links {
    control: Arc<Pipe>,
    input: Arc<Pipe>,
    context: Arc<Pipe>,
    connection: Arc<Connection>,
}

impl Links {
    pub(crate) fn new(
        [control, input, context]: [Pipe; 3], scope: Scope, id: ConnectionId, peer: Arc<Process>,
        stop: Arc<Cancellation>,
    ) -> Self {
        Self {
            control: Arc::new(control),
            input: Arc::new(input),
            context: Arc::new(context),
            connection: Arc::new(Connection { scope, id, peer, stop }),
        }
    }

    /// A stalled Recorder write is a transport failure. Its only finite read
    /// bound is the readiness deadline of the first collector status.
    pub(crate) fn recorder(self) -> RecorderLanes {
        RecorderLanes {
            scope: self.connection.scope.clone(),
            cancel: Arc::clone(&self.connection.stop),
            command: Sender::new(&self.control, &self.connection, Failure::Transport),
            status: Receiver::new(&self.control, &self.connection, Failure::ReadinessDeadline),
            input: Receiver::new(&self.input, &self.connection, Failure::Transport),
            query: Sender::new(&self.context, &self.connection, Failure::Transport),
            observation: Receiver::new(&self.context, &self.connection, Failure::Transport),
        }
    }
}

/// Only an authenticated launch can produce these typed Recorder endpoints.
/// A transport error in any direction cancels the entire connection.
pub struct RecorderLanes {
    pub scope: Scope,
    pub cancel: Arc<Cancellation>,
    pub command: Sender<Control>,
    pub status: Receiver<Status>,
    pub input: Receiver<GrantedInput>,
    pub query: Sender<ContextCommand>,
    pub observation: Receiver<Observation>,
}

struct Lane<T> {
    pipe: Arc<Pipe>,
    connection: Arc<Connection>,
    next: u64,
    frame: Vec<u8>,
    /// The failure of an operation still incomplete at its deadline.
    expired: Failure,
    record: PhantomData<T>,
}

impl<T: Record> Lane<T> {
    fn new(pipe: &Arc<Pipe>, connection: &Arc<Connection>, expired: Failure) -> Self {
        Self {
            pipe: Arc::clone(pipe),
            connection: Arc::clone(connection),
            next: T::FIRST_SEQUENCE,
            expired,
            frame: vec![0; runtime::capacity::<T>()],
            record: PhantomData,
        }
    }

    fn complete<R>(&mut self, result: Result<R>) -> Result<R> {
        erase(&mut self.frame);
        if result.is_ok() {
            self.next += 1;
        } else {
            // A caller must never resume after a partial operation or
            // reinterpret a later packet as the failed packet.
            self.connection.stop.cancel();
        }
        result
    }
}

impl<T> Drop for Lane<T> {
    fn drop(&mut self) {
        self.connection.stop.cancel();
        erase(&mut self.frame);
    }
}

pub struct Sender<T>(Lane<T>);

impl<T: Record> Sender<T> {
    fn new(pipe: &Arc<Pipe>, connection: &Arc<Connection>, expired: Failure) -> Self {
        Self(Lane::new(pipe, connection, expired))
    }

    pub fn send(&mut self, value: &T, until: Instant) -> Result<()> {
        self.send_inner(value, until, None).map(|_| ())
    }

    fn send_scoped(&mut self, value: &T, until: Instant, grant: &grant::Grant, visit: VisitBinding) -> Result<bool> {
        let Some(cancel) = grant.evidence_cancel(visit) else { return Ok(false); };
        let live = || grant.witnessed_at(visit).is_some()
            && grant.evidence_cancel(visit).is_some() && grant.check(false).is_ok();
        let scope: (&Cancellation, &dyn Fn() -> bool) = (&cancel, &live);
        self.send_inner(value, until, Some(scope))
    }

    /// `scope` is the grant cancellation and liveness predicate of a scoped
    /// write.
    fn send_inner(
        &mut self, value: &T, until: Instant, scope: Option<(&Cancellation, &dyn Fn() -> bool)>,
    ) -> Result<bool> {
        let lane = &mut self.0;
        let connection = &lane.connection;
        let deadline = Deadline::new(until, lane.expired, &connection.stop).peer(&connection.peer);
        let deadline = match scope {
            Some((cancel, current)) => deadline.scoped(cancel, current),
            None => deadline,
        };
        let encoded = runtime::encode(value, &connection.scope, connection.id, lane.next, &mut lane.frame)
            .map_err(|_| Failure::Protocol);
        let len = match encoded {
            Ok(len) => len,
            Err(error) => return lane.complete(Err(error)),
        };
        if scope.is_some_and(|(cancel, _)| cancel.check().is_err()) {
            erase(&mut lane.frame);
            return Ok(false);
        }
        match lane.pipe.write(&lane.frame[..len], deadline) {
            Err(Failure::ScopeClosed) => {
                erase(&mut lane.frame);
                Ok(false)
            }
            result => lane.complete(result).map(|()| true),
        }
    }
}

pub struct Receiver<T>(Lane<T>);

impl<T: Record> Receiver<T> {
    fn new(pipe: &Arc<Pipe>, connection: &Arc<Connection>, expired: Failure) -> Self {
        Self(Lane::new(pipe, connection, expired))
    }

    pub fn receive(&mut self, until: Instant) -> Result<T> {
        let record = self.receive_lossy(until)?;
        record.ok_or_else(|| {
            self.0.connection.stop.cancel();
            Failure::Protocol
        })
    }

    /// Like `receive`, but a frame with valid framing whose payload does not
    /// decode is consumed and reported as `None` (E3, §6.2). Corrupt framing
    /// remains a transport failure.
    pub fn receive_lossy(&mut self, until: Instant) -> Result<Option<T>> {
        let lane = &mut self.0;
        let connection = &lane.connection;
        let deadline = Deadline::new(until, lane.expired, &connection.stop).peer(&connection.peer);
        let result = (|| {
            let (id, next) = (connection.id, lane.next);
            let len = lane.pipe.read_frame(&mut lane.frame, HEADER_BYTES, |header| {
                runtime::payload_len::<T>(header, id, next).map_err(|_| Failure::Protocol)
            }, deadline)?;
            let framed = len >= HEADER_BYTES && runtime::payload_len::<T>(&lane.frame[..HEADER_BYTES], connection.id, lane.next)
                .is_ok_and(|payload| HEADER_BYTES + payload == len);
            if !framed { return Err(Failure::Protocol); }
            // Only an undecodable or invalid payload is skippable; a foreign
            // scope or contradicting kind remains a protocol failure.
            match runtime::decode(&lane.frame[..len], &connection.scope, connection.id, lane.next) {
                Ok(record) => Ok(Some(record)),
                Err(PacketError::InvalidPayload) => Ok(None),
                Err(_) => Err(Failure::Protocol),
            }
        })();
        lane.complete(result)
    }
}
