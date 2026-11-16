//! One native thread owns the recorder's registered system hotkeys.

use std::{
    collections::VecDeque,
    ptr::null_mut,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use crate::{
    CaptureError,
    protocol::{MessageTickMs, QpcTick},
    session::hotkey::{ControlLoss, HotkeyActivation, HotkeyBindings, RecorderAction},
};

use windows_sys::Win32::{
    Foundation::GetLastError,
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyboardLayoutList, HKL, MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
            MapVirtualKeyExW, RegisterHotKey, ToUnicodeEx, UnregisterHotKey,
        },
        WindowsAndMessaging::{DispatchMessageW, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, WM_HOTKEY, WM_QUIT},
    },
};

use super::clock;

const ACTIVE_POLL: Duration = Duration::from_millis(8);
const STARTUP_WAIT: Duration = Duration::from_secs(2);
const MAX_LAYOUTS: usize = 64;
const MAX_PENDING: usize = 16;

#[derive(Clone)]
pub struct RecorderHotkeys(Arc<Owner>);

struct Owner {
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
    keys: [u16; 4],
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    inbox: Mutex<HotkeyInbox>,
}

#[derive(Default)]
struct HotkeyInbox {
    active: bool,
    retired_at_ms: Option<u64>,
    batch: HotkeyBatch,
}

impl HotkeyInbox {
    fn ready_for_start(&self) -> bool {
        !self.active && self.retired_at_ms.is_none_or(|retired| clock::uptime_ms() > retired)
    }

    fn retire_attempt(&mut self) {
        // Retire controls still buffered for the completed attempt.
        self.retired_at_ms = Some(clock::uptime_ms());
        self.batch.events.clear();
        self.batch.stop = None;
        self.batch.loss = None;
        self.active = false;
    }

    fn accept(&mut self, activation: HotkeyActivation, received_ms: u64) {
        // WM_HOTKEY carries the posting tick, which can precede our read
        // from the OS queue or our acquisition of the inbox lock.
        let eligible = self.retired_at_ms.is_none_or(|retired| {
            let age = (received_ms as u32).wrapping_sub(activation.message_tick.0);
            if age > i32::MAX as u32 {
                return false;
            }
            // Native ticks are coarse. A message in the retirement tick
            // cannot prove it was posted after the previous attempt.
            received_ms.checked_sub(u64::from(age)).is_some_and(|posted| posted > retired)
        });
        if eligible {
            self.batch.push(activation);
        }
    }
}

#[derive(Default)]
pub(crate) struct HotkeyBatch {
    events: VecDeque<HotkeyActivation>,
    stop: Option<HotkeyActivation>,
    pub before: Option<QpcTick>,
    pub loss: Option<ControlLoss>,
    pub failed: bool,
}

impl HotkeyBatch {
    pub fn into_events(self) -> impl Iterator<Item = HotkeyActivation> {
        let mut events = self.events.into_iter().collect::<Vec<_>>();
        events.extend(self.stop);
        events.sort_unstable_by_key(|activation| activation.sequence);
        events.into_iter()
    }

    fn push(&mut self, activation: HotkeyActivation) {
        if self.events.len() < MAX_PENDING {
            self.events.push_back(activation);
        } else if activation.action == RecorderAction::Stop && self.stop.is_none() {
            self.stop = Some(activation);
        } else {
            ControlLoss::record(&mut self.loss, activation.at);
        }
    }
}

