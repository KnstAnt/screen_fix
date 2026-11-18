//! Out-of-context notifications stay on their installing threads. A callback
//! filters, stamps and copies into a fixed-size thread-owned ring.

use std::{
    cell::{Cell, UnsafeCell},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering::{Acquire, Release}},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use windows_sys::{
    Win32::{
        Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Performance::QueryPerformanceCounter, Threading::GetCurrentThreadId},
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::*,
        },
    },
    w,
};

use screen_fix_protocol::{MessageTickMs, QpcTick, WindowId};

use crate::LossCounter;

pub use screen_fix_protocol::ipc::elevated::WindowNotice as Notice;

#[derive(Debug)]
pub enum StartError {
    Spawn(std::io::Error),
    StartupTimeout,
    HookRegistration,
    HiddenWindowModule,
    HiddenWindowClass,
    HiddenWindowCreation,
}

const RING_LENGTH: usize = 512;
const QUEUE_LENGTH: usize = 1024;
/// The longest idle wait between two progress updates of a loop.
const IDLE_WAIT_MS: u32 = 5;

const EMPTY: Notice = Notice {
    event: 0, window: WindowId(0), object: 0, thread: 0,
    source_tick: None, receipt: QpcTick(0),
};

struct Pending {
    notices: [Notice; RING_LENGTH],
    used: usize,
    lost: u64,
    /// Receipts of the first and last notice lost to ring overflow (C3).
    lost_receipts: [i64; 2],
}

impl Pending {
    fn new() -> Self {
        Self { notices: [EMPTY; RING_LENGTH], used: 0, lost: 0, lost_receipts: [0; 2] }
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

pub struct Notifications {
    stop: Arc<AtomicBool>,
    lost: Arc<LossCounter>,
    main: Option<JoinHandle<()>>,
    locations: Option<JoinHandle<()>>,
    /// The loop threads' IDs, posted a wake message on stop.
    threads: [u32; 2],
    progress: Arc<[AtomicI64; 2]>,
    notices: Receiver<Notice>,
}

impl Notifications {
    pub fn start() -> Result<Self, StartError> {
        Self::open(false)
    }

    pub fn start_scoped() -> Result<Self, StartError> {
        Self::open(true)
    }

    fn open(scoped: bool) -> Result<Self, StartError> {
        let (send, notices) = sync_channel(QUEUE_LENGTH);
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(LossCounter::default());
        let progress = Arc::new([const { AtomicI64::new(0) }; 2]);
        let (main, main_thread) = start(Loop::Main, Arc::clone(&stop), Arc::clone(&lost), Arc::clone(&progress), send.clone(), scoped)?;
        let (locations, locations_thread) = match start(Loop::Locations, Arc::clone(&stop), Arc::clone(&lost), Arc::clone(&progress), send, scoped) {
            Ok(worker) => worker,
            Err(error) => {
                stop.store(true, Release);
                wake(main_thread);
                if scoped { let _ = main.join(); }
                return Err(error);
            }
        };
        Ok(Self {
            stop, lost, main: Some(main), locations: Some(locations), threads: [main_thread, locations_thread], progress, notices,
        })
    }

    pub fn drain(&self) -> impl Iterator<Item = Notice> + '_ {
        self.notices.try_iter().take(QUEUE_LENGTH)
    }

    /// Lost notices since the last drain: count and first and last receipt.
    pub fn take_loss(&self) -> Option<(u64, QpcTick, QpcTick)> {
        self.lost.take()
    }

    pub fn running(&self) -> bool {
        self.main.as_ref().is_some_and(|owner| !owner.is_finished())
            && self.locations.as_ref().is_some_and(|owner| !owner.is_finished())
    }

    pub fn before_qpc(&self) -> QpcTick {
        QpcTick(self.progress[0].load(Acquire).min(self.progress[1].load(Acquire)))
    }

    fn request_stop(&self) {
        self.stop.store(true, Release);
        for thread in self.threads {
            wake(thread);
        }
    }

    pub fn join(mut self) -> bool {
        self.request_stop();
        let main = self.main.take().is_some_and(|owner| owner.join().is_ok());
        let locations = self.locations.take().is_some_and(|owner| owner.join().is_ok());
        main && locations
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        self.request_stop();
    }
}

/// End a loop's idle wait, so that it observes a stop request at once. After
/// the loop thread ended, the post fails, or reaches a thread that reused the
/// ID, where `WM_NULL` does nothing.
fn wake(thread: u32) {
    unsafe { PostThreadMessageW(thread, WM_NULL, 0, 0) };
}

/// Start a loop thread; returns its owner and its thread ID.
fn start(
    kind: Loop, stop: Arc<AtomicBool>, lost: Arc<LossCounter>, progress: Arc<[AtomicI64; 2]>, send: SyncSender<Notice>, scoped: bool,
) -> Result<(JoinHandle<()>, u32), StartError> {
    let (ready, initialized) = sync_channel(1);
    let name = match kind { Loop::Main => "context-window", Loop::Locations => "context-location" };
    let worker_stop = Arc::clone(&stop);
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
        // Ensure that a message queue exists before revealing the thread ID.
        let mut message = MSG::default();
        unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE) };
        if ready.try_send(Ok(unsafe { GetCurrentThreadId() })).is_err() {
            return;
        }
        while !worker_stop.load(Acquire) {
            let mut before = 0;
            if unsafe { QueryPerformanceCounter(&mut before) } == 0 { return; }
            let mut exhausted = false;
            let mut busy = false;
            for _ in 0..128 {
                if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 {
                    exhausted = true;
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
            let [first, last] = std::mem::take(&mut pending.lost_receipts);
            lost.note(std::mem::take(&mut pending.lost), first, last);
            for notice in &pending.notices[..pending.used] {
                match send.try_send(*notice) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => lost.note(1, notice.receipt.0, notice.receipt.0),
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            pending.used = 0;
            if exhausted {
                progress[match kind { Loop::Main => 0, Loop::Locations => 1 }].store(before, Release);
            }
            if !busy {
                // Wait for new messages, a stop wake or the next progress update.
                unsafe { MsgWaitForMultipleObjectsEx(0, null(), IDLE_WAIT_MS, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
            }
        }
    }).map_err(StartError::Spawn)?;
    match initialized.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(thread)) => Ok((worker, thread)),
        result => {
            // A loop that has not reported ready observes the stop flag
            // before its first wait.
            stop.store(true, Release);
            if scoped { let _ = worker.join(); }
            match result {
                Ok(Err(error)) => Err(error),
                _ => Err(StartError::StartupTimeout),
            }
        }
    }
}

