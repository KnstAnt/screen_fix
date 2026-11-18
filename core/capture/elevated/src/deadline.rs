use std::{ptr::null, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};

use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject},
};

use crate::{process::Process, win::{Failure, Handle, Result}};

pub const READINESS: Duration = Duration::from_secs(5);
pub(crate) const CLEANUP: Duration = Duration::from_millis(750);
const POLL: Duration = Duration::from_millis(25);

pub struct Cancellation {
    signalled: AtomicBool,
    event: Handle,
}

impl Cancellation {
    pub fn new() -> Result<Self> { Ok(Self { signalled: AtomicBool::new(false), event: event()? }) }

    pub fn cancel(&self) {
        // The manual-reset event remains owned for all concurrent waiters.
        self.signalled.store(true, Ordering::Release);
        unsafe { SetEvent(self.event.raw()) };
    }

    pub fn check(&self) -> Result<()> {
        if self.signalled.load(Ordering::Acquire) {
            return Err(Failure::Cancelled);
        }
        Ok(())
    }

    pub(crate) fn handle(&self) -> &Handle { &self.event }

    /// Block for up to `timeout`, returning early once cancelled.
    pub(crate) fn wait(&self, timeout: Duration) {
        unsafe { WaitForSingleObject(self.event.raw(), milliseconds(timeout)) };
    }
}

pub(crate) fn event() -> Result<Handle> {
    unsafe { Handle::take(CreateEventW(null(), 1, 0, null()), Failure::Supervisor) }
}

pub(crate) fn milliseconds(duration: Duration) -> u32 {
    duration.as_nanos().div_ceil(1_000_000).min(u128::from(u32::MAX - 1)) as u32
}

#[derive(Clone, Copy)]
pub(crate) struct Deadline<'a> {
    at: Instant,
    /// The failure reported when `at` passes; each bound names its own stage.
    expired: Failure,
    cancellation: &'a Cancellation,
    scope: Option<Scope<'a>>,
    peer: Option<&'a Process>,
}

/// The grant scope of an operation: its cancellation and a polled liveness
/// predicate.
#[derive(Clone, Copy)]
struct Scope<'a> {
    cancel: &'a Cancellation,
    current: &'a dyn Fn() -> bool,
}

impl<'a> Deadline<'a> {
    pub(crate) fn new(at: Instant, expired: Failure, cancellation: &'a Cancellation) -> Self {
        Self { at, expired, cancellation, scope: None, peer: None }
    }

    /// Also fails once `peer` exits.
    pub(crate) fn peer(self, peer: &'a Process) -> Self {
        Self { peer: Some(peer), ..self }
    }

    /// Also fails with `ScopeClosed` once `cancel` is cancelled or `current`
    /// returns false.
    pub(crate) fn scoped(self, cancel: &'a Cancellation, current: &'a dyn Fn() -> bool) -> Self {
        Self { scope: Some(Scope { cancel, current }), ..self }
    }

    pub(crate) fn has_scope(self) -> bool { self.scope.is_some() }

    pub(crate) fn check(self) -> Result<()> {
        self.cancellation.check()?;
        if self.scope.is_some_and(|scope| scope.cancel.check().is_err() || !(scope.current)()) {
            return Err(Failure::ScopeClosed);
        }
        if let Some(peer) = self.peer {
            peer.ensure_live()?;
        }
        if Instant::now() >= self.at {
            return Err(self.expired);
        }
        Ok(())
    }

    pub(crate) fn wait(self, completed: &Handle) -> Result<()> {
        self.wait_any(&[completed]).map(|_| ())
    }

    pub(crate) fn wait_any(self, completed: &[&Handle]) -> Result<usize> {
        let mut handles = Vec::with_capacity(3 + completed.len());
        handles.push(self.cancellation.event.raw());
        let peer_wait = self.peer.map(|peer| {
            let index = handles.len() as u32;
            handles.push(peer.raw());
            WAIT_OBJECT_0 + index
        });
        let grant_wait = self.scope.map(|scope| {
            let index = handles.len() as u32;
            handles.push(scope.cancel.event.raw());
            WAIT_OBJECT_0 + index
        });
        let first = handles.len() as u32;
        handles.extend(completed.iter().map(|event| event.raw()));
        let limit = handles.len() as u32;
        loop {
            self.check()?;
            // Every other condition is a waited handle; only the scope
            // predicate has to be polled.
            let remaining = self.at.saturating_duration_since(Instant::now());
            let remaining = if self.scope.is_some() { remaining.min(POLL) } else { remaining };
            let outcome = unsafe { WaitForMultipleObjects(limit, handles.as_ptr(), 0, milliseconds(remaining)) };
            match outcome {
                WAIT_OBJECT_0 => return Err(Failure::Cancelled),
                index if Some(index) == peer_wait => return Err(Failure::PeerExited),
                index if Some(index) == grant_wait => return Err(Failure::ScopeClosed),
                WAIT_TIMEOUT => {}
                index if (WAIT_OBJECT_0 + first..WAIT_OBJECT_0 + limit).contains(&index) => {
                    self.check()?;
                    return Ok((index - WAIT_OBJECT_0 - first) as usize);
                }
                _ => return Err(Failure::Transport),
            }
        }
    }

    pub(crate) fn pause(self) -> Result<()> {
        self.check()?;
        self.cancellation.wait(self.at.saturating_duration_since(Instant::now()).min(POLL));
        self.check()
    }
}
