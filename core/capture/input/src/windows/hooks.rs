//! One hook-only owner thread and two single-writer, single-reader rings.
//!
//! The Win32 contracts give valid pointers to exactly KBDLLHOOKSTRUCT or
//! MSLLHOOKSTRUCT for HC_ACTION. The callbacks copy those fields before
//! returning CallNextHookEx. Neither callback borrows native data past return,
//! allocates, locks, signals a consumer, or waits for queue space.

use std::{
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}},
        mpsc,
    },
    thread,
    time::Duration,
};

use screen_fix_protocol::{
    MessageTickMs, PhysicalPoint, QpcTick, SourceSequence,
    ipc::{CollectedInput, HelperFailureStage, HelperToCore, HookKey, HookPointer, InputSample,
        Producer, ProducerGap, ProducerProgress, Receipt},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Performance::QueryPerformanceCounter,
        Threading::GetCurrentThreadId,
    },
    UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
        PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_QUIT,
    },
};

use super::{NativeError, last_error};

const HOOK_CAPACITY: usize = 4096;
const WORDS: usize = 8;
type Words = [u64; WORDS];

const PROBE_PROGRESS: u32 = WM_APP + 2;
static PROGRESS_PENDING: AtomicBool = AtomicBool::new(false);

static KEYBOARD: HookRing = HookRing::new();
static POINTER: HookRing = HookRing::new();

pub(super) struct Hooks {
    thread_id: u32,
    owner: thread::JoinHandle<Result<(), NativeError>>,
}

impl Hooks {
    pub(super) fn start() -> Result<Self, NativeError> {
        let (ready, initialized) = mpsc::sync_channel(1);
        let owner = thread::Builder::new().name("input-hooks".into()).spawn(move || hook_loop(ready))
            .map_err(|error| NativeError {
                stage: HelperFailureStage::Startup,
                code: error.raw_os_error(),
            })?;
        match initialized.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(thread_id)) => Ok(Self { thread_id, owner }),
            Ok(Err(error)) => Err(error),
            // Coordinator reports a failure and exits the helper process.
            Err(_) => Err(NativeError::without_code(HelperFailureStage::MessageLoop)),
        }
    }

    pub(super) fn failure(&self) -> Option<NativeError> {
        if KEYBOARD.failed.load(Acquire) != 0 {
            Some(NativeError::without_code(HelperFailureStage::KeyboardHook))
        } else if POINTER.failed.load(Acquire) != 0 {
            Some(NativeError::without_code(HelperFailureStage::PointerHook))
        } else if self.owner.is_finished() {
            Some(NativeError::without_code(HelperFailureStage::MessageLoop))
        } else {
            None
        }
    }

    pub(super) fn request_progress(&self) -> Result<(), NativeError> {
        if PROGRESS_PENDING.swap(true, Relaxed) {
            return Ok(());
        }
        if unsafe { PostThreadMessageW(self.thread_id, PROBE_PROGRESS, 0, 0) } == 0 {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        Ok(())
    }

    pub(super) fn read(&mut self, producer: Producer) -> Option<HelperToCore> {
        match producer {
            Producer::KeyboardHook => KEYBOARD.pop().map(|words| unpack(producer, words)),
            Producer::PointerHook => POINTER.pop().map(|words| unpack(producer, words)),
            _ => None,
        }
    }

    pub(super) fn stop(self) -> Result<ClosedHooks, NativeError> {
        // GetMessage already has a thread queue when the owner reports ready.
        if !self.owner.is_finished()
            && unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } == 0
        {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        self.owner.join().map_err(|_| NativeError::without_code(HelperFailureStage::MessageLoop))??;
        Ok(ClosedHooks)
    }
}

pub(super) struct ClosedHooks;

impl ClosedHooks {
    pub(super) fn read(&mut self, producer: Producer) -> Option<HelperToCore> {
        let ring = match producer {
            Producer::KeyboardHook => &KEYBOARD,
            Producer::PointerHook => &POINTER,
            _ => return None,
        };
        // The hook thread is joined. No writer remains, so opening its final
        // pending gap here does not race a callback.
        ring.pop().or_else(|| ring.take_gap()).map(|words| unpack(producer, words))
    }
}

