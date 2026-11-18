//! One native thread owns the recorder's registered system hotkeys.

use std::{
    collections::VecDeque,
    ptr::null_mut,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use screen_fix_native_observation::{owner::{self, StartupError}, wake::WakeEvent};

use crate::{
    CaptureError,
    protocol::{MessageTickMs, QpcTick},
    session::hotkey::{ControlLoss, DEBUGGER_FUNCTION, HotkeyActivation, HotkeyBindings, RecorderAction},
};

use windows_sys::Win32::{
    Foundation::GetLastError,
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyboardLayoutList, HKL, MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
            MapVirtualKeyExW, RegisterHotKey, ToUnicodeEx, UnregisterHotKey, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_MENU,
        },
        WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, WM_HOTKEY, WM_QUIT,
        },
    },
};

use super::clock;

/// The longest wait for a message before the pump renews its progress mark.
/// It stays well below the controller's 250 ms hotkey progress gap.
const PROGRESS_INTERVAL_MS: u32 = 50;
const MAX_LAYOUTS: usize = 64;
const MAX_PENDING: usize = 16;
/// `ToUnicodeEx` flag bit 2: do not change the keyboard state.
const TO_UNICODE_KEEP_STATE: u32 = 0x4;

#[derive(Clone)]
pub struct RecorderHotkeys(Arc<Owner>);

struct Owner {
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
    keys: [u16; 4],
}

/// Called by the pump when `take_start` may return an activation or
/// `available` turned false.
pub type HotkeyNotifier = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    stop: AtomicBool,
    /// A manual-reset event set with `stop`, so the pump leaves its wait at once.
    wake: WakeEvent,
    inbox: Mutex<HotkeyInbox>,
    notify: HotkeyNotifier,
}

impl Shared {
    fn new(notify: HotkeyNotifier) -> Result<Self, CaptureError> {
        let wake = WakeEvent::new(true)
            .map_err(|error| controller_error("Start recorder hotkeys", error.raw_os_error(), error.to_string()))?;
        Ok(Self { stop: AtomicBool::new(false), wake, inbox: Mutex::default(), notify })
    }

    /// The pump stops for good: hotkeys are no longer available.
    fn fail(&self) {
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner).batch.failed = true;
        (self.notify)();
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        // A failed signal leaves the pump to notice `stop` within one interval.
        let _ = self.wake.set();
    }
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

    /// Whether `take_start` can return the accepted activation.
    fn accept(&mut self, activation: HotkeyActivation, received_ms: u64) -> bool {
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
        eligible && !self.active && activation.action == RecorderAction::Start
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
    /// The pump calls `notify` from its own thread; see [`HotkeyNotifier`].
    pub fn open(bindings: HotkeyBindings, notify: HotkeyNotifier) -> Result<Self, CaptureError> {
        let shared = Arc::new(Shared::new(notify)?);
        let owner_data = Arc::clone(&shared);
        let started = owner::spawn("recorder-hotkeys", false, || {}, move |ready| {
            match Registration::open(bindings) {
                Ok(registration) => {
                    if ready.ready(()) && pump(&owner_data, &registration).is_err() {
                        owner_data.fail();
                    }
                }
                Err(error) => ready.fail(error),
            }
        });
        match started {
            Ok((owner, ())) => Ok(Self(Arc::new(Owner { shared, thread: Some(owner.handle), keys: bindings.virtual_keys() }))),
            Err(StartupError::Spawn(error)) => Err(controller_error("Start recorder hotkeys", error.raw_os_error(), error.to_string())),
            Err(StartupError::Failed(error)) => Err(error),
            Err(StartupError::Timeout) => {
                shared.request_stop();
                Err(controller_error("Register recorder hotkeys", None, "Hotkey preflight did not complete."))
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
            return Err(controller_error("Use recorder hotkeys", None, "An attempt already owns the hotkey input."));
        }
        if !inbox.ready_for_start() {
            return Err(controller_error("Use recorder hotkeys", None, "The previous hotkey commands are still clearing. Try Start again."));
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
        self.shared.request_stop();
        if let Some(owner) = self.thread.take() {
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
            return Err(controller_error("Read installed keyboard layouts", None, "The hotkey layout inventory is unavailable."));
        }
        for (index, action) in RecorderAction::ALL.into_iter().enumerate() {
            let vk = bindings.virtual_key(action);
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
                let note = if HotkeyBindings::function_of(vk) == u16::from(DEBUGGER_FUNCTION) {
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
    for modifier in [VK_CONTROL, VK_MENU, VK_LCONTROL, VK_LMENU] {
        held[usize::from(modifier)] = 0x80;
    }
    let mut output = [0_u16; 16];
    let scan = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC, layout) };
    (unsafe { ToUnicodeEx(u32::from(vk), scan, held.as_ptr(), output.as_mut_ptr(),
        output.len() as i32, TO_UNICODE_KEEP_STATE, layout) }) != 0
}

/// A hotkey failure, owned by the controller.
fn controller_error(operation: &'static str, native_code: Option<i32>, message: impl Into<String>) -> CaptureError {
    CaptureError::owned(crate::package::diagnostics::FailureComponent::Controller, operation, native_code, message.into())
}

fn chord_failure(action: RecorderAction, vk: u16, code: Option<i32>, message: &str) -> CaptureError {
    controller_error("Validate recorder hotkeys", code, format!(
        "{} (Ctrl+Alt+F{}). {message}", action.name(), HotkeyBindings::function_of(vk),
    ))
}

/// The pump stopped for good: the thread's queue quit, a clock or wait call
/// failed, or the activation sequence ran out.
struct PumpFailed;

/// Deliver the hotkey activations until stop.
fn pump(shared: &Shared, registration: &Registration) -> Result<(), PumpFailed> {
    let mut sequence = 0_u64;
    while !shared.stop.load(Ordering::Acquire) {
        let observed = clock::qpc();
        let mut received = Vec::new();
        for _ in 0..32 {
            let mut message = MSG::default();
            if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 { break; }
            if message.message == WM_QUIT {
                return Err(PumpFailed);
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
            let at = clock::qpc().map_err(|_| PumpFailed)?;
            if sequence == (1 << 63) - 1 {
                return Err(PumpFailed);
            }
            sequence += 1;
            let activation = HotkeyActivation {
                action: RecorderAction::ALL[index],
                sequence, at, virtual_key: vk, message_tick: MessageTickMs(message.time),
            };
            received.push((activation, clock::uptime_ms()));
        }
        let startable = {
            let mut inbox = shared.inbox.lock().unwrap_or_else(PoisonError::into_inner);
            let mut startable = false;
            for (event, received_ms) in received {
                startable |= inbox.accept(event, received_ms);
            }
            // An early return releases the inbox before the caller fails the pump.
            inbox.batch.before = Some(observed.map_err(|_| PumpFailed)?);
            startable
        };
        if startable {
            (shared.notify)();
        }
        // Wake on a queued message, on stop, or after the progress interval.
        shared.wake.wait_messages(PROGRESS_INTERVAL_MS).map_err(|_| PumpFailed)?;
    }
    Ok(())
}
