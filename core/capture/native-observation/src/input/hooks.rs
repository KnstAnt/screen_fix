//! One hook-only owner thread and two single-writer, single-reader rings.
//!
//! The Win32 contracts give valid pointers to exactly KBDLLHOOKSTRUCT or
//! MSLLHOOKSTRUCT for HC_ACTION. The callbacks copy those fields before
//! returning CallNextHookEx. Neither callback borrows native data past return,
//! allocates, locks, signals a consumer, or waits for queue space.

use std::{
    cell::OnceCell,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::{Acquire, Relaxed, Release}},
    },
};

use screen_fix_protocol::{
    MessageTickMs, PhysicalPoint, QpcTick, SourceSequence, WindowId,
    ipc::{CollectedInput, HelperFailureStage, HelperToCore, HookKey, HookPointer, InputSample,
        GapCause, Producer, ProducerGap, ProducerProgress, Receipt},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Performance::QueryPerformanceCounter,
    },
    UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
        PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP,
    },
};

use crate::{StaticLease, focus::window_owner, owner::{self, OwnerThread, Startup, StartupError}};

use super::{HookForeground, InputScope, NativeError, last_error};

const HOOK_CAPACITY: usize = 4096;
/// Words per ring slot (see `Entry::encode`).
const WORDS: usize = 12;
type Words = [u64; WORDS];

const PROBE_PROGRESS: u32 = WM_APP + 2;
static PROGRESS_PENDING: AtomicBool = AtomicBool::new(false);

static KEYBOARD: HookRing = HookRing::new();
static POINTER: HookRing = HookRing::new();
/// Set while one owner holds the static rings, which have a single writer
/// and a single reader.
static RINGS_CLAIMED: AtomicBool = AtomicBool::new(false);

pub(super) struct Hooks {
    owner: OwnerThread<Result<(), NativeError>>,
    /// The exclusive claim on the static rings.
    claim: StaticLease,
}

impl Hooks {
    pub(super) fn start(scope: Option<Arc<dyn InputScope>>) -> Result<Self, NativeError> {
        let claim = StaticLease::claim(&RINGS_CLAIMED)
            .ok_or_else(|| NativeError::without_code(HelperFailureStage::Startup))?;
        KEYBOARD.reset();
        POINTER.reset();
        PROGRESS_PENDING.store(false, Release);
        let scoped = scope.is_some();
        match owner::spawn("input-hooks", scoped, || {}, move |ready| hook_loop(ready, scope)) {
            Ok((owner, ())) => Ok(Self { owner, claim }),
            Err(StartupError::Spawn(error)) => Err(NativeError::io(HelperFailureStage::Startup, &error)),
            Err(StartupError::Failed(error)) => Err(error),
            Err(StartupError::Timeout) => Err(NativeError::without_code(HelperFailureStage::MessageLoop)),
        }
    }

