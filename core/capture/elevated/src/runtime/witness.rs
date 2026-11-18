use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    mem::{size_of, size_of_val},
    num::NonZeroU32,
    ptr::{null, null_mut},
    sync::{Arc, Mutex, atomic::{AtomicI64, AtomicU64, Ordering::{Acquire, Release}}},
    thread::{self, JoinHandle},
    time::Duration,
};

use screen_fix_protocol::{MESSAGE_TICK_UNCERTAINTY_MS, MessageTickMs, QpcTick, WindowId, ipc::elevated::WindowInstance};
use windows_sys::{
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, LRESULT, WAIT_FAILED, WPARAM},
        System::{
            SystemInformation::GetTickCount64,
            LibraryLoader::GetModuleHandleW,
            RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, ProcessIdToSessionId, WTS_SESSIONSTATE_UNLOCK, WTSActive, WTSFreeMemory,
                WTSINFOEXW, WTSQuerySessionInformationW, WTSRegisterSessionNotification,
                WTSSessionInfoEx, WTSUnRegisterSessionNotification},
            StationsAndDesktops::{CloseDesktop, DESKTOP_HOOKCONTROL, DESKTOP_JOURNALRECORD,
                DESKTOP_READOBJECTS, GetProcessWindowStation, GetThreadDesktop,
                GetUserObjectInformationW, HDESK, OpenInputDesktop, UOI_IO, UOI_NAME},
            Threading::{GetCurrentProcessId, GetCurrentThreadId},
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::*,
        },
    },
    w,
};

use crate::{deadline::{Cancellation, milliseconds}, process::Process, win::{Failure, Result}};
use super::lifetime::qpc;

/// The longest interval between two witness samples.
const SAMPLE: Duration = Duration::from_millis(10);
/// A coarse tick older than this is not bounded from its anchor.
const MAX_TICK_AGE_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Foreground {
    pub(super) window: WindowId,
    pub(super) pid: NonZeroU32,
    pub(super) thread: NonZeroU32,
}

impl Foreground {
    pub(super) fn read() -> Option<Self> {
        let hwnd = unsafe { GetForegroundWindow() };
        let foreground = Self::of(hwnd)?;
        (unsafe { GetForegroundWindow() } == hwnd).then_some(foreground)
    }

    pub(super) fn of(hwnd: HWND) -> Option<Self> {
        let mut pid = 0;
        if hwnd.is_null() { return None; }
        let thread = NonZeroU32::new(unsafe { GetWindowThreadProcessId(hwnd, &mut pid) })?;
        Some(Self { window: WindowId(hwnd as usize as u64), pid: NonZeroU32::new(pid)?, thread })
    }

    pub(super) fn matches(self, target: WindowInstance) -> bool {
        self.window == target.window && self.pid == target.process.pid && self.thread == target.thread
    }
}

#[derive(Clone, Copy)]
pub(super) struct Snapshot {
    pub(super) foreground: Option<Foreground>,
    pub(super) revision: u64,
    pub(super) focus_revision: u64,
    pub(super) observed: QpcTick,
    pub(super) before_change: QpcTick,
    pub(super) safe: bool,
}

pub(super) struct Witness {
    pub(super) progress: Arc<AtomicI64>,
    pub(super) view: View,
    worker: JoinHandle<Result<()>>,
}

impl Witness {
    pub(super) fn start(session: u32, stop: &Arc<Cancellation>, lifetime: Arc<super::lifetime::Lifetime>) -> Result<Self> {
        let view = View {
            snapshot: Arc::new(Mutex::new(History::default())),
            revisions: Arc::new(AtomicU64::new(0)),
            serial: Arc::new(AtomicU64::new(1)),
            published: Arc::new(AtomicU64::new(0)),
        };
        let progress = Arc::new(AtomicI64::new(0));
        let output = view.clone();
        let stamp = Arc::clone(&progress);
        let cancel = Arc::clone(stop);
        let worker = thread::Builder::new().name("elevated-witness".into()).spawn(move || {
            let result = observe(session, &cancel, &output, &stamp, &lifetime);
            if result.is_err() { cancel.cancel(); }
            result
        }).map_err(|_| Failure::Supervisor)?;
        Ok(Self { progress, view, worker })
    }

