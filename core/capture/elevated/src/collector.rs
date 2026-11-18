use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use screen_fix_protocol::ipc::elevated::attachment::{Role, Transition};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        GetCurrentProcess, SetEvent, TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
    },
};

use crate::{
    deadline::{CLEANUP, Cancellation, Deadline, READINESS, event, milliseconds},
    pipe::{self, Pipe},
    runtime::{self, Links, lifetime::{Lifetime, qpc}},
    process::{Process, verify_linked_pair},
    trust::{ImageRole, Installation},
    win::{Failure, Handle, Result, restrict_dll_search},
    wire::{Bootstrap, LaunchArguments, frequency},
};

const SUPERVISOR_POLL: Duration = Duration::from_millis(25);

pub fn run_collector() -> Result<()> {
    let until = Instant::now() + READINESS;
    let supervisor = Supervisor::start(until)?;
    let result = supervise_entry(&supervisor, until);
    // An unresolved kernel I/O completion can still own a frame buffer.
    // Keep the independent process deadline in force through that drain.
    if pipe::available().is_err() {
        supervisor.shared.cancel.cancel();
        while pipe::available().is_err() {
            std::thread::park_timeout(SUPERVISOR_POLL);
        }
    }
    result
}

fn supervise_entry(supervisor: &Supervisor, until: Instant) -> Result<()> {
    restrict_dll_search()?;
    pipe::available()?;
    let args = LaunchArguments::parse(std::env::args_os().skip(1))?;
    let parent_handle = Process::open_handle(args.parent.pid.get())?;
    supervisor.shared.parent.set(Arc::clone(&parent_handle)).map_err(|_| Failure::Supervisor)?;
    let parent = Process::inspect(parent_handle)?;
    if parent.instance() != args.parent { return Err(Failure::ProcessIdentity) }
    let deadline = Deadline {
        at: until, expired: Failure::ReadinessDeadline, cancellation: &supervisor.shared.cancel, grant: None,
        scope_current: None, peer: Some(&parent),
    };
    deadline.check()?;

    let current = Process::current()?;
    let installation = Installation::verify(ImageRole::Collector)?;
    installation.verify_collector(&current)?;
    installation.verify_recorder(&parent)?;
    let user = verify_linked_pair(&parent, &current)?;
    deadline.check()?;

    let mut pipe = Pipe::client(&args.bootstrap(), deadline)?;
    let bootstrap = Bootstrap::receive(&mut pipe, deadline)?;
    drop(pipe);
    drop(args);
    if bootstrap.scope.release != installation.release.id || bootstrap.scope.qpc_frequency != frequency()? {
        return Err(Failure::Protocol);
    }
    let mut pipes = Vec::with_capacity(Role::ORDER.len());
    for role in Role::ORDER {
        deadline.check()?;
        pipes.push(Pipe::collector(&bootstrap.name(role)?, &user)?);
    }
    for (role, pipe) in Role::ORDER.into_iter().zip(&mut pipes) {
        pipe.accept(&parent, deadline)?;
        bootstrap.bind_server(pipe, role, deadline)?;
    }
    bootstrap.receive_seal(&mut pipes[0], Transition::Seal, deadline)?;
    bootstrap.send_seal(&mut pipes[0], Transition::Authenticated, deadline)?;
    let scope = bootstrap.scope.clone();
    let connection = bootstrap.connection;
    drop(bootstrap);
    deadline.check()?;

    let lifetime = Arc::new(Lifetime::new(scope.qpc_frequency, until)?);
    supervisor.shared.runtime.set(Arc::clone(&lifetime)).map_err(|_| Failure::Supervisor)?;
    let pipes = pipes.try_into().map_err(|_| Failure::Protocol)?;
    let links = Links::new(pipes, scope, connection, Arc::new(parent), Arc::clone(&supervisor.shared.cancel));
    runtime::run(links, &lifetime, until)
}

struct Shared {
    cancel: Arc<Cancellation>,
    done: Handle,
    parent: OnceLock<Arc<Handle>>,
    runtime: OnceLock<Arc<Lifetime>>,
}

struct Supervisor {
    shared: Arc<Shared>,
}

impl Supervisor {
    fn start(until: Instant) -> Result<Self> {
        let shared = Arc::new(Shared {
            cancel: Arc::new(Cancellation::new()?), done: event()?, parent: OnceLock::new(), runtime: OnceLock::new(),
        });
        let detector = Arc::clone(&shared);
        std::thread::Builder::new().name("elevated-supervisor".into())
            .spawn(move || watch(&detector, until)).map_err(|_| Failure::Supervisor)?;
        Ok(Self { shared })
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) { unsafe { SetEvent(self.shared.done.raw()) }; }
}

fn watch(shared: &Shared, boot_until: Instant) {
    let mut last_qpc = None;
    loop {
        if shared.cancel.check().is_err() { break }
        let remaining = if let Some(lifetime) = shared.runtime.get() {
            let healthy = qpc().is_ok_and(|now| {
                if last_qpc.is_some_and(|previous| previous > now) {
                    return false;
                }
                last_qpc = Some(now);
                if lifetime.cleanup_expired(now) { terminate() }
                !lifetime.provider_expired(now) && lifetime.witness_healthy(now) && lifetime.owner_healthy(now)
                    && lifetime.check(now).is_ok()
            });
            if !healthy {
                lifetime.stop();
                shared.cancel.cancel();
                break;
            }
            SUPERVISOR_POLL
        } else {
            let remaining = boot_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() { terminate() }
            remaining
        };
        let parent = shared.parent.get().map(Arc::as_ref);
        let handles = [shared.done.raw(), shared.cancel.handle().raw(), parent.map_or(std::ptr::null_mut(), Handle::raw)];
        let count = if parent.is_some() { 3 } else { 2 };
        let status = unsafe {
            WaitForMultipleObjects(count, handles.as_ptr(), 0, milliseconds(remaining.min(SUPERVISOR_POLL)))
        };
        match status {
            WAIT_OBJECT_0 => return,
            WAIT_TIMEOUT => {}
            index if index == WAIT_OBJECT_0 + 1 || index == WAIT_OBJECT_0 + 2 => {
                shared.cancel.cancel();
                break;
            }
            _ => terminate(),
        }
    }
    let runtime = shared.runtime.get();
    let cleaned_by = Instant::now() + if let Some(lifetime) = runtime {
        lifetime.stop();
        CLEANUP
    } else {
        CLEANUP.min(boot_until.saturating_duration_since(Instant::now()))
    };
    loop {
        let remaining = cleaned_by.saturating_duration_since(Instant::now());
        if remaining.is_zero() || runtime.is_some_and(|lifetime| {
            qpc().map_or(true, |now| lifetime.cleanup_expired(now))
        }) {
            terminate();
        }
        match unsafe { WaitForSingleObject(shared.done.raw(), milliseconds(remaining.min(SUPERVISOR_POLL))) } {
            WAIT_OBJECT_0 => return,
            WAIT_TIMEOUT => {}
            _ => terminate(),
        }
    }
}

fn terminate() -> ! {
    // The supervisor uses the process's own pseudo-handle. Bypass teardown
    // that could be blocked by the operation whose lifetime has expired.
    unsafe { TerminateProcess(GetCurrentProcess(), 1) };
    std::process::abort();
}
