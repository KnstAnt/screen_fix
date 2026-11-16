//! Opt-in clipboard notifications and delayed-rendering reads belong to this
//! owner alone. The context gate must approve one concrete change before a read.

use std::{
    mem::align_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::{Acquire, Release}},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HGLOBAL, HINSTANCE, HWND},
        System::{
            DataExchange::{
                AddClipboardFormatListener, CloseClipboard, GetClipboardData, GetClipboardOwner,
                GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
                RegisterClipboardFormatW, RemoveClipboardFormatListener,
            },
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::CF_UNICODETEXT,
        },
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

use crate::protocol::{MessageTickMs, QpcTick};

use super::{
    ProbeError, clock, context::{FocusGuard, Visit, identity::WindowProcess},
    mp4::io_error, stage, unavailable,
};

const QUEUE: usize = 8;
const MAX_BYTES: usize = 64 * 1024;
pub(super) const MAX_AGE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
pub(super) struct Notice {
    pub sequence: u32,
    pub source_tick: MessageTickMs,
    pub received: QpcTick,
    pub foreground: WindowProcess,
    pub owner: Option<WindowProcess>,
    pub observed: Instant,
}

struct Grant {
    notice: Notice,
    visit: Visit,
    focus: FocusGuard,
}

pub(super) struct Read {
    pub notice: Notice,
    pub visit: Visit,
    pub completed: QpcTick,
    pub text: String,
}

pub(super) struct Worker {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
    notices: Receiver<Notice>,
    grants: SyncSender<Grant>,
    results: Receiver<Read>,
}

impl Worker {
    pub fn start() -> Result<Self, ProbeError> {
        let (ready, initialized) = sync_channel(1);
        let (notice, notices) = sync_channel(QUEUE);
        let (grants, approved) = sync_channel(QUEUE);
        let (result, results) = sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = thread::Builder::new().name("context-clipboard".into()).spawn(move || {
            let owner = match ClipboardWindow::open() {
                Ok(owner) => owner,
                Err(error) => {
                    let _ = ready.try_send(Err(error));
                    return;
                }
            };
            if ready.try_send(Ok(())).is_err() {
                return;
            }
            let mut last_sequence = unsafe { GetClipboardSequenceNumber() };
            while !stopped.load(Acquire) {
                let mut message = MSG::default();
                for _ in 0..64 {
                    if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                        break;
                    }
                    if message.message == WM_QUIT {
                        return;
                    }
                    if message.message == WM_CLIPBOARDUPDATE && message.hwnd == owner.0 {
                        let sequence = unsafe { GetClipboardSequenceNumber() };
                        if sequence != 0 && sequence != last_sequence {
                            last_sequence = sequence;
                            if let Some(observation) = observe(sequence, MessageTickMs(message.time)) {
                                let _ = notice.try_send(observation);
                            }
                        }
                    } else {
                        unsafe { DispatchMessageW(&message) };
                    }
                }
                for grant in approved.try_iter().take(QUEUE) {
                    if stopped.load(Acquire) {
                        break;
                    }
                    if let Some(read) = owner.read(grant) {
                        let _ = result.try_send(read);
                    }
                }
                thread::park_timeout(Duration::from_millis(5));
            }
        }).map_err(|error| io_error("Start the opt-in clipboard owner", error))?;
        initialized.recv_timeout(Duration::from_secs(2))
            .map_err(|_| unavailable("The clipboard owner did not start"))??;
        Ok(Self { stop, thread, notices, grants, results })
    }

    pub fn notices(&self) -> impl Iterator<Item = Notice> + '_ {
        self.notices.try_iter().take(QUEUE)
    }

    pub fn grant(&self, notice: Notice, visit: Visit, focus: FocusGuard) {
        if self.grants.try_send(Grant { notice, visit, focus }).is_ok() {
            self.thread.thread().unpark();
        }
    }

    pub fn results(&self) -> impl Iterator<Item = Read> + '_ {
        self.results.try_iter().take(QUEUE)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Release);
        self.thread.thread().unpark();
    }
}

fn observe(sequence: u32, source_tick: MessageTickMs) -> Option<Notice> {
    let received = clock::qpc().ok()?;
    let foreground = WindowProcess::foreground()?;
    let owner = clipboard_owner()?;
    if owner.is_some_and(|owner| owner.process != foreground.process)
        || unsafe { GetClipboardSequenceNumber() } != sequence
        || WindowProcess::foreground() != Some(foreground)
    {
        return None;
    }
    Some(Notice { sequence, source_tick, received, foreground, owner, observed: Instant::now() })
}

