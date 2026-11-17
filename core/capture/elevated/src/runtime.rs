pub(crate) mod lifetime;
mod context;
mod grant;
mod input;
mod notices;
mod outbox;
mod owner;
mod witness;

pub(crate) use owner::run;
pub use lifetime::HEARTBEAT;
pub use witness::normal_capture_desktop;

use std::{marker::PhantomData, sync::Arc, time::Instant};

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

    pub(crate) fn recorder(self) -> RecorderLanes {
        RecorderLanes {
            scope: self.connection.scope.clone(),
            cancel: Arc::clone(&self.connection.stop),
            command: Sender::new(&self.control, &self.connection),
            status: Receiver::new(&self.control, &self.connection),
            input: Receiver::new(&self.input, &self.connection),
            query: Sender::new(&self.context, &self.connection),
            observation: Receiver::new(&self.context, &self.connection),
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
    record: PhantomData<T>,
}

impl<T: Record> Lane<T> {
    fn new(pipe: &Arc<Pipe>, connection: &Arc<Connection>) -> Self {
        Self {
            pipe: Arc::clone(pipe),
            connection: Arc::clone(connection),
            next: T::FIRST_SEQUENCE,
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
    fn new(pipe: &Arc<Pipe>, connection: &Arc<Connection>) -> Self {
        Self(Lane::new(pipe, connection))
    }

    pub fn send(&mut self, value: &T, until: Instant) -> Result<()> {
        self.send_inner(value, until, None, None).map(|_| ())
    }

    fn send_scoped(&mut self, value: &T, until: Instant, grant: &grant::Grant, visit: VisitBinding) -> Result<bool> {
        let Some(cancel) = grant.evidence_cancel(visit) else { return Ok(false); };
        let live = || grant.witnessed_at(visit).is_some()
            && grant.evidence_cancel(visit).is_some() && grant.check(false).is_ok();
        self.send_inner(value, until, Some(&cancel), Some(&live))
    }

    fn send_inner(
        &mut self, value: &T, until: Instant,
        grant: Option<&Cancellation>, scope_current: Option<&dyn Fn() -> bool>,
    ) -> Result<bool> {
        let lane = &mut self.0;
        let connection = &lane.connection;
        let deadline = Deadline { at: until, cancellation: &connection.stop, grant, scope_current, peer: Some(&connection.peer) };
        let encoded = runtime::encode(value, &connection.scope, connection.id, lane.next, &mut lane.frame)
            .map_err(|_| Failure::Protocol);
        let len = match encoded {
            Ok(len) => len,
            Err(error) => return lane.complete(Err(error)),
        };
        if grant.is_some_and(|grant| grant.check().is_err()) {
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
    fn new(pipe: &Arc<Pipe>, connection: &Arc<Connection>) -> Self {
        Self(Lane::new(pipe, connection))
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
        let deadline = Deadline { at: until, cancellation: &connection.stop, grant: None, scope_current: None, peer: Some(&connection.peer) };
        let result = (|| {
            let len = if lane.pipe.is_server() {
                lane.pipe.read_message(&mut lane.frame, deadline)?
            } else {
                lane.pipe.read_exact(&mut lane.frame[..HEADER_BYTES], deadline)?;
                let payload = runtime::payload_len::<T>(&lane.frame[..HEADER_BYTES], connection.id, lane.next)
                    .map_err(|_| Failure::Protocol)?;
                let end = HEADER_BYTES + payload;
                lane.pipe.read_exact(&mut lane.frame[HEADER_BYTES..end], deadline)?;
                end
            };
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