    pub(super) fn failure(&self) -> Option<NativeError> {
        if KEYBOARD.failed.load(Acquire) {
            Some(NativeError::without_code(HelperFailureStage::KeyboardHook))
        } else if POINTER.failed.load(Acquire) {
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
        if unsafe { PostThreadMessageW(self.owner.thread_id, PROBE_PROGRESS, 0, 0) } == 0 {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        Ok(())
    }

    pub(super) fn read(&mut self, producer: Producer) -> Option<(HelperToCore, Option<HookForeground>)> {
        match producer {
            Producer::KeyboardHook => KEYBOARD.pop().map(|entry| entry.record(producer)),
            Producer::PointerHook => POINTER.pop().map(|entry| entry.record(producer)),
            _ => None,
        }
    }

    pub(super) fn stop(self) -> Result<ClosedHooks, NativeError> {
        self.owner.post_quit_and_join()
            .map_err(|error| NativeError::io(HelperFailureStage::MessageLoop, &error))?
            .map_err(|_| NativeError::without_code(HelperFailureStage::MessageLoop))??;
        Ok(ClosedHooks { _claim: self.claim })
    }
}

/// The rings of joined hooks, still claimed until they are drained.
pub(super) struct ClosedHooks {
    _claim: StaticLease,
}

impl ClosedHooks {
    pub(super) fn read(&mut self, producer: Producer) -> Option<(HelperToCore, Option<HookForeground>)> {
        let ring = match producer {
            Producer::KeyboardHook => &KEYBOARD,
            Producer::PointerHook => &POINTER,
            _ => return None,
        };
        // The hook thread is joined. No writer remains, so opening its final
        // pending gap here does not race a callback.
        ring.pop().or_else(|| ring.gap.take()).map(|entry| entry.record(producer))
    }
}

thread_local! {
    /// Set once on the hook thread before the hooks are installed. The
    /// callbacks run on that thread and only read it (ARCH-2).
    static SCOPE: OnceCell<Option<Arc<dyn InputScope>>> = const { OnceCell::new() };
}

fn hook_loop(ready: Startup<(), NativeError>, scope: Option<Arc<dyn InputScope>>) -> Result<(), NativeError> {
    // Each hook thread is new, so its cell is still empty here.
    SCOPE.with(|state| { let _ = state.set(scope); });
    let mut message = MSG::default();
    // Ensure that a message queue exists before revealing the thread ID.
    unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE) };
    let (keyboard, pointer) = match install_hooks() {
        Ok(pair) => pair,
        Err(error) => {
            ready.fail(error);
            return Err(error);
        }
    };
    let _registration = (keyboard, pointer);
    if !ready.ready(()) {
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
        let Some(proof) = hook_foreground() else { return };
        let qpc = callback_qpc();
        // HC_ACTION supplies a complete keyboard hook structure.
        let event = unsafe { *(data as *const KBDLLHOOKSTRUCT) };
        if hook_foreground() != Some(proof) { return; }
        KEYBOARD.offer(qpc, event.time, proof, Sample::Key(HookKey {
            virtual_key: event.vkCode, scan_code: event.scanCode, flags: event.flags,
            message: message as u32, extra_information: event.dwExtraInfo as u64,
        }));
    }).is_err() {
        KEYBOARD.failed.store(true, Release);
    }
    // Negative codes and other non-action codes pass through unchanged.
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