    pub(super) fn join(self) -> Result<()> {
        self.worker.join().map_err(|_| Failure::Supervisor)?
    }
}

#[derive(Clone)]
pub(super) struct View {
    snapshot: Arc<Mutex<History>>,
    serial: Arc<AtomicU64>,
    published: Arc<AtomicU64>,
    revisions: Arc<AtomicU64>,
}

const MAX_BOUNDARIES: usize = 32;

#[derive(Default)]
struct History {
    current: Option<Snapshot>,
    changes: VecDeque<(u64, QpcTick)>,
}

impl History {
    fn publish(&mut self, next: Snapshot) {
        if self.current.is_none_or(|old| old.focus_revision != next.focus_revision) {
            if self.changes.len() == MAX_BOUNDARIES { self.changes.pop_front(); }
            self.changes.push_back((next.focus_revision, next.before_change));
        }
        self.current = Some(next);
    }

    fn lower_since(&self, focus_revision: u64) -> QpcTick {
        let Some(next) = focus_revision.checked_add(1) else { return QpcTick(0); };
        let Some(index) = self.changes.iter().position(|(revision, _)| *revision == next) else {
            return QpcTick(0);
        };
        self.changes.iter().skip(index).map(|(_, bound)| *bound).min().unwrap_or(QpcTick(0))
    }
}

impl View {
    pub(super) fn dirty(&self) -> bool {
        self.serial.load(Acquire) != self.published.load(Acquire)
    }

    pub(super) fn revision(&self) -> u64 {
        self.revisions.load(Acquire)
    }

    pub(super) fn read(&self) -> Result<Option<Snapshot>> {
        Ok(self.snapshot.lock().map_err(|_| Failure::Supervisor)?.current)
    }

    pub(super) fn boundary_since(&self, focus_revision: u64) -> Result<QpcTick> {
        Ok(self.snapshot.lock().map_err(|_| Failure::Supervisor)?.lower_since(focus_revision))
    }
}

#[derive(Default)]
struct Changes {
    boundary: bool,
    focus: bool,
    sleeping: bool,
    ending: bool,
    event_tick: Option<u32>,
    time_unknown: bool,
    last_foreground: Option<Foreground>,
}

thread_local! {
    static SERIAL: RefCell<Option<Arc<AtomicU64>>> = const { RefCell::new(None) };
    static CHANGES: Cell<Changes> = const { Cell::new(Changes {
        boundary: false, focus: false, sleeping: false, ending: false, event_tick: None, time_unknown: false, last_foreground: None,
    }) };
}

fn record(f: impl FnOnce(&mut Changes)) {
    let _ = CHANGES.try_with(|changes| {
        let mut next = changes.take();
        f(&mut next);
        changes.set(next);
    });
}

fn invalidate() {
    let _ = SERIAL.try_with(|slot| {
        if let Some(serial) = slot.borrow().as_ref() {
            let _ = serial.fetch_update(Release, Acquire, |old| old.checked_add(1));
        }
    });
}

struct Hook(HWINEVENTHOOK);

impl Hook {
    fn start(event: u32) -> Result<Self> {
        let handle = unsafe {
            SetWinEventHook(event, event, null_mut(), Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT)
        };
        if handle.is_null() { return Err(Failure::Witness); }
        Ok(Self(handle))
    }
}

impl Drop for Hook {
    fn drop(&mut self) { unsafe { UnhookWinEvent(self.0) }; }
}

