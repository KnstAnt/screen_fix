use std::{
    ffi::OsStr,
    mem::size_of,
    os::windows::io::{FromRawHandle, OwnedHandle},
    sync::{
        Arc, Mutex, MutexGuard, mpsc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use screen_fix_elevated::{Authenticated, Cancellation, Failure, LaunchRequest, RecorderBootstrap, Result};
use screen_fix_native_observation::{StaticLease, com::ComApartment};
use screen_fix_protocol::{AttemptId, SessionId};
use crate::platform::windows::controller::SourceDispatch;
use super::{super::wide, visit::matches_target};

use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::{
    Foundation::{ERROR_CANCELLED, GetLastError, INVALID_HANDLE_VALUE},
    UI::{
        Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

const CONSENT: Duration = Duration::from_secs(60);
const READINESS: Duration = screen_fix_elevated::READINESS;
const POLL: Duration = Duration::from_millis(25);

static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
enum Phase {
    Consent(Instant),
    Binding(Instant),
}

impl Phase {
    fn check(self, now: Instant) -> Result<()> {
        match self {
            Self::Consent(at) if now >= at => Err(Failure::ConsentDeadline),
            Self::Binding(at) if now >= at => Err(Failure::ReadinessDeadline),
            _ => Ok(()),
        }
    }
}

enum State {
    Pending(Phase),
    Complete(Phase, Result<Authenticated>),
    Taken,
}

const BEFORE_DISPATCH: u8 = 0;
const DISPATCHED: u8 = 1;
const CANCELLED: u8 = 2;

struct Permit {
    cancellation: Cancellation,
    dispatch: AtomicU8,
    consent_until: Instant,
}

impl Permit {
    fn cancel(&self) {
        self.dispatch.store(CANCELLED, Ordering::Release);
        self.cancellation.cancel();
    }

    fn begin_dispatch(&self) -> Result<()> {
        Phase::Consent(self.consent_until).check(Instant::now())?;
        self.dispatch.compare_exchange(BEFORE_DISPATCH, DISPATCHED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Failure::Cancelled)?;
        Phase::Consent(self.consent_until).check(Instant::now())?;
        self.cancellation.check()
    }
}

struct Shared {
    permit: Arc<Permit>,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn enter_binding(&self, returned: Instant) -> Result<()> {
        Phase::Consent(self.permit.consent_until).check(returned)?;
        let mut state = self.lock();
        if !matches!(*state, State::Pending(Phase::Consent(_))) {
            return Err(Failure::Cancelled);
        }
        self.permit.cancellation.check()?;
        let phase = Phase::Binding(returned + READINESS);
        phase.check(Instant::now())?;
        *state = State::Pending(phase);
        Ok(())
    }

    fn publish(&self, result: Result<Authenticated>) {
        let mut state = self.lock();
        if let State::Pending(phase) = *state {
            if self.permit.cancellation.check().is_ok() {
                let result = result.and_then(|binding| phase.check(Instant::now()).map(|_| binding));
                *state = State::Complete(phase, result);
            }
        }
    }
}

pub(super) enum LaunchStatus {
    AwaitingConsent,
    Authenticating,
    Complete { authenticating: bool, result: Result<(Authenticated, Instant)> },
}

pub(super) struct Ticket {
    shared: Arc<Shared>,
}

impl Ticket {
    /// A controller must close capture admission before asking Windows for consent.
    pub(super) fn start(attempt: AttemptId, session: SessionId, source: SourceDispatch) -> Result<Self> {
        let flight = Arc::new(StaticLease::claim(&IN_FLIGHT).ok_or(Failure::Busy)?);
        let consent_until = Instant::now() + CONSENT;
        let shared = Arc::new(Shared {
            permit: Arc::new(Permit {
                cancellation: Cancellation::new()?,
                dispatch: AtomicU8::new(BEFORE_DISPATCH),
                consent_until,
            }),
            state: Mutex::new(State::Pending(Phase::Consent(consent_until))),
        });
        let owner = Arc::clone(&shared);
        thread::Builder::new().name("elevated-attachment".into()).spawn(move || {
            let result = attach(&owner, &flight, attempt, session, source);
            owner.publish(result);
        }).map_err(|_| Failure::Supervisor)?;
        Ok(Self { shared })
    }

    pub(super) fn poll(&mut self) -> LaunchStatus {
        let mut state = self.shared.lock();
        match *state {
            State::Pending(phase) => match phase.check(Instant::now()) {
                Ok(()) => match phase {
                    Phase::Consent(_) => LaunchStatus::AwaitingConsent,
                    Phase::Binding(_) => LaunchStatus::Authenticating,
                },
                Err(failure) => {
                    self.shared.permit.cancel();
                    *state = State::Taken;
                    LaunchStatus::Complete { authenticating: matches!(phase, Phase::Binding(_)), result: Err(failure) }
                }
            },
            State::Taken => LaunchStatus::Complete { authenticating: false, result: Err(Failure::Cancelled) },
            State::Complete(_, _) => {
                let State::Complete(phase, result) = std::mem::replace(&mut *state, State::Taken) else {
                    unreachable!("checked the completed attachment state");
                };
                drop(state);
                let result = result.and_then(|binding| {
                    phase.check(Instant::now())?;
                    binding.ensure_live()?;
                    let Phase::Binding(until) = phase else { return Err(Failure::Protocol); };
                    Ok((binding, until))
                });
                LaunchStatus::Complete { authenticating: matches!(phase, Phase::Binding(_)), result }
            }
        }
    }

    fn cancel(&mut self) {
        self.shared.permit.cancel();
        let prior = std::mem::replace(&mut *self.shared.lock(), State::Taken);
        drop(prior);
    }

    pub(super) fn withdraw_undispatched(&self) -> bool {
        let permit = &self.shared.permit;
        if permit.dispatch.compare_exchange(BEFORE_DISPATCH, CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            permit.cancellation.cancel();
            true
        } else {
            false
        }
    }

    pub(super) fn was_dispatched(&self) -> bool {
        self.shared.permit.dispatch.load(Ordering::Acquire) == DISPATCHED
    }
}

impl Drop for Ticket {
    fn drop(&mut self) { self.cancel(); }
}

fn attach(shared: &Shared, flight: &Arc<StaticLease>, attempt: AttemptId, session: SessionId, source: SourceDispatch) -> Result<Authenticated> {
    let permit = &shared.permit;
    source.with_closed_phase(|| permit.cancellation.check())?;
    let (bootstrap, request) = RecorderBootstrap::prepare(attempt, session, &permit.cancellation, permit.consent_until)?;
    let (sender, receiver) = mpsc::sync_channel(1);

    // The nested shell worker only keeps the fixed launch request, its guards,
    // the target proof, dispatch permit and one-flight latch. It never owns a pipe or
    // an attachment. A consent dialog can outlive this preparation owner.
    let shell_flight = Arc::clone(flight);
    let shell_permit = Arc::clone(permit);
    let shell_source = Arc::new(source);
    let source = Arc::clone(&shell_source);
    thread::Builder::new().name("elevated-consent".into()).spawn(move || {
        match ComApartment::initialize(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) {
            Ok(apartment) => {
                let outcome = shell(request, &shell_permit, &shell_source);
                let _ = sender.send(outcome);
                drop(shell_flight);
                drop(apartment);
            }
            Err(_) => {
                let _ = sender.send(Err(Failure::ConsentLaunch));
            }
        }
    }).map_err(|_| Failure::Supervisor)?;

    let (child, returned) = loop {
        if source.stopped() { return Err(Failure::Cancelled); }
        permit.cancellation.check()?;
        if let Ok(outcome) = receiver.try_recv() {
            break outcome?;
        }
        Phase::Consent(permit.consent_until).check(Instant::now())?;
        let remaining = permit.consent_until.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(POLL)) {
            Ok(outcome) => break outcome?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Failure::ConsentLaunch),
        }
    };
    shared.enter_binding(returned)?;
    bootstrap.authenticate(child, returned, &permit.cancellation)
}

fn shell(request: LaunchRequest, permit: &Permit, source: &SourceDispatch) -> Result<(OwnedHandle, Instant)> {
    let exe = wide(request.executable().as_os_str());
    let cwd = wide(request.directory().as_os_str());
    let verb = wide(OsStr::new("runas"));
    let arguments = request.arguments();
    let arguments = wide(OsStr::new(&arguments));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: exe.as_ptr(),
        lpParameters: arguments.as_ptr(),
        lpDirectory: cwd.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    source.with_closed_phase(|| permit.cancellation.check())?;
    let target_current = matches_target(source.target());
    source.with_closed_phase(|| {
        permit.cancellation.check()?;
        if !target_current? { return Err(Failure::TargetChanged); }
        permit.begin_dispatch()
    })?;
    let success = unsafe { ShellExecuteExW(&mut info) } != 0;
    let error = if success { 0 } else { unsafe { GetLastError() } };
    let returned = Instant::now();
    let handle = if info.hProcess.is_null() || info.hProcess == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(unsafe { OwnedHandle::from_raw_handle(info.hProcess) })
    };
    // COM teardown can execute external providers. The caller observes the
    // actual shell return timestamp and does not wait for this apartment.
    let result = if success {
        handle.map(|handle| (handle, returned)).ok_or(Failure::ConsentLaunch)
    } else if error == ERROR_CANCELLED {
        Err(Failure::ConsentDenied)
    } else {
        Err(Failure::ConsentLaunch)
    };
    // The shell worker forwards this value before uninitializing its apartment.
    result
}
