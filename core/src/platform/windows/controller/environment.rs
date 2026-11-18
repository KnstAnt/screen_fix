//! Windows notifications have their own message loop. Session end can hold
//! that window procedure briefly while the controller and owners drain.

use std::{
    cell::Cell,
    ffi::c_void,
    io,
    ptr::{null, null_mut},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::{AcqRel, Acquire, Relaxed, Release}, fence},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use windows_sys::{
    Win32::{
        Foundation::{ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            Power::{
                DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, RegisterSuspendResumeNotification,
                UnregisterSuspendResumeNotification,
            },
            RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification},
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::{
                CREATESTRUCTW, CreateWindowExW, DEVICE_NOTIFY_CALLBACK, DefWindowProcW, DestroyWindow, DispatchMessageW,
                EVENT_SYSTEM_DESKTOPSWITCH, GWLP_USERDATA, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PM_REMOVE,
                PeekMessageW, RegisterClassW, WINEVENT_OUTOFCONTEXT, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION,
                WM_NCCREATE, WM_NCDESTROY, WM_QUERYENDSESSION, WM_QUIT, WM_SETTINGCHANGE, WM_WTSSESSION_CHANGE, WNDCLASSW,
                WS_POPUP, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
            },
        },
    },
    w,
};

#[cfg(target_pointer_width = "32")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW};
#[cfg(target_pointer_width = "64")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW};

use crate::{
    CaptureError,
    protocol::{QpcFrequency, QpcTick, ipc::EnvironmentChange},
    session::PauseReason,
};

use crate::platform::windows::{clock, on_input_desktop};

const CLASS_NAME: *const u16 = w!("ScreenFixControllerNotifications");
const DESKTOP_POLL: Duration = Duration::from_millis(100);
const PUMP_INTERVAL: Duration = Duration::from_millis(10);
pub(super) const SESSION_END_BUDGET: Duration = Duration::from_millis(3800);
pub(super) const SUSPEND_BUDGET: Duration = Duration::from_secs(2);
const LOCK_SCREEN: u32 = 1;
const SLEEP: u32 = 2;
const SECURE_DESKTOP: u32 = 4;
const EDGE_SHIFT: u32 = 8;
const ASSERT_EDGES: u32 = (LOCK_SCREEN | SLEEP | SECURE_DESKTOP) << EDGE_SHIFT;

thread_local! {
    static DESKTOP_SIGNALS: Cell<*const Signals> = const { Cell::new(null()) };
}

/// A consistent snapshot of the latest suspend and resume marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SuspendMarks {
    /// The number of suspends observed so far. A reader that sees a larger
    /// value than before knows a suspend happened in between, even if it
    /// missed the marks of an earlier one.
    pub sequence: u64,
    /// QPC of the latest `PBT_APMSUSPEND`, if one was observed and QPC was readable.
    pub suspend: Option<QpcTick>,
    /// QPC of the `PBT_APMRESUMEAUTOMATIC` that followed the latest suspend.
    /// `None` while that suspend has not resumed.
    pub resume: Option<QpcTick>,
}

/// QPC time over an interval with suspended time excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwakeElapsed {
    Known(Duration),
    /// A needed suspend or resume mark has no QPC, the latest suspend has not
    /// resumed, or more than one suspend fell in the interval. The caller
    /// treats the interval as not evaluable; it never produces a fault.
    Unknown,
}

impl SuspendMarks {
    /// QPC time over `[from, to]` excluding the latest suspend interval.
    /// `since` is the suspend `sequence` read at or after `from`: a later suspend
    /// beyond the latest one leaves earlier suspends in the interval unmeasured.
    pub(crate) fn awake_elapsed(
        &self, from: QpcTick, to: QpcTick, since: u64, frequency: QpcFrequency,
    ) -> AwakeElapsed {
        let ticks = |from: QpcTick, to: QpcTick| u64::try_from(to.0.saturating_sub(from.0)).unwrap_or(0);
        let total = ticks(from, to);
        let excluded = if self.sequence == 0 {
            0
        } else if self.sequence > since.saturating_add(1) {
            return AwakeElapsed::Unknown;
        } else {
            match (self.suspend, self.resume) {
                // The latest suspend ended before the interval began.
                (_, Some(resume)) if resume <= from => 0,
                // The latest suspend began after the interval ended.
                (Some(suspend), _) if suspend >= to => 0,
                (Some(suspend), Some(resume)) => ticks(suspend.max(from), resume.min(to)),
                // Open suspend, or a mark without its QPC.
                _ => return AwakeElapsed::Unknown,
            }
        };
        AwakeElapsed::Known(frequency.duration(total.saturating_sub(excluded)))
    }