unsafe extern "system" fn on_event(
    _: HWINEVENTHOOK, event: u32, window: HWND, object: i32, child: i32, _: u32, tick: u32,
) {
    record(|change| {
        if event == EVENT_OBJECT_DESTROY
            && (object != OBJID_WINDOW as i32 || child != 0
                || change.last_foreground.is_none_or(|old| old.window.0 != window as usize as u64))
        {
            return;
        }
        if event == EVENT_OBJECT_FOCUS && change.last_foreground.is_some_and(|current| {
            Foreground::of(window).is_some_and(|candidate| candidate.pid != current.pid)
        }) {
            return;
        }
        if change.event_tick.is_none_or(|previous| (tick.wrapping_sub(previous) as i32) < 0) {
            change.event_tick = Some(tick);
        }
        invalidate();
        if event == EVENT_OBJECT_FOCUS {
            change.focus = true;
        } else {
            change.boundary = true;
        }
    });
}

struct Window(HWND);

impl Window {
    fn start() -> Result<Self> {
        let model = w!("ScreenFixElevatedWitness");
        let module = unsafe { GetModuleHandleW(null()) };
        let class = WNDCLASSW { hInstance: module, lpszClassName: model, lpfnWndProc: Some(window_proc), ..Default::default() };
        if module.is_null() || unsafe { RegisterClassW(&class) } == 0 {
            return Err(Failure::Witness);
        }
        let hwnd = unsafe { CreateWindowExW(0, model, null(), WS_POPUP, 0, 0, 0, 0,
            null_mut(), null_mut(), module, null()) };
        if hwnd.is_null() { return Err(Failure::Witness); }
        let window = Self(hwnd);
        if unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } == 0 {
            return Err(Failure::Witness);
        }
        Ok(window)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            WTSUnRegisterSessionNotification(self.0);
            DestroyWindow(self.0);
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_WTSSESSION_CHANGE | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            invalidate();
            record(|change| { change.boundary = true; change.time_unknown = true; });
            0
        }
        WM_QUERYENDSESSION | WM_ENDSESSION => {
            invalidate();
            record(|change| change.ending = true);
            1
        }
        WM_POWERBROADCAST => {
            invalidate();
            record(|change| {
                change.boundary = true;
                change.time_unknown = true;
                // PBT_APMSUSPEND and the three PBT_APMRESUME* messages.
                match wparam { 4 => change.sleeping = true, 6 | 7 | 18 => change.sleeping = false, _ => {} }
            });
            1
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn observe(
    session: u32, cancel: &Cancellation, output: &View, progress: &AtomicI64,
    lifetime: &super::lifetime::Lifetime,
) -> Result<()> {
    SERIAL.with(|slot| slot.replace(Some(Arc::clone(&output.serial))));
    let _window = Window::start()?;
    let _foreground = Hook::start(EVENT_SYSTEM_FOREGROUND)?;
    let _desktop = Hook::start(EVENT_SYSTEM_DESKTOPSWITCH)?;
    let _focus = Hook::start(EVENT_OBJECT_FOCUS)?;
    let _destruction = Hook::start(EVENT_OBJECT_DESTROY)?;
    let mut previous = Snapshot { foreground: None, revision: 0, focus_revision: 0, observed: QpcTick(0), before_change: QpcTick(0), safe: false };
    while cancel.check().is_ok() {
        let mut msg = MSG::default();
        let mut count = 0;
        while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            count += 1;
            if count > 128 || msg.message == WM_QUIT {
                return Err(Failure::Witness);
            }
            unsafe { DispatchMessageW(&msg) };
        }
        let serial = output.serial.load(Acquire);
        if serial == u64::MAX { return Err(Failure::Witness); }
        let (boundary, focus, unavailable, ending, event_tick, time_unknown) = CHANGES.with(|changes| {
            let state = changes.take();
            let result = (state.boundary, state.focus, state.sleeping, state.ending, state.event_tick, state.time_unknown);
            changes.set(state);
            result
        });
        if ending { return Err(Failure::Witness); }
        let started = qpc()?;
        let safe = !unavailable && normal_desktop(None) && session_unlocked(session);
        let foreground = if safe { Foreground::read() } else { None };
        record(|change| change.last_foreground = foreground);
        let finished = qpc()?;
        if started < previous.observed || finished < started { return Err(Failure::Witness); }
        let changed = boundary || safe != previous.safe || foreground != previous.foreground || previous.revision == 0;
        let revision = previous.revision.checked_add(u64::from(changed)).ok_or(Failure::Witness)?;
        let focus_revision = previous.focus_revision.checked_add(u64::from(changed || focus)).ok_or(Failure::Witness)?;
        let before_change = if changed || focus {
            if time_unknown {
                QpcTick(0)
            } else if let Some(tick) = event_tick {
                coarse_lower(tick, lifetime).unwrap_or(QpcTick(0)).min(started)
            } else if changed {
                // A polled change has no trustworthy notification timestamp.
                QpcTick(0)
            } else {
                previous.observed
            }
        } else {
            previous.before_change
        };
        if output.serial.load(Acquire) != serial {
            // A native callback ran while polling. Keep its dirty latch and
            // retain the whole event boundary for the next stable sample.
            continue;
        }
        record(|state| {
            state.boundary = false;
            state.focus = false;
            state.event_tick = None;
            state.time_unknown = false;
        });
        previous = Snapshot { foreground, revision, focus_revision, safe, observed: started, before_change };
        output.snapshot.lock().map_err(|_| Failure::Witness)?.publish(previous);
        output.revisions.store(focus_revision, Release);
        output.published.store(serial, Release);
        progress.store(finished.0, Release);
        // Wake on a queued message or cancellation; the timeout bounds the
        // polling of state that changes without a notification.
        let handles = [cancel.handle().raw()];
        let woken = unsafe {
            MsgWaitForMultipleObjectsEx(handles.len() as u32, handles.as_ptr(), milliseconds(SAMPLE), QS_ALLINPUT,
                MWMO_INPUTAVAILABLE)
        };
        if woken == WAIT_FAILED { return Err(Failure::Witness); }
    }
    Ok(())
}