impl RecorderHotkeys {
    /// M1 uses the stub profile, which has no application-pack shortcuts.
    /// P12 supplies the settings UX over these same typed bindings.
    pub fn open(bindings: HotkeyBindings) -> Result<Self, CaptureError> {
        let shared = Arc::new(Shared::default());
        let owner_data = Arc::clone(&shared);
        let (ready, initialized) = mpsc::sync_channel(1);
        let owner = thread::Builder::new().name("recorder-hotkeys".into()).spawn(move || {
            match Registration::open(bindings) {
                Ok(registration) => {
                    let initialized = ready.try_send(Ok(()));
                    if initialized.is_ok() {
                        pump(&owner_data, &registration);
                    }
                }
                Err(error) => { let _ = ready.try_send(Err(error)); }
            }
        }).map_err(|error| CaptureError::backend("Start recorder hotkeys", error.raw_os_error(), error.to_string()))?;
        match initialized.recv_timeout(STARTUP_WAIT) {
            Ok(Ok(())) => Ok(Self(Arc::new(Owner { shared, thread: Some(owner), keys: bindings.virtual_keys() }))),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                shared.stop.store(true, Ordering::Release);
                owner.thread().unpark();
                Err(CaptureError::backend("Register recorder hotkeys", None, "Hotkey preflight did not complete.".into()))
            }
        }
    }

    /// Idle UI takes the request to start, then hands consumption to the
    /// controller. Any later system activations remain in their native order.
    pub fn take_start(&self) -> Option<HotkeyActivation> {
        let mut inbox = self.0.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        if !inbox.ready_for_start() {
            return None;
        }
        let batch = &mut inbox.batch;
        while let Some(event) = batch.events.pop_front() {
            if event.action == RecorderAction::Start {
                if batch.stop.is_some_and(|stop| stop.sequence < event.sequence) {
                    batch.stop = None;
                }
                return Some(event);
            }
        }
        batch.stop = None;
        batch.loss = None;
        None
    }

    pub fn available(&self) -> bool {
        !self.0.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner).batch.failed
    }

    pub fn ready_for_start(&self) -> bool {
        self.0.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner).ready_for_start()
    }

    pub(crate) fn claim(&self) -> Result<ActiveHotkeys, CaptureError> {
        let mut inbox = self.0.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        if inbox.active {
            return Err(CaptureError::backend("Use recorder hotkeys", None, "An attempt already owns the hotkey input.".into()));
        }
        if !inbox.ready_for_start() {
            return Err(CaptureError::backend("Use recorder hotkeys", None, "The previous hotkey commands are still clearing. Try Start again.".into()));
        }
        inbox.active = true;
        Ok(ActiveHotkeys(self.clone()))
    }
}

pub(crate) struct ActiveHotkeys(RecorderHotkeys);

impl ActiveHotkeys {
    pub(crate) fn virtual_keys(&self) -> [u16; 4] { (self.0).0.keys }

    pub(crate) fn drain(&self) -> HotkeyBatch {
        let mut inbox = (self.0).0.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        let pending = &mut inbox.batch;
        HotkeyBatch {
            events: std::mem::take(&mut pending.events), stop: pending.stop.take(),
            before: pending.before, loss: pending.loss.take(), failed: pending.failed,
        }
    }
}

impl Drop for ActiveHotkeys {
    fn drop(&mut self) {
        (self.0).0.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner).retire_attempt();
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(owner) = self.thread.take() {
            owner.thread().unpark();
            let _ = owner.join();
        }
    }
}

struct Registration {
    active: usize,
    virtual_keys: [u16; 4],
}