    /// QPC time from `mark` to `to` excluding the latest suspend interval.
    pub(crate) fn since(&self, mark: AwakeMark, to: QpcTick, frequency: QpcFrequency) -> AwakeElapsed {
        self.awake_elapsed(mark.qpc, to, mark.suspends, frequency)
    }
}

/// A QPC progress mark with the suspend `sequence` read at or after it: the
/// start of an interval measured with sleep excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AwakeMark {
    pub qpc: QpcTick,
    pub suspends: u64,
}

/// The suspend `sequence` last seen by a deadline that restarts after each suspend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SuspendWatch(u64);

impl SuspendWatch {
    pub(crate) fn new(marks: SuspendMarks) -> Self {
        Self(marks.sequence)
    }

    /// A suspend happened since the previous check. The watch moves to `marks`.
    pub(crate) fn slept(&mut self, marks: SuspendMarks) -> bool {
        std::mem::replace(&mut self.0, marks.sequence) != marks.sequence
    }
}

/// The only store of suspend and resume times. The power callback is its sole
/// writer; it never blocks on a reader. A sequence counter, odd while a mark
/// is written, lets readers take consistent snapshots without a lock.
#[derive(Default)]
pub(crate) struct PowerMarks {
    version: AtomicU64,
    suspends: AtomicU64,
    suspend_qpc: AtomicI64,
    suspend_known: AtomicBool,
    /// The suspend number that the resume mark follows; zero when none.
    resumed_suspend: AtomicU64,
    resume_qpc: AtomicI64,
}

impl PowerMarks {
    fn write(&self, update: impl FnOnce()) {
        // Windows delivers one registration's power notifications one at a
        // time, so this claim normally succeeds at once. It spins only across
        // another writer's few atomic stores and never waits on a reader.
        let mut version = self.version.load(Relaxed);
        loop {
            if version % 2 == 0 {
                match self.version.compare_exchange_weak(version, version + 1, Acquire, Relaxed) {
                    Ok(_) => break,
                    Err(current) => version = current,
                }
            } else {
                std::hint::spin_loop();
                version = self.version.load(Relaxed);
            }
        }
        fence(Release);
        update();
        self.version.store(version + 2, Release);
    }

    fn suspended(&self, qpc: Option<QpcTick>) {
        self.write(|| {
            self.suspend_qpc.store(qpc.map_or(0, |qpc| qpc.0), Relaxed);
            self.suspend_known.store(qpc.is_some(), Relaxed);
            self.suspends.fetch_add(1, Relaxed);
        });
    }

    fn resumed(&self, qpc: Option<QpcTick>) {
        self.write(|| {
            // A resume whose QPC could not be read leaves the suspend open.
            let suspend = if qpc.is_some() { self.suspends.load(Relaxed) } else { 0 };
            self.resume_qpc.store(qpc.map_or(0, |qpc| qpc.0), Relaxed);
            self.resumed_suspend.store(suspend, Relaxed);
        });
    }