struct Hooks(Vec<HWINEVENTHOOK>);

impl Hooks {
    fn open(kind: Loop) -> Result<Self, StartError> {
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
                return Err(StartError::HookRegistration);
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
    // Window-object kinds are kept only for top-level windows, so child
    // windows never use ring capacity (CTX-1). GetAncestor does not block.
    // A destroyed window has no ancestry left to check.
    let top_level = || unsafe { GetAncestor(window, GA_ROOT) } == window;
    let keep = match event {
        EVENT_OBJECT_LOCATIONCHANGE | EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE => child == 0
            && (object == caret_object || object == window_object && top_level()),
        EVENT_OBJECT_DESTROY => child == 0 && object == window_object,
        EVENT_OBJECT_NAMECHANGE => child == 0 && (object == cursor_object || object == window_object && top_level()),
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
            if pending.lost == 0 { pending.lost_receipts[0] = qpc; }
            pending.lost_receipts[1] = qpc;
            pending.lost = pending.lost.saturating_add(1);
            return;
        }
        pending.notices[pending.used] = Notice {
            event, window: WindowId(window as usize as u64), object, thread, source_tick, receipt: QpcTick(qpc),
        };
        pending.used += 1;
    });
}

pub const DISPLAY_CHANGE: u32 = 0x1_0000 + WM_DISPLAYCHANGE;
pub const SETTINGS_CHANGE: u32 = 0x1_0000 + WM_SETTINGCHANGE;

struct HiddenWindow(HWND);

impl HiddenWindow {
    fn open() -> Result<Self, StartError> {
        let name = w!("ScreenFixContextNotifications");
        let module = unsafe { GetModuleHandleW(null()) };
        if module.is_null() {
            return Err(StartError::HiddenWindowModule);
        }
        let class = WNDCLASSW {
            hInstance: module, lpszClassName: name, lpfnWndProc: Some(window_proc), ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(StartError::HiddenWindowClass);
        }
        let hwnd = unsafe { CreateWindowExW(0, name, null(), WS_POPUP, 0, 0, 0, 0,
            null_mut(), null_mut(), module, null()) };
        if hwnd.is_null() {
            return Err(StartError::HiddenWindowCreation);
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