fn clipboard_owner() -> Option<Option<WindowProcess>> {
    match unsafe { GetClipboardOwner() } {
        Ok(window) if !window.0.is_null() => WindowProcess::read(window).map(Some),
        _ => Some(None),
    }
}

struct ClipboardWindow(HWND, ExclusionFormats);

struct ExclusionFormats {
    monitor: u32,
    history: u32,
}

impl ClipboardWindow {
    fn open() -> Result<Self, ProbeError> {
        let monitor = unsafe { RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")) };
        let history = unsafe { RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")) };
        if monitor == 0 || history == 0 {
            return Err(unavailable("Could not register clipboard exclusion formats"));
        }
        let window = stage(unsafe {
            CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("screen_fix clipboard"), WINDOW_STYLE(0),
                0, 0, 0, 0, Some(HWND_MESSAGE), None, None::<HINSTANCE>, None)
        }, "Create the clipboard listener window")?;
        let worker = Self(window, ExclusionFormats { monitor, history });
        stage(unsafe { AddClipboardFormatListener(window) }, "Subscribe to opt-in clipboard changes")?;
        Ok(worker)
    }

    fn read(&self, grant: Grant) -> Option<Read> {
        if !valid(&grant) || unsafe { OpenClipboard(Some(self.0)) }.is_err() {
            return None;
        }
        let clipboard = OpenedClipboard;
        if !valid(&grant) || self.excluded() || !valid(&grant) {
            return None;
        }
        let format = CF_UNICODETEXT.0 as u32;
        unsafe { IsClipboardFormatAvailable(format) }.ok()?;
        let handle = unsafe { GetClipboardData(format) }.ok()?;
        let text = read_global(HGLOBAL(handle.0), MAX_BYTES, |data| {
            if data.len() < 2 || data.len() % 2 != 0 || (data.as_ptr() as usize) % align_of::<u16>() != 0 {
                return None;
            }
            let units = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u16>(), data.len() / 2) };
            let end = units.iter().position(|unit| *unit == 0)?;
            let text = String::from_utf16(&units[..end]).ok()?;
            (!text.is_empty() && text.len() <= MAX_BYTES).then_some(text)
        })?;
        if !valid(&grant) {
            return None;
        }
        drop(clipboard);
        let completed = clock::qpc().ok()?;
        Some(Read { notice: grant.notice, visit: grant.visit, completed, text })
    }

    fn excluded(&self) -> bool {
        if unsafe { IsClipboardFormatAvailable(self.1.monitor) }.is_ok() {
            return true;
        }
        if unsafe { IsClipboardFormatAvailable(self.1.history) }.is_err() {
            return false;
        }
        let Some(handle) = (unsafe { GetClipboardData(self.1.history) }).ok() else { return true };
        read_global(HGLOBAL(handle.0), 64, |data| {
            Some(u32::from_le_bytes(data.get(..4)?.try_into().ok()?) == 0)
        }).unwrap_or(true)
    }
}

impl Drop for ClipboardWindow {
    fn drop(&mut self) {
        let _ = unsafe { RemoveClipboardFormatListener(self.0) };
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

struct OpenedClipboard;

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

struct LockedMemory(HGLOBAL);

impl Drop for LockedMemory {
    fn drop(&mut self) {
        let _ = unsafe { GlobalUnlock(self.0) };
    }
}

fn read_global<T>(memory: HGLOBAL, max: usize, read: impl FnOnce(&[u8]) -> Option<T>) -> Option<T> {
    let bytes = unsafe { GlobalSize(memory) };
    if bytes == 0 || bytes > max {
        return None;
    }
    let pointer = unsafe { GlobalLock(memory) }.cast::<u8>();
    if pointer.is_null() {
        return None;
    }
    let _lock = LockedMemory(memory);
    // Clipboard keeps the owned HGLOBAL live; the lock pins the buffer.
    read(unsafe { std::slice::from_raw_parts(pointer, bytes) })
}

fn valid(grant: &Grant) -> bool {
    let notice = grant.notice;
    grant.focus.current() && notice.observed.elapsed() < MAX_AGE
        && unsafe { GetClipboardSequenceNumber() } == notice.sequence
        && clipboard_owner() == Some(notice.owner)
        && WindowProcess::foreground() == Some(grant.visit.foreground)
        && grant.visit.foreground == notice.foreground
        && grant.visit.clipboard_focus_is_current()
}