fn hook_loop(ready: mpsc::SyncSender<Result<u32, NativeError>>) -> Result<(), NativeError> {
    let mut message = MSG::default();
    // Ensure that a message queue exists before revealing the thread ID.
    unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE) };
    let (keyboard, pointer) = match install_hooks() {
        Ok(pair) => pair,
        Err(error) => {
            let _ = ready.try_send(Err(error));
            return Err(error);
        }
    };
    let _registration = (keyboard, pointer);
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready.try_send(Ok(thread_id)).is_err() {
        return Ok(());
    }
    loop {
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(last_error(HelperFailureStage::MessageLoop)),
            0 => return Ok(()),
            _ if message.hwnd.is_null() && message.message == PROBE_PROGRESS => {
                let qpc = callback_qpc();
                if qpc < 0 {
                    return Err(last_error(HelperFailureStage::MessageLoop));
                }
                KEYBOARD.progress(qpc);
                POINTER.progress(qpc);
                PROGRESS_PENDING.store(false, Release);
            }
            _ => {}
        }
    }
}

fn install_hooks() -> Result<(Hook, Hook), NativeError> {
    // Both hook bodies are functions in the current process image, which
    // remains loaded until after the hook thread releases its registrations.
    let module = unsafe { GetModuleHandleW(null()) };
    if module.is_null() {
        return Err(last_error(HelperFailureStage::Startup));
    }
    let keyboard = Hook::install(WH_KEYBOARD_LL, Some(key_callback), module, HelperFailureStage::KeyboardHook)?;
    let pointer = Hook::install(WH_MOUSE_LL, Some(pointer_callback), module, HelperFailureStage::PointerHook)?;
    Ok((keyboard, pointer))
}

struct Hook(HHOOK);

impl Hook {
    fn install(
        kind: i32,
        callback: windows_sys::Win32::UI::WindowsAndMessaging::HOOKPROC,
        module: windows_sys::Win32::Foundation::HINSTANCE,
        stage: HelperFailureStage,
    ) -> Result<Self, NativeError> {
        let handle = unsafe { SetWindowsHookExW(kind, callback, module, 0) };
        if handle.is_null() { Err(last_error(stage)) } else { Ok(Self(handle)) }
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        unsafe { UnhookWindowsHookEx(self.0) };
    }
}