    /// Read-only snapshot for every consumer that excludes sleep from QPC intervals.
    pub(crate) fn snapshot(&self) -> SuspendMarks {
        loop {
            let before = self.version.load(Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let sequence = self.suspends.load(Relaxed);
            let suspend = self.suspend_known.load(Relaxed).then(|| QpcTick(self.suspend_qpc.load(Relaxed)));
            let resumed = self.resumed_suspend.load(Relaxed);
            let resume = QpcTick(self.resume_qpc.load(Relaxed));
            fence(Acquire);
            if self.version.load(Relaxed) == before {
                return SuspendMarks {
                    sequence,
                    suspend: suspend.filter(|_| sequence != 0),
                    resume: (sequence != 0 && resumed == sequence).then_some(resume),
                };
            }
        }
    }
}

/// The environment facts that the notification owner publishes to the
/// controller and the recording owners.
#[derive(Default)]
pub(super) struct EnvironmentSignals {
    /// Pending `EnvironmentChange` bits for the standard input helper.
    pub(super) input: AtomicU32,
    /// Pending `EnvironmentChange` bits for the elevated collector.
    pub(super) elevated: AtomicU32,
    pub(super) display_revision: AtomicU32,
    /// When Windows announced that the session ends.
    pub(super) session_end: OnceLock<Instant>,
    /// Written only by the power callback; read through `PowerMarks::snapshot`.
    pub(super) power: PowerMarks,
}

impl EnvironmentSignals {
    /// Publish environment changes to the standard and the elevated owner.
    fn publish(&self, bits: u32) {
        self.input.fetch_or(bits, Release);
        self.elevated.fetch_or(bits, Release);
    }
}

/// One environment pause reason, read by `Notifications::reasons`.
#[derive(Clone, Copy)]
pub(super) struct ReasonState {
    pub(super) reason: PauseReason,
    /// The reason was asserted since the previous read, even if it has cleared.
    pub(super) asserted: bool,
    /// The reason holds now.
    pub(super) active: bool,
}

pub(super) struct Notifications {
    signals: Arc<Signals>,
    stop: SyncSender<()>,
    owner: thread::JoinHandle<()>,
}

struct Signals {
    reasons: AtomicU32,
    shared: Arc<EnvironmentSignals>,
    complete: AtomicBool,
    wake: SyncSender<()>,
}

impl Signals {
    fn reason(&self, bit: u32, active: bool) {
        let changed = if active {
            self.reasons.fetch_update(AcqRel, Acquire, |before| {
                (before & bit == 0).then_some(before | bit | (bit << EDGE_SHIFT))
            }).is_ok()
        } else {
            self.reasons.fetch_and(!bit, AcqRel) & bit != 0
        };
        if changed {
            self.shared.elevated.fetch_or(EnvironmentChange::Session.bit(), Release);
            let _ = self.wake.try_send(());
        }
    }
}

impl Notifications {
    pub(super) fn spawn(
        shared: Arc<EnvironmentSignals>, wake: SyncSender<()>,
    ) -> Result<(Self, Receiver<Result<(), CaptureError>>), CaptureError> {
        let signals = Arc::new(Signals { reasons: AtomicU32::new(0), shared, complete: AtomicBool::new(false), wake });
        let (stop, stopped) = mpsc::sync_channel(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        let owned = Arc::clone(&signals);
        let owner = thread::Builder::new().name("recording-windows-notifications".into()).spawn(move || {
            let owner = match Window::open(&owned) {
                Ok(owner) => owner,
                Err(error) => { let _ = ready.try_send(Err(error)); return; }
            };
            owned.reason(SECURE_DESKTOP, !on_input_desktop());
            let _ = ready.try_send(Ok(()));
            let _ = owned.wake.try_send(());
            let mut next_poll = Instant::now() + DESKTOP_POLL;
            loop {
                if !owner.pump() { break; }
                let now = Instant::now();
                if now >= next_poll {
                    owned.reason(SECURE_DESKTOP, !on_input_desktop());
                    next_poll = now + DESKTOP_POLL;
                }
                if !matches!(stopped.recv_timeout(PUMP_INTERVAL), Err(mpsc::RecvTimeoutError::Timeout)) { break; }
            }
        }).map_err(|error| window_error("Start the Windows notification owner", error.raw_os_error()))?;
        Ok((Self { signals, stop, owner }, initialized))
    }

    /// Take the assertion edges and read each environment reason: sleep, lock
    /// screen, then secure desktop.
    pub(super) fn reasons(&self) -> impl Iterator<Item = ReasonState> + use<> {
        let reasons = self.signals.reasons.fetch_and(!ASSERT_EDGES, AcqRel);
        let (active, asserted) = (reasons & !ASSERT_EDGES, (reasons & ASSERT_EDGES) >> EDGE_SHIFT);
        [(SLEEP, PauseReason::Sleep), (LOCK_SCREEN, PauseReason::LockScreen), (SECURE_DESKTOP, PauseReason::SecureDesktop)]
            .into_iter()
            .map(move |(bit, reason)| ReasonState { reason, asserted: asserted & bit != 0, active: active & bit != 0 })
    }

    pub(super) fn running(&self) -> bool { !self.owner.is_finished() }

    pub(super) fn finished(&self) {
        if !self.signals.complete.swap(true, AcqRel) {
            let _ = self.stop.try_send(());
            self.owner.thread().unpark();
        }
    }
}

impl Drop for Notifications {
    fn drop(&mut self) { self.finished(); }
}

struct Window {
    handle: HWND,
    hook: HWINEVENTHOOK,
    power: Option<PowerRegistration>,
    wts: bool,
}

impl Window {
    fn open(signals: &Arc<Signals>) -> Result<Self, CaptureError> {
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err(native_error("Read the module for controller notifications"));
        }
        let class = WNDCLASSW {
            hInstance: instance, lpszClassName: CLASS_NAME, lpfnWndProc: Some(window_proc),
            ..WNDCLASSW::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            let error = io::Error::last_os_error().raw_os_error();
            if error != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
                return Err(window_error("Register controller notifications", error));
            }
        }
        // The signals are owned by the message loop and outlive the window.
        let handle = unsafe {
            CreateWindowExW(0, CLASS_NAME, null(), WS_POPUP, 0, 0, 0, 0,
                null_mut(), null_mut(), instance, Arc::as_ptr(signals).cast())
        };
        if handle.is_null() {
            return Err(native_error("Create the controller notification window"));
        }
        let mut owner = Self { handle, hook: null_mut(), power: None, wts: false };
        if unsafe { WTSRegisterSessionNotification(handle, NOTIFY_FOR_THIS_SESSION) } == 0 {
            return Err(native_error("Subscribe to Windows session notifications"));
        }
        owner.wts = true;
        DESKTOP_SIGNALS.set(Arc::as_ptr(signals));
        owner.hook = unsafe {
            SetWinEventHook(EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_DESKTOPSWITCH,
                null_mut(), Some(desktop_changed), 0, 0, WINEVENT_OUTOFCONTEXT)
        };
        if owner.hook.is_null() {
            return Err(native_error("Subscribe to Windows desktop switches"));
        }
        owner.power = Some(PowerRegistration::open(signals)?);
        Ok(owner)
    }