unsafe extern "system" fn pointer_callback(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == 0 && std::panic::catch_unwind(|| {
        let Some(proof) = hook_foreground() else { return };
        let qpc = callback_qpc();
        // HC_ACTION supplies a complete mouse hook structure.
        let event = unsafe { *(data as *const MSLLHOOKSTRUCT) };
        if hook_foreground() != Some(proof) { return; }
        POINTER.offer(qpc, event.time, proof, Sample::Pointer(HookPointer {
            point: PhysicalPoint { x: event.pt.x, y: event.pt.y }, mouse_data: event.mouseData,
            flags: event.flags, message: message as u32, extra_information: event.dwExtraInfo as u64,
        }));
    }).is_err() {
        POINTER.failed.store(true, Release);
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

fn hook_foreground() -> Option<Option<HookForeground>> {
    SCOPE.with(|scope| {
        let Some(scope) = scope.get().and_then(Option::as_ref) else { return Some(None) };
        let serial = scope.hook_open()?;
        let hwnd = unsafe { GetForegroundWindow() };
        let owner = window_owner(hwnd)?;
        let window = WindowId(hwnd as usize as u64);
        let foreground = scope.foreground();
        if window != foreground.window || owner.pid != foreground.process.pid || owner.thread != foreground.thread {
            return None;
        }
        Some(Some(HookForeground { window, pid: owner.pid.get(), thread: owner.thread.get(), serial }))
    })
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
    failed: AtomicBool,
    gap: PendingGap,
    slots: [[AtomicU64; WORDS]; HOOK_CAPACITY],
}

impl HookRing {
    const fn new() -> Self {
        Self {
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            gap: PendingGap::new(),
            slots: [const { [const { AtomicU64::new(0) }; WORDS] }; HOOK_CAPACITY],
        }
    }

    fn reset(&self) {
        self.write.store(0, Relaxed);
        self.read.store(0, Relaxed);
        self.sequence.store(0, Relaxed);
        self.failed.store(false, Relaxed);
        self.gap.clear();
    }

    fn offer(&self, ticks: i64, tick: u32, proof: Option<HookForeground>, sample: Sample) {
        let sequence = self.sequence.fetch_add(1, Relaxed).wrapping_add(1);
        if ticks < 0 || sequence == 0 {
            self.failed.store(true, Release);
            return;
        }
        if !self.push_gap() || !self.push(Entry::Event { sequence, qpc: ticks, tick, sample, proof }) {
            self.gap.record(sequence, ticks);
        }
    }

    fn progress(&self, qpc: i64) {
        // This runs on the callback owner between callbacks. A marker cannot
        // precede an in-flight callback or a missing callback range.
        if self.push_gap() {
            let _ = self.push(Entry::Progress(qpc));
        }
    }

    /// Push the pending gap, if any, ahead of the next entry. False while it
    /// stays pending: no later entry may overtake it (E3).
    fn push_gap(&self) -> bool {
        let Some(gap) = self.gap.pending() else { return true };
        let pushed = self.push(gap);
        if pushed { self.gap.clear(); }
        pushed
    }

    fn push(&self, entry: Entry) -> bool {
        let write = self.write.load(Relaxed);
        if write.wrapping_sub(self.read.load(Acquire)) == HOOK_CAPACITY as u64 {
            return false;
        }
        let slot = &self.slots[write as usize % HOOK_CAPACITY];
        for (field, value) in slot.iter().zip(entry.encode()) {
            field.store(value, Relaxed);
        }
        self.write.store(write.wrapping_add(1), Release);
        true
    }

    fn pop(&self) -> Option<Entry> {
        let read = self.read.load(Relaxed);
        if self.write.load(Acquire) == read {
            return None;
        }
        let words = self.slots[read as usize % HOOK_CAPACITY].each_ref().map(|value| value.load(Relaxed));
        self.read.store(read.wrapping_add(1), Release);
        Some(Entry::decode(words))
    }
}

/// The bounds of the callbacks lost since the last pushed entry. Only the
/// callback owner writes them, or the collector once that owner is joined.
struct PendingGap {
    /// Zero while no gap is pending; a recorded sequence is never zero.
    first_sequence: AtomicU64,
    first_qpc: AtomicI64,
    last_sequence: AtomicU64,
    last_qpc: AtomicI64,
}

impl PendingGap {
    const fn new() -> Self {
        Self {
            first_sequence: AtomicU64::new(0), first_qpc: AtomicI64::new(0),
            last_sequence: AtomicU64::new(0), last_qpc: AtomicI64::new(0),
        }
    }

    /// Open a gap at the lost callback, or extend the pending one to it.
    fn record(&self, sequence: u64, qpc: i64) {
        if self.first_sequence.load(Relaxed) == 0 {
            self.first_sequence.store(sequence, Relaxed);
            self.first_qpc.store(qpc, Relaxed);
        }
        self.last_sequence.store(sequence, Relaxed);
        self.last_qpc.store(qpc, Relaxed);
    }

    fn pending(&self) -> Option<Entry> {
        let first_sequence = self.first_sequence.load(Relaxed);
        (first_sequence != 0).then(|| Entry::Gap {
            first_sequence,
            first_qpc: self.first_qpc.load(Relaxed),
            last_sequence: self.last_sequence.load(Relaxed),
            last_qpc: self.last_qpc.load(Relaxed),
        })
    }

    fn clear(&self) {
        self.first_sequence.store(0, Relaxed);
    }

    fn take(&self) -> Option<Entry> {
        let gap = self.pending()?;
        self.clear();
        Some(gap)
    }
}

/// The sample of one hook callback.
#[derive(Clone, Copy)]
enum Sample {
    Key(HookKey),
    Pointer(HookPointer),
}

/// One ring slot. `encode` and `decode` define the slot layout together: the
/// first word is the tag, and each variant owns the words after it.
#[derive(Clone, Copy)]
enum Entry {
    Event { sequence: u64, qpc: i64, tick: u32, sample: Sample, proof: Option<HookForeground> },
    Gap { first_sequence: u64, first_qpc: i64, last_sequence: u64, last_qpc: i64 },
    Progress(i64),
}

const TAG_KEY: u64 = 1;
const TAG_POINTER: u64 = 2;
const TAG_GAP: u64 = 3;
const TAG_PROGRESS: u64 = 4;

impl Entry {
    fn encode(self) -> Words {
        match self {
            Self::Event { sequence, qpc, tick, sample, proof } => {
                let (tag, message, first, second, flags, extra) = match sample {
                    Sample::Key(key) => (
                        TAG_KEY, key.message, u64::from(key.virtual_key), u64::from(key.scan_code),
                        key.flags, key.extra_information,
                    ),
                    Sample::Pointer(pointer) => (
                        TAG_POINTER, pointer.message,
                        (u64::from(pointer.point.x as u32) << 32) | u64::from(pointer.point.y as u32),
                        u64::from(pointer.mouse_data), pointer.flags, pointer.extra_information,
                    ),
                };
                // A proof always has a nonzero process ID, so its `owner` word is nonzero.
                let (window, owner, serial) = proof.map_or((0, 0, 0), |proof| {
                    (proof.window.0, (u64::from(proof.pid) << 32) | u64::from(proof.thread), proof.serial)
                });
                [
                    tag, sequence, qpc as u64, u64::from(tick), u64::from(message), first, second,
                    u64::from(flags), extra, window, owner, serial,
                ]
            }
            Self::Gap { first_sequence, first_qpc, last_sequence, last_qpc } =>
                [TAG_GAP, first_sequence, first_qpc as u64, last_sequence, last_qpc as u64, 0, 0, 0, 0, 0, 0, 0],
            Self::Progress(qpc) => [TAG_PROGRESS, qpc as u64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        }
    }

    fn decode(words: Words) -> Self {
        let [tag, sequence, qpc, tick, message, first, second, flags, extra, window, owner, serial] = words;
        match tag {
            // The gap and progress layouts reuse the leading words.
            TAG_PROGRESS => Self::Progress(sequence as i64),
            TAG_GAP => Self::Gap {
                first_sequence: sequence, first_qpc: qpc as i64, last_sequence: tick, last_qpc: message as i64,
            },
            TAG_KEY | TAG_POINTER => {
                let (message, flags) = (message as u32, flags as u32);
                let sample = if tag == TAG_KEY {
                    Sample::Key(HookKey {
                        virtual_key: first as u32, scan_code: second as u32, flags, message, extra_information: extra,
                    })
                } else {
                    Sample::Pointer(HookPointer {
                        point: PhysicalPoint { x: (first >> 32) as i32, y: first as i32 },
                        mouse_data: second as u32, flags, message, extra_information: extra,
                    })
                };
                let proof = (owner != 0).then_some(HookForeground {
                    window: WindowId(window), pid: (owner >> 32) as u32, thread: owner as u32, serial,
                });
                Self::Event { sequence, qpc: qpc as i64, tick: tick as u32, sample, proof }
            }
            _ => unreachable!("every ring slot is written by `Entry::encode`"),
        }
    }

    /// The record of `producer` this entry carries, with its foreground proof.
    fn record(self, producer: Producer) -> (HelperToCore, Option<HookForeground>) {
        match self {
            Self::Progress(qpc) => (HelperToCore::Progress(ProducerProgress { producer, before_qpc: QpcTick(qpc) }), None),
            Self::Gap { first_sequence, first_qpc, last_sequence, last_qpc } => (HelperToCore::Gap(ProducerGap {
                producer, cause: GapCause::Overflow,
                first_sequence: SourceSequence(first_sequence),
                first_qpc: QpcTick(first_qpc),
                last_sequence: SourceSequence(last_sequence),
                last_qpc: QpcTick(last_qpc),
            }), None),
            Self::Event { sequence, qpc, tick, sample, proof } => {
                let sample = match sample {
                    Sample::Key(key) => InputSample::HookKey(key),
                    Sample::Pointer(pointer) => InputSample::HookPointer(pointer),
                };
                let record = HelperToCore::Input(CollectedInput {
                    receipt: Receipt {
                        sequence: SourceSequence(sequence),
                        qpc: QpcTick(qpc),
                        message_tick_ms: Some(MessageTickMs(tick)),
                    },
                    foreground_at_receipt: None,
                    position_at_receipt: None,
                    swapped_buttons: None,
                    absolute_desktop: None,
                    sample,
                });
                (record, proof)
            }
        }
    }
}