unsafe extern "system" fn key_callback(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == 0 && std::panic::catch_unwind(|| {
        let qpc = callback_qpc();
        // HC_ACTION supplies a complete keyboard hook structure.
        let event = unsafe { *(data as *const KBDLLHOOKSTRUCT) };
        KEYBOARD.offer(qpc, [
            u64::from(event.time), message as u64, u64::from(event.vkCode),
            u64::from(event.scanCode), u64::from(event.flags), event.dwExtraInfo as u64,
        ]);
    }).is_err() {
        KEYBOARD.failed.store(1, Release);
    }
    // Negative codes and other non-action codes pass through unchanged.
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

unsafe extern "system" fn pointer_callback(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == 0 && std::panic::catch_unwind(|| {
        let qpc = callback_qpc();
        // HC_ACTION supplies a complete mouse hook structure.
        let event = unsafe { *(data as *const MSLLHOOKSTRUCT) };
        let point = (u64::from(event.pt.x as u32) << 32) | u64::from(event.pt.y as u32);
        POINTER.offer(qpc, [
            u64::from(event.time), message as u64, point, u64::from(event.mouseData),
            u64::from(event.flags), event.dwExtraInfo as u64,
        ]);
    }).is_err() {
        POINTER.failed.store(1, Release);
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

fn callback_qpc() -> i64 {
    let mut ticks = 0;
    if unsafe { QueryPerformanceCounter(&mut ticks) } == 0 {
        return -1;
    }
    ticks
}

struct HookRing {
    write: AtomicU64,
    read: AtomicU64,
    sequence: AtomicU64,
    failed: AtomicU64,
    gap: [AtomicU64; 4],
    slots: [[AtomicU64; WORDS]; HOOK_CAPACITY],
}

impl HookRing {
    const fn new() -> Self {
        Self {
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            gap: [const { AtomicU64::new(0) }; 4],
            slots: [const { [const { AtomicU64::new(0) }; WORDS] }; HOOK_CAPACITY],
        }
    }

    fn offer(&self, ticks: i64, payload: [u64; 6]) {
        let sequence = self.sequence.fetch_add(1, Relaxed).wrapping_add(1);
        if ticks < 0 || sequence == 0 {
            self.failed.store(1, Release);
            return;
        }
        if let Some(gap) = self.pending_gap() {
            if self.push(gap) {
                self.gap[0].store(0, Relaxed);
            } else {
                self.record_gap(sequence, ticks);
                return;
            }
        }
        let [tick, message, first, second, flags, extra] = payload;
        if !self.push([sequence, ticks as u64, tick, message, first, second, flags, extra]) {
            self.record_gap(sequence, ticks);
        }
    }

    fn progress(&self, qpc: i64) {
        // This runs on the callback owner between callbacks. A marker cannot
        // precede an in-flight callback or a missing callback range.
        if let Some(gap) = self.pending_gap() {
            if !self.push(gap) {
                return;
            }
            self.gap[0].store(0, Relaxed);
        }
        let _ = self.push([0, 0, qpc as u64, 0, 0, 0, 0, 0]);
    }

    fn record_gap(&self, sequence: u64, ticks: i64) {
        if self.gap[0].load(Relaxed) == 0 {
            self.gap[0].store(sequence, Relaxed);
            self.gap[1].store(ticks as u64, Relaxed);
        }
        self.gap[2].store(sequence, Relaxed);
        self.gap[3].store(ticks as u64, Relaxed);
    }

    fn pending_gap(&self) -> Option<Words> {
        let [first, first_qpc, last, last_qpc] = self.gap.each_ref().map(|value| value.load(Relaxed));
        (first != 0).then_some([0, first, first_qpc, last, last_qpc, 0, 0, 0])
    }

    fn take_gap(&self) -> Option<Words> {
        let gap = self.pending_gap()?;
        self.gap[0].store(0, Relaxed);
        Some(gap)
    }

    fn push(&self, words: Words) -> bool {
        let write = self.write.load(Relaxed);
        if write.wrapping_sub(self.read.load(Acquire)) == HOOK_CAPACITY as u64 {
            return false;
        }
        let slot = &self.slots[write as usize % HOOK_CAPACITY];
        for (field, value) in slot.iter().zip(words) {
            field.store(value, Relaxed);
        }
        self.write.store(write.wrapping_add(1), Release);
        true
    }

    fn pop(&self) -> Option<Words> {
        let read = self.read.load(Relaxed);
        if self.write.load(Acquire) == read {
            return None;
        }
        let words = self.slots[read as usize % HOOK_CAPACITY].each_ref().map(|value| value.load(Relaxed));
        self.read.store(read.wrapping_add(1), Release);
        Some(words)
    }
}

fn unpack(producer: Producer, words: Words) -> HelperToCore {
    let [sequence, qpc, tick, message, first, second, flags, extra] = words;
    if sequence == 0 && qpc == 0 {
        return HelperToCore::Progress(ProducerProgress { producer, before_qpc: QpcTick(tick as i64) });
    }
    if sequence == 0 {
        return HelperToCore::Gap(ProducerGap {
            producer,
            first_sequence: SourceSequence(qpc),
            first_qpc: QpcTick(tick as i64),
            last_sequence: SourceSequence(message),
            last_qpc: QpcTick(first as i64),
        });
    }
    let sample = match producer {
        Producer::KeyboardHook => InputSample::HookKey(HookKey {
            virtual_key: first as u32, scan_code: second as u32, flags: flags as u32,
            message: message as u32, extra_information: extra,
        }),
        _ => InputSample::HookPointer(HookPointer {
            point: PhysicalPoint { x: (first >> 32) as i32, y: first as i32 },
            mouse_data: second as u32, flags: flags as u32, message: message as u32,
            extra_information: extra,
        }),
    };
    HelperToCore::Input(CollectedInput {
        receipt: Receipt {
            sequence: SourceSequence(sequence),
            qpc: QpcTick(qpc as i64),
            message_tick_ms: Some(MessageTickMs(tick as u32)),
        },
        foreground_at_receipt: None,
        position_at_receipt: None,
        swapped_buttons: None,
        absolute_desktop: None,
        sample,
    })
}
