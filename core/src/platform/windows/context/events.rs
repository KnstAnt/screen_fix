//! Out-of-context notifications stay on their installing threads. A callback
//! filters, stamps and copies into a fixed-size thread-owned ring.

use std::{
    cell::{Cell, UnsafeCell},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use windows_sys::{
    Win32::{
        Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Performance::QueryPerformanceCounter},
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::*,
        },
    },
    w,
};

use crate::protocol::{MessageTickMs, QpcTick, WindowId};

use super::super::{ProbeError, mp4::io_error, unavailable};

const RING_LENGTH: usize = 512;
const QUEUE_LENGTH: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub(super) struct Notice {
    pub event: u32,
    pub window: WindowId,
    pub object: i32,
    pub thread: u32,
    pub source_tick: Option<MessageTickMs>,
    pub receipt: QpcTick,
}

const EMPTY: Notice = Notice {
    event: 0, window: WindowId(0), object: 0, thread: 0,
    source_tick: None, receipt: QpcTick(0),
};

struct Pending {
    notices: [Notice; RING_LENGTH],
    used: usize,
    lost: u64,
}

impl Pending {
    fn new() -> Self {
        Self { notices: [EMPTY; RING_LENGTH], used: 0, lost: 0 }
    }
}

thread_local! {
    static ACTIVE: Cell<*mut Pending> = const { Cell::new(null_mut()) };
}

/// The stack allocation outlives both the registration and all dispatch on
/// the installing thread. The pointer is cleared after unhook, before drop.
struct Binding;

impl Binding {
    fn install(pending: &UnsafeCell<Pending>) -> Self {
        ACTIVE.with(|active| active.set(pending.get()));
        Self
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(null_mut()));
    }
}

#[derive(Clone, Copy)]
enum Loop {
    Main,
    Locations,
}

pub(super) struct Notifications {
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicU64>,
    main: JoinHandle<()>,
    locations: JoinHandle<()>,
    notices: Receiver<Notice>,
}

impl Notifications {
    pub fn start() -> Result<Self, ProbeError> {
        let (send, notices) = sync_channel(QUEUE_LENGTH);
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicU64::new(0));
        let main = start(Loop::Main, Arc::clone(&stop), Arc::clone(&lost), send.clone())?;
        let locations = match start(Loop::Locations, Arc::clone(&stop), Arc::clone(&lost), send) {
            Ok(worker) => worker,
            Err(error) => {
                stop.store(true, Release);
                main.thread().unpark();
                return Err(error);
            }
        };
        Ok(Self { stop, lost, main, locations, notices })
    }

    pub fn drain(&self) -> impl Iterator<Item = Notice> + '_ {
        self.notices.try_iter().take(QUEUE_LENGTH)
    }

    pub fn take_loss(&self) -> u64 {
        self.lost.swap(0, Relaxed)
    }

    pub fn running(&self) -> bool {
        !self.main.is_finished() && !self.locations.is_finished()
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        self.stop.store(true, Release);
        self.main.thread().unpark();
        self.locations.thread().unpark();
    }
}