    fn pump(&self) -> bool {
        let mut message = MSG::default();
        for _ in 0..64 {
            if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 { break; }
            if message.message == WM_QUIT { return false; }
            unsafe { DispatchMessageW(&message) };
        }
        true
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        drop(self.power.take());
        DESKTOP_SIGNALS.set(null());
        unsafe {
            if !self.hook.is_null() { UnhookWinEvent(self.hook); }
            if self.wts { WTSUnRegisterSessionNotification(self.handle); }
            DestroyWindow(self.handle);
        }
    }
}

struct PowerRegistration {
    handle: HPOWERNOTIFY,
    subscription: Option<Box<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>>,
    signals: *const Signals,
}

impl PowerRegistration {
    fn open(signals: &Arc<Signals>) -> Result<Self, CaptureError> {
        let signals = Arc::into_raw(Arc::clone(signals));
        let mut subscription = Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS { Callback: Some(power_changed), Context: signals.cast_mut().cast() });
        // The boxed native subscription and the strong reference behind its
        // context remain valid until the registration has been removed.
        let handle = unsafe { RegisterSuspendResumeNotification((&mut *subscription as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS).cast(), DEVICE_NOTIFY_CALLBACK) };
        if handle == 0 {
            let error = native_error("Subscribe to Windows suspend and resume");
            unsafe { drop(Arc::from_raw(signals)) };
            return Err(error);
        }
        Ok(Self { handle, subscription: Some(subscription), signals })
    }
}

impl Drop for PowerRegistration {
    fn drop(&mut self) {
        if unsafe { UnregisterSuspendResumeNotification(self.handle) } != 0 {
            unsafe { drop(Arc::from_raw(self.signals)) };
        } else {
            // A failed unregister does not establish that the callback stopped.
            // Keep its native registration data and strong context alive.
            if let Some(subscription) = self.subscription.take() {
                let _ = Box::into_raw(subscription);
            }
        }
    }
}