/// A WinEvent or input message tick can wrap and is coarser than QPC.
pub(super) fn coarse_lower(tick: u32, lifetime: &super::lifetime::Lifetime) -> Option<QpcTick> {
    let start = qpc().ok()?;
    let anchor = unsafe { GetTickCount64() };
    let end = qpc().ok()?;
    if end < start { return None; }
    let delta = MessageTickMs(tick).delta_from(anchor)?;
    let elapsed = u64::try_from(-i64::from(delta)).ok().filter(|elapsed| *elapsed <= MAX_TICK_AGE_MS)?;
    let range = Duration::from_millis(elapsed + MESSAGE_TICK_UNCERTAINTY_MS);
    Some(QpcTick(start.0.checked_sub(lifetime.ticks(range).ok()?)?))
}

pub(crate) fn fresh(progress: &AtomicI64, now: QpcTick, max_age: i64) -> bool {
    let observed = progress.load(Acquire);
    observed > 0 && now.0.saturating_sub(observed) <= max_age
}

fn session_unlocked(session: u32) -> bool {
    let mut buffer = null_mut();
    let mut length = 0;
    if unsafe { WTSQuerySessionInformationW(null_mut(), session, WTSSessionInfoEx, &mut buffer, &mut length) } == 0
        || buffer.is_null()
    {
        return false;
    }
    let result = if length as usize >= size_of::<WTSINFOEXW>() {
        let info = unsafe { buffer.cast::<WTSINFOEXW>().read_unaligned() };
        if info.Level == 1 {
            let data = unsafe { info.Data.WTSInfoExLevel1 };
            data.SessionId == session && data.SessionState == WTSActive
                && data.SessionFlags == WTS_SESSIONSTATE_UNLOCK as i32
        } else { false }
    } else { false };
    unsafe { WTSFreeMemory(buffer.cast()) };
    result
}

struct Desktop(HDESK);

impl Drop for Desktop {
    fn drop(&mut self) { unsafe { CloseDesktop(self.0) }; }
}

pub(super) fn normal_desktop(target_thread: Option<NonZeroU32>) -> bool {
    input_desktop(target_thread, DESKTOP_READOBJECTS).is_some()
}

pub fn normal_capture_desktop(target_thread: NonZeroU32) -> bool {
    let mut session = 0;
    normal_desktop(Some(target_thread))
        && unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } != 0
        && session_unlocked(session)
}

