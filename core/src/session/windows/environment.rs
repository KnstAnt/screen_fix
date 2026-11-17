//! Windows notifications have their own message loop. Session end can hold
//! that window procedure briefly while the controller and owners drain.

use std::{
    cell::Cell,
    ffi::c_void,
    io,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering::{AcqRel, Acquire, Release}},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use windows_sys::{
    Win32::{
        Foundation::{ERROR_CLASS_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification},
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::{
                CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
                EVENT_SYSTEM_DESKTOPSWITCH, GWLP_USERDATA, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
                WINEVENT_OUTOFCONTEXT, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION, WM_NCCREATE, WM_NCDESTROY,
                WM_QUERYENDSESSION, WM_QUIT, WM_SETTINGCHANGE, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_POPUP,
            },
        },
    },
    w,
};

#[cfg(target_pointer_width = "32")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW};
#[cfg(target_pointer_width = "64")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW};

use crate::{CaptureError, platform::windows::on_input_desktop, protocol::ipc::EnvironmentChange};

const CLASS_NAME: *const u16 = w!("ScreenFixControllerNotifications");
const DESKTOP_POLL: Duration = Duration::from_millis(100);
const PUMP_INTERVAL: Duration = Duration::from_millis(10);
pub(super) const SESSION_END_BUDGET: Duration = Duration::from_millis(3800);
pub(super) const SUSPEND_BUDGET: Duration = Duration::from_secs(2);
pub(super) const LOCK_SCREEN: u32 = 1;
pub(super) const SLEEP: u32 = 2;
pub(super) const SECURE_DESKTOP: u32 = 4;
const EDGE_SHIFT: u32 = 8;
const ASSERT_EDGES: u32 = (LOCK_SCREEN | SLEEP | SECURE_DESKTOP) << EDGE_SHIFT;
const WTS_SESSION_LOCK: WPARAM = 0x7;
const WTS_SESSION_UNLOCK: WPARAM = 0x8;
const PBT_APMSUSPEND: u32 = 0x4;
const PBT_APMRESUMEAUTOMATIC: u32 = 0x12;
const DEVICE_NOTIFY_CALLBACK: u32 = 2;

thread_local! {
    static DESKTOP_SIGNALS: Cell<*const Signals> = const { Cell::new(null()) };
}

pub(super) struct Notifications {
    signals: Arc<Signals>,
    stop: SyncSender<()>,
    owner: thread::JoinHandle<()>,
}

struct Signals {
    reasons: AtomicU32,
    environment: Arc<AtomicU32>,
    elevated_environment: Arc<AtomicU32>,
    display_revision: Arc<AtomicU32>,
    session_end: Arc<OnceLock<Instant>>,
    suspended: Mutex<Option<Instant>>,
    complete: AtomicBool,
    wake: SyncSender<()>,
}

impl Signals {
    fn reason(&self, bit: u32, active: bool) {
        if active {
            if self.reasons.fetch_update(AcqRel, Acquire, |before| {
                (before & bit == 0).then_some(before | bit | (bit << EDGE_SHIFT))
            }).is_ok() {
                self.elevated_environment.fetch_or(EnvironmentChange::Session.bit(), Release);
                let _ = self.wake.try_send(());
            }
        } else if self.reasons.fetch_and(!bit, AcqRel) & bit != 0 {
            self.elevated_environment.fetch_or(EnvironmentChange::Session.bit(), Release);
            let _ = self.wake.try_send(());
        }
    }
}

impl Notifications {
    pub(super) fn spawn(
        environment: Arc<AtomicU32>, elevated_environment: Arc<AtomicU32>, display_revision: Arc<AtomicU32>,
        session_end: Arc<OnceLock<Instant>>, wake: SyncSender<()>,
    ) -> Result<(Self, Receiver<Result<(), CaptureError>>), CaptureError> {
        let signals = Arc::new(Signals {
            reasons: AtomicU32::new(0), environment, elevated_environment, display_revision, session_end,
            suspended: Mutex::new(None), complete: AtomicBool::new(false), wake,
        });
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

    pub(super) fn reasons(&self) -> (u32, u32) {
        let reasons = self.signals.reasons.fetch_and(!ASSERT_EDGES, AcqRel);
        (reasons & !ASSERT_EDGES, (reasons & ASSERT_EDGES) >> EDGE_SHIFT)
    }

    pub(super) fn suspend_started(&self) -> Option<Instant> {
        *self.signals.suspended.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

#[repr(C)]
struct PowerSubscription {
    callback: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> u32,
    context: *mut c_void,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterSuspendResumeNotification(recipient: HANDLE, flags: u32) -> HANDLE;
    fn UnregisterSuspendResumeNotification(notification: HANDLE) -> i32;
}

struct PowerRegistration {
    handle: HANDLE,
    subscription: Option<Box<PowerSubscription>>,
    signals: *const Signals,
}

impl PowerRegistration {
    fn open(signals: &Arc<Signals>) -> Result<Self, CaptureError> {
        let signals = Arc::into_raw(Arc::clone(signals));
        let mut subscription = Box::new(PowerSubscription { callback: power_changed, context: signals.cast_mut().cast() });
        // The boxed native subscription and the strong reference behind its
        // context remain valid until the registration has been removed.
        let handle = unsafe { RegisterSuspendResumeNotification((&mut *subscription as *mut PowerSubscription).cast(), DEVICE_NOTIFY_CALLBACK) };
        if handle.is_null() {
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

unsafe extern "system" fn power_changed(context: *mut c_void, event: u32, _setting: *mut c_void) -> u32 {
    // This is the live reference owned by the callback registration.
    let signals = unsafe { &*context.cast::<Signals>() };
    match event {
        PBT_APMSUSPEND => {
            if signals.reasons.load(Acquire) & SLEEP == 0 {
                *signals.suspended.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
            }
            signals.reason(SLEEP, true);
        }
        PBT_APMRESUMEAUTOMATIC => signals.reason(SLEEP, false),
        _ => {}
    }
    0
}

unsafe extern "system" fn desktop_changed(
    _: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32,
) {
    // Out-of-context callbacks are dispatched on this thread. The message
    // loop clears this pointer before removing the hook.
    if let Some(signals) = unsafe { DESKTOP_SIGNALS.get().as_ref() } {
        signals.reason(SECURE_DESKTOP, !on_input_desktop());
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
                match wparam {
                    WTS_SESSION_LOCK => signals.reason(LOCK_SCREEN, true),
                    WTS_SESSION_UNLOCK => signals.reason(LOCK_SCREEN, false),
                    _ => {}
                }
            }
            EnvironmentChange::Session.bit()
        }
        WM_ENDSESSION if wparam != 0 => {
            if let Some(signals) = signals {
                let started = *signals.session_end.get_or_init(Instant::now);
                signals.environment.fetch_or(EnvironmentChange::SessionEnding.bit(), Release);
                signals.elevated_environment.fetch_or(EnvironmentChange::SessionEnding.bit(), Release);
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
            signals.display_revision.fetch_add(1, Release);
        }
        signals.environment.fetch_or(bits, Release);
        signals.elevated_environment.fetch_or(bits, Release);
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
    CaptureError::backend(stage, code, stage.to_owned())
}