impl Registration {
    fn open(bindings: HotkeyBindings) -> Result<Self, CaptureError> {
        let mut message = MSG::default();
        // A NULL-window registration is delivered to this thread's queue.
        unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE) };
        let mut virtual_keys = [0_u16; 4];
        let mut layouts = [null_mut(); MAX_LAYOUTS];
        let count = unsafe { GetKeyboardLayoutList(0, null_mut()) };
        if count <= 0 || count as usize > MAX_LAYOUTS
            || unsafe { GetKeyboardLayoutList(MAX_LAYOUTS as i32, layouts.as_mut_ptr()) } != count
        {
            return Err(CaptureError::backend("Read installed keyboard layouts", None, "The hotkey layout inventory is unavailable.".into()));
        }
        for (index, action) in RecorderAction::ALL.into_iter().enumerate() {
            let vk = 0x6f + u16::from(bindings.function(action));
            if virtual_keys[..index].contains(&vk) {
                return Err(chord_failure(action, vk, None, "This chord is assigned to two recorder actions."));
            }
            for &layout in &layouts[..count as usize] {
                if layout.is_null() || yields_character(vk, layout) {
                    return Err(chord_failure(action, vk, None, "An installed keyboard layout maps this chord to a character or dead key."));
                }
            }
            virtual_keys[index] = vk;
        }
        let mut registration = Self { active: 0, virtual_keys };
        for (index, action) in RecorderAction::ALL.into_iter().enumerate() {
            let vk = virtual_keys[index];
            let ok = unsafe { RegisterHotKey(null_mut(), index as i32 + 1, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, u32::from(vk)) };
            if ok == 0 {
                let note = if vk == 0x7b {
                    "Windows reserves F12 for debugging. Select another function key if this chord is unavailable."
                } else {
                    "Windows could not reserve this chord. Select another function key."
                };
                return Err(chord_failure(action, vk, Some(unsafe { GetLastError() } as i32), note));
            }
            registration.active += 1;
        }
        Ok(registration)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        for index in 0..self.active {
            unsafe { UnregisterHotKey(null_mut(), index as i32 + 1) };
        }
    }
}

fn yields_character(vk: u16, layout: HKL) -> bool {
    let mut held = [0_u8; 256];
    for modifier in [0x11, 0x12, 0xa2, 0xa4] {
        held[modifier] = 0x80;
    }
    let mut output = [0_u16; 16];
    let scan = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC, layout) };
    (unsafe { ToUnicodeEx(u32::from(vk), scan, held.as_ptr(), output.as_mut_ptr(),
        output.len() as i32, 0x4, layout) }) != 0
}

fn chord_failure(action: RecorderAction, vk: u16, code: Option<i32>, message: &str) -> CaptureError {
    CaptureError::backend("Validate recorder hotkeys", code, format!(
        "{} (Ctrl+Alt+F{}). {message}", action.name(), vk - 0x6f,
    ))
}

fn pump(shared: &Shared, registration: &Registration) {
    let mut sequence = 0_u64;
    while !shared.stop.load(Ordering::Acquire) {
        let observed = clock::qpc();
        let mut received = Vec::new();
        for _ in 0..32 {
            let mut message = MSG::default();
            if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 { break; }
            if message.message == WM_QUIT {
                shared.inbox.lock().unwrap_or_else(PoisonError::into_inner).batch.failed = true;
                return;
            }
            if message.message != WM_HOTKEY {
                unsafe { DispatchMessageW(&message) };
                continue;
            }
            let Some(index) = message.wParam.checked_sub(1).filter(|index| *index < registration.active) else { continue };
            let vk = ((message.lParam as usize >> 16) & 0xffff) as u16;
            let flags = message.lParam as usize & 0xffff;
            if vk != registration.virtual_keys[index] || flags as u32 & (MOD_CONTROL | MOD_ALT) != (MOD_CONTROL | MOD_ALT) {
                continue;
            }
            let Ok(at) = clock::qpc() else {
                shared.inbox.lock().unwrap_or_else(PoisonError::into_inner).batch.failed = true;
                return;
            };
            if sequence == (1 << 63) - 1 {
                shared.inbox.lock().unwrap_or_else(PoisonError::into_inner).batch.failed = true;
                return;
            }
            sequence += 1;
            let activation = HotkeyActivation {
                action: RecorderAction::ALL[index],
                sequence, at, virtual_key: vk, message_tick: MessageTickMs(message.time),
            };
            received.push((activation, clock::uptime_ms()));
        }
        {
            let mut inbox = shared.inbox.lock().unwrap_or_else(PoisonError::into_inner);
            for (event, received_ms) in received {
                inbox.accept(event, received_ms);
            }
            match observed {
                Ok(before) => inbox.batch.before = Some(before),
                Err(_) => { inbox.batch.failed = true; return; }
            }
        }
        thread::park_timeout(ACTIVE_POLL);
    }
}