/// Hold the access-checked handle throughout the physical key read. UOI_IO
/// identifies the actual desktop receiving input, even if names match.
pub(super) struct HeldAccess {
    input: Desktop,
    target_thread: NonZeroU32,
}

impl HeldAccess {
    pub(super) fn open(target_thread: NonZeroU32) -> Result<Self> {
        let input = input_desktop(Some(target_thread), DESKTOP_READOBJECTS | DESKTOP_HOOKCONTROL)
            .or_else(|| input_desktop(Some(target_thread), DESKTOP_READOBJECTS | DESKTOP_JOURNALRECORD))
            .ok_or(Failure::TargetAccess)?;
        Ok(Self { input, target_thread })
    }

    pub(super) fn current(&self) -> bool {
        desktop_matches(self.input.0, Some(self.target_thread))
    }
}

fn input_desktop(target_thread: Option<NonZeroU32>, rights: u32) -> Option<Desktop> {
    let station = unsafe { GetProcessWindowStation() };
    if !name_is(station, "WinSta0") { return None; }
    let input = unsafe { OpenInputDesktop(0, 0, rights) };
    if input.is_null() { return None; }
    let input = Desktop(input);
    desktop_matches(input.0, target_thread).then_some(input)
}

fn desktop_matches(input: HDESK, target_thread: Option<NonZeroU32>) -> bool {
    is_active_default(input) && is_active_default(unsafe { GetThreadDesktop(GetCurrentThreadId()) })
        && target_thread.is_none_or(|thread| is_active_default(unsafe { GetThreadDesktop(thread.get()) }))
}

fn is_active_default(desktop: HDESK) -> bool {
    if !name_is(desktop, "Default") { return false; }
    let mut receiving = 0i32;
    let mut bytes = 0;
    let allowed = unsafe { GetUserObjectInformationW(
        desktop, UOI_IO, (&mut receiving as *mut i32).cast(), size_of::<i32>() as u32, &mut bytes,
    ) };
    allowed != 0 && receiving != 0
}

fn name_is(handle: HANDLE, expected: &str) -> bool {
    if handle.is_null() { return false; }
    let mut name = [0u16; 32];
    if unsafe { GetUserObjectInformationW(handle, UOI_NAME, name.as_mut_ptr().cast(), size_of_val(&name) as u32, null_mut()) } == 0 {
        return false;
    }
    let Some(end) = name.iter().position(|&unit| unit == 0) else { return false; };
    name[..end].iter().copied().eq(expected.encode_utf16())
}

/// The HWND is obtained locally. The claim is used only for comparison.
pub(super) struct Target {
    pub(super) window: Foreground,
    pub(super) process: Process,
}

impl Target {
    pub(super) fn prove(
        local: Foreground, claim: &screen_fix_protocol::ipc::elevated::runtime::IdentityClaim, parent: &Process,
    ) -> Result<Self> {
        // `recheck` below also proves the desktop binding.
        if local.pid.get() == parent.pid() || local.pid.get() == unsafe { GetCurrentProcessId() }
            || !local.matches(claim.target)
        {
            return Err(Failure::TargetChanged);
        }
        let process = Process::open(local.pid.get())?;
        if process.instance() != claim.target.process || !process.matches_win32_image(claim.image.as_str())? {
            return Err(Failure::TargetChanged);
        }
        let target = Self { window: local, process };
        target.recheck(parent)?;
        Ok(target)
    }

    pub(super) fn recheck(&self, parent: &Process) -> Result<()> {
        crate::process::verify_capture_target(parent, &self.process)?;
        self.bound()
    }

    /// The window, thread and desktop binding, without the token facts.
    pub(super) fn bound(&self) -> Result<()> {
        let hwnd = self.window.window.0 as usize as HWND;
        if Foreground::of(hwnd) != Some(self.window) || !normal_desktop(Some(self.window.thread)) {
            return Err(Failure::TargetChanged);
        }
        Ok(())
    }
}