unsafe extern "system" fn power_changed(context: *const c_void, event: u32, _setting: *const c_void) -> u32 {
    // This is the live reference owned by the callback registration.
    let signals = unsafe { &*context.cast::<Signals>() };
    // A panic must not unwind into the Windows power manager. The body uses
    // only atomics and the bounded wake channel; it takes no lock of its own.
    let _ = catch_unwind(AssertUnwindSafe(|| match event {
        PBT_APMSUSPEND => {
            signals.shared.power.suspended(clock::qpc().ok());
            signals.reason(SLEEP, true);
        }
        PBT_APMRESUMEAUTOMATIC => {
            signals.shared.power.resumed(clock::qpc().ok());
            signals.reason(SLEEP, false);
        }
        _ => {}
    }));
    0
}

unsafe extern "system" fn desktop_changed(
    _: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32,
) {
    // A panic must not unwind into the WinEvent dispatcher.
    let _ = catch_unwind(|| {
        // Out-of-context callbacks are dispatched on this thread. The message
        // loop clears this pointer before removing the hook.
        if let Some(signals) = unsafe { DESKTOP_SIGNALS.get().as_ref() } {
            signals.reason(SECURE_DESKTOP, !on_input_desktop());
        }
    });
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic must not unwind into the window manager. The body uses only
    // atomics, the once-initialized session-end mark and the bounded wake
    // channel; it takes no lock.
    catch_unwind(AssertUnwindSafe(|| unsafe { handle_message(window, message, wparam, lparam) }))
        .unwrap_or_else(|_| unsafe { DefWindowProcW(window, message, wparam, lparam) })
}

unsafe fn handle_message(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_QUERYENDSESSION { return 1; }
    if message == WM_NCCREATE {
        let creation = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { set_signals(window, creation.lpCreateParams as isize) };
        return 1;
    }
    if message == WM_NCDESTROY {
        unsafe { set_signals(window, 0) };
    }
    // The message owner retains the signals until after DestroyWindow.
    let signals = unsafe { (get_signals(window) as *const Signals).as_ref() };
    let bits = match message {
        WM_DISPLAYCHANGE | WM_DPICHANGED => EnvironmentChange::Display.bit(),
        WM_SETTINGCHANGE => EnvironmentChange::Settings.bit(),
        WM_WTSSESSION_CHANGE => {
            if let Some(signals) = signals {
                match u32::try_from(wparam) {
                    Ok(WTS_SESSION_LOCK) => signals.reason(LOCK_SCREEN, true),
                    Ok(WTS_SESSION_UNLOCK) => signals.reason(LOCK_SCREEN, false),
                    _ => {}
                }
            }
            EnvironmentChange::Session.bit()
        }
        WM_ENDSESSION if wparam != 0 => {
            if let Some(signals) = signals {
                let started = *signals.shared.session_end.get_or_init(Instant::now);
                signals.shared.publish(EnvironmentChange::SessionEnding.bit());
                let _ = signals.wake.try_send(());
                while !signals.complete.load(Acquire) {
                    let remaining = SESSION_END_BUDGET.saturating_sub(started.elapsed());
                    if remaining.is_zero() { break; }
                    thread::park_timeout(remaining.min(PUMP_INTERVAL));
                }
            }
            return 0;
        }
        WM_ENDSESSION => EnvironmentChange::Session.bit(),
        _ => return unsafe { DefWindowProcW(window, message, wparam, lparam) },
    };
    if let Some(signals) = signals {
        if matches!(message, WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE) {
            signals.shared.display_revision.fetch_add(1, Release);
        }
        signals.shared.publish(bits);
    }
    0
}

unsafe fn set_signals(window: HWND, value: isize) {
    #[cfg(target_pointer_width = "32")]
    unsafe { SetWindowLongW(window, GWLP_USERDATA, value as i32) };
    #[cfg(target_pointer_width = "64")]
    unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, value) };
}

unsafe fn get_signals(window: HWND) -> isize {
    #[cfg(target_pointer_width = "32")]
    return unsafe { GetWindowLongW(window, GWLP_USERDATA) } as isize;
    #[cfg(target_pointer_width = "64")]
    return unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) };
}

fn native_error(stage: &'static str) -> CaptureError {
    window_error(stage, io::Error::last_os_error().raw_os_error())
}

fn window_error(stage: &'static str, code: Option<i32>) -> CaptureError {
    CaptureError::owned(crate::package::diagnostics::FailureComponent::Controller, stage, code, stage.to_owned())
}
