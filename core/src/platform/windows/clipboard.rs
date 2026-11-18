//! Opt-in clipboard notifications and delayed-rendering reads belong to this
//! owner alone. The context gate must approve one concrete change before a read.

use std::{
    mem::align_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::{Acquire, Release}},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
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
use windows_sys::Win32::System::Threading::INFINITE;

use screen_fix_native_observation::{LossCounter, LossSpan, owner::{self, StartupError}, wake::WakeEvent};

use crate::{protocol::{MessageTickMs, QpcTick}, platform::windows::controller::ActiveSourceProof};

use super::{
    ProbeError, clock, context::{FocusGuard, Visit, identity::WindowProcess},
    io_error, stage, unavailable,
};

const QUEUE: usize = 8;
const MAX_BYTES: usize = 64 * 1024;
pub(super) const MAX_AGE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq, Eq)]
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
    focus: Authorization,
}

enum Authorization {
    Ordinary(FocusGuard),
    Elevated { live: Arc<AtomicBool>, until: QpcTick, source: ActiveSourceProof },
}

pub(super) struct Permit {
    live: Arc<AtomicBool>,
    until: QpcTick,
}

impl Permit {
    pub fn until(&self) -> QpcTick { self.until }
}

impl Drop for Permit {
    fn drop(&mut self) { self.live.store(false, Release); }
}

pub(super) struct Read {
    pub notice: Notice,
    pub visit: Visit,
    pub started: QpcTick,
    pub completed: QpcTick,
    /// None when the text exceeds the 64 KiB bound (`too_large`, C3).
    pub text: Option<String>,
}

pub(super) struct Worker {
    stop: Arc<AtomicBool>,
    /// An auto-reset event that wakes the owner for a grant or for stop.
    wake: Arc<WakeEvent>,
    thread: thread::JoinHandle<()>,
    notices: Receiver<Notice>,
    grants: SyncSender<Grant>,
    results: Receiver<Read>,
    /// Notices and results the full queues could not hold (counts only).
    lost: Arc<LossCounter>,
}

impl Worker {
    pub fn start() -> Result<Self, ProbeError> {
        let (notice, notices) = sync_channel(QUEUE);
        let (grants, approved) = sync_channel(QUEUE);
        let (result, results) = sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let lost = Arc::new(LossCounter::default());
        let worker_lost = Arc::clone(&lost);
        let wake = Arc::new(WakeEvent::new(false)
            .map_err(|error| io_error("Create the clipboard owner wake", error))?);
        let owner_wake = Arc::clone(&wake);
        let started = owner::spawn("context-clipboard", false, || {}, move |ready| {
            let owner = match ClipboardWindow::open() {
                Ok(owner) => owner,
                Err(error) => {
                    ready.fail(error);
                    return;
                }
            };
            if !ready.ready(()) {
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
                            if let Some(observation) = observe(sequence, MessageTickMs(message.time))
                                && let Err(TrySendError::Full(observation)) = notice.try_send(observation)
                            {
                                worker_lost.note(LossSpan::at(observation.received));
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
                    if let Some(read) = owner.read(grant)
                        && let Err(TrySendError::Full(read)) = result.try_send(read)
                    {
                        worker_lost.note(LossSpan::at(read.notice.received));
                    }
                }
                // Wake on a clipboard or other message, a grant, or stop.
                if owner_wake.wait_messages(INFINITE).is_err() {
                    return;
                }
            }
        });
        let thread = match started {
            Ok((owner, ())) => owner.handle,
            Err(StartupError::Spawn(error)) => return Err(io_error("Start the opt-in clipboard owner", error)),
            Err(StartupError::Failed(error)) => return Err(error),
            Err(StartupError::Timeout) => return Err(unavailable("The clipboard owner did not start")),
        };
        Ok(Self { stop, wake, thread, notices, grants, results, lost })
    }

    /// The owner thread ended without being stopped (§6.2 row 17).
    pub fn exited(&self) -> bool { self.thread.is_finished() }

    pub fn take_loss(&self) -> Option<LossSpan> {
        self.lost.take()
    }

    pub fn notices(&self) -> impl Iterator<Item = Notice> + '_ {
        self.notices.try_iter().take(QUEUE)
    }

    /// False when the grant queue is full.
    pub fn grant(&self, notice: Notice, visit: Visit, focus: FocusGuard) -> bool {
        self.submit(Grant { notice, visit, focus: Authorization::Ordinary(focus) })
    }

    pub fn grant_elevated(&self, notice: Notice, visit: Visit, until: QpcTick, source: ActiveSourceProof) -> Option<Permit> {
        let live = Arc::new(AtomicBool::new(true));
        let focus = Authorization::Elevated { live: Arc::clone(&live), until, source };
        self.submit(Grant { notice, visit, focus }).then_some(Permit { live, until })
    }

    fn submit(&self, grant: Grant) -> bool {
        if self.grants.try_send(grant).is_err() { return false; }
        self.wake_owner();
        true
    }

    fn wake_owner(&self) {
        // A failed signal leaves the grant for the owner's next message.
        let _ = self.wake.set();
    }

    pub fn results(&self) -> impl Iterator<Item = Read> + '_ {
        self.results.try_iter().take(QUEUE)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Release);
        self.wake_owner();
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
        let started = clock::qpc().ok()?;
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
        let memory = HGLOBAL(handle.0);
        // Text over the bound is recorded as a change without content.
        let text = if unsafe { GlobalSize(memory) } > MAX_BYTES { None } else {
            Some(read_global(memory, MAX_BYTES, |data| {
                if data.len() < 2 || data.len() % 2 != 0 || (data.as_ptr() as usize) % align_of::<u16>() != 0 {
                    return None;
                }
                let units = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u16>(), data.len() / 2) };
                let end = units.iter().position(|unit| *unit == 0)?;
                let text = String::from_utf16(&units[..end]).ok()?;
                (!text.is_empty()).then_some(text)
            })?).filter(|text| text.len() <= MAX_BYTES)
        };
        if !valid(&grant) {
            return None;
        }
        drop(clipboard);
        let completed = clock::qpc().ok()?;
        Some(Read { notice: grant.notice, visit: grant.visit, started, completed, text })
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

pub(super) fn notice_current(notice: Notice, visit: Visit) -> bool {
    notice.observed.elapsed() < MAX_AGE
        && unsafe { GetClipboardSequenceNumber() } == notice.sequence
        && clipboard_owner() == Some(notice.owner)
        && WindowProcess::foreground() == Some(visit.foreground)
        && visit.foreground == notice.foreground
}

fn valid(grant: &Grant) -> bool {
    let authorized = match &grant.focus {
        Authorization::Ordinary(focus) => focus.current() && grant.visit.clipboard_focus_is_current(),
        Authorization::Elevated { live, until, source } => live.load(Acquire) && source.current()
            && clock::qpc().is_ok_and(|now| now < *until)
            && grant.visit.elevated_clipboard_focus_is_current(),
    };
    authorized && notice_current(grant.notice, grant.visit)
}