fn start(
    kind: Loop, stop: Arc<AtomicBool>, lost: Arc<AtomicU64>, send: SyncSender<Notice>,
) -> Result<JoinHandle<()>, ProbeError> {
    let (ready, initialized) = sync_channel(1);
    let name = match kind { Loop::Main => "context-window", Loop::Locations => "context-location" };
    let worker = thread::Builder::new().name(name.into()).spawn(move || {
        let storage = UnsafeCell::new(Pending::new());
        let _binding = Binding::install(&storage);
        let hooks = Hooks::open(kind);
        let window = match kind {
            Loop::Main => HiddenWindow::open().map(Some),
            Loop::Locations => Ok(None),
        };
        let (_hooks, _window) = match hooks.and_then(|hooks| window.map(|window| (hooks, window))) {
            Ok(owners) => owners,
            Err(error) => {
                let _ = ready.try_send(Err(error));
                return;
            }
        };
        if ready.try_send(Ok(())).is_err() {
            return;
        }
        while !stop.load(Acquire) {
            let mut message = MSG::default();
            let mut busy = false;
            for _ in 0..128 {
                if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 {
                    break;
                }
                if message.message == WM_QUIT {
                    return;
                }
                busy = true;
                unsafe { DispatchMessageW(&message) };
            }
            // No Windows dispatch is performed while reading this ring.
            let pending = unsafe { &mut *storage.get() };
            let mut dropped = std::mem::take(&mut pending.lost);
            for notice in &pending.notices[..pending.used] {
                match send.try_send(*notice) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => dropped = dropped.saturating_add(1),
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            pending.used = 0;
            if dropped != 0 {
                let _ = lost.fetch_update(Relaxed, Relaxed, |count| Some(count.saturating_add(dropped)));
            }
            if !busy {
                thread::park_timeout(Duration::from_millis(5));
            }
        }
    }).map_err(|error| io_error("Start context notification loop", error))?;
    initialized.recv_timeout(Duration::from_secs(2))
        .map_err(|_| unavailable("A context notification loop did not start"))??;
    Ok(worker)
}

struct Hooks(Vec<HWINEVENTHOOK>);

impl Hooks {
    fn open(kind: Loop) -> Result<Self, ProbeError> {
        const MAIN: &[(u32, u32)] = &[
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_SYSTEM_MENUSTART, EVENT_SYSTEM_MENUPOPUPEND),
            (EVENT_SYSTEM_CAPTURESTART, EVENT_SYSTEM_CAPTUREEND),
            (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
            (EVENT_SYSTEM_DIALOGSTART, EVENT_SYSTEM_DIALOGEND),
            (EVENT_SYSTEM_SWITCHSTART, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_DESKTOPSWITCH),
            (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_FOCUS, EVENT_OBJECT_FOCUS),
            (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
            (EVENT_OBJECT_IME_SHOW, EVENT_OBJECT_IME_CHANGE),
        ];
        const LOCATION: &[(u32, u32)] = &[(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE)];
        let pairs = match kind { Loop::Main => MAIN, Loop::Locations => LOCATION };
        let mut hooks = Self(Vec::with_capacity(pairs.len()));
        for &(from, to) in pairs {
            let hook = unsafe {
                SetWinEventHook(from, to, null_mut(), Some(on_event), 0, 0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)
            };
            if hook.is_null() {
                return Err(unavailable("Could not subscribe to Windows context notifications"));
            }
            hooks.0.push(hook);
        }
        Ok(hooks)
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        for &hook in &self.0 {
            unsafe { UnhookWinEvent(hook) };
        }
    }
}

unsafe extern "system" fn on_event(
    _: HWINEVENTHOOK, event: u32, window: HWND, object: i32, child: i32, thread: u32, tick: u32,
) {
    let window_object = OBJID_WINDOW as i32;
    let caret_object = OBJID_CARET as i32;
    let cursor_object = OBJID_CURSOR as i32;
    let keep = match event {
        EVENT_OBJECT_LOCATIONCHANGE => child == 0 && (object == window_object || object == caret_object),
        EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE => child == 0 && (object == window_object || object == caret_object),
        EVENT_OBJECT_DESTROY => child == 0 && object == window_object,
        EVENT_OBJECT_NAMECHANGE => child == 0 && (object == window_object || object == cursor_object),
        _ => true,
    };
    if keep {
        let _ = std::panic::catch_unwind(|| enqueue(event, window, object, thread, Some(MessageTickMs(tick))));
    }
}

fn enqueue(event: u32, window: HWND, object: i32, thread: u32, source_tick: Option<MessageTickMs>) {
    let mut qpc = 0;
    let have_stamp = unsafe { QueryPerformanceCounter(&mut qpc) } != 0;
    let _ = ACTIVE.try_with(|active| {
        // The installing thread owns this memory. No dispatching API is
        // called while the callback touches it and no reference is retained.
        let Some(pending) = (unsafe { active.get().as_mut() }) else { return };
        if !have_stamp || pending.used >= RING_LENGTH {
            pending.lost = pending.lost.saturating_add(1);
            return;
        }
        pending.notices[pending.used] = Notice {
            event, window: WindowId(window as usize as u64), object, thread, source_tick, receipt: QpcTick(qpc),
        };
        pending.used += 1;
    });
}

pub(super) const DISPLAY_CHANGE: u32 = 0x1_0000 + WM_DISPLAYCHANGE;
pub(super) const SETTINGS_CHANGE: u32 = 0x1_0000 + WM_SETTINGCHANGE;

struct HiddenWindow(HWND);

impl HiddenWindow {
    fn open() -> Result<Self, ProbeError> {
        let name = w!("ScreenFixContextNotifications");
        let module = unsafe { GetModuleHandleW(null()) };
        if module.is_null() {
            return Err(unavailable("Could not get the window context module"));
        }
        let class = WNDCLASSW {
            hInstance: module, lpszClassName: name, lpfnWndProc: Some(window_proc), ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(unavailable("Could not register the window context class"));
        }
        let hwnd = unsafe { CreateWindowExW(0, name, null(), WS_POPUP, 0, 0, 0, 0,
            null_mut(), null_mut(), module, null()) };
        if hwnd.is_null() {
            return Err(unavailable("Could not create the hidden top-level context window"));
        }
        Ok(Self(hwnd))
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let event = match message {
        WM_DISPLAYCHANGE => DISPLAY_CHANGE,
        WM_SETTINGCHANGE => SETTINGS_CHANGE,
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    };
    let _ = std::panic::catch_unwind(|| enqueue(event, hwnd, 0, 0, None));
    0
}
