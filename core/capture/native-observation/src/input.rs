mod hooks;
mod hid;
mod device;
mod raw;
mod raw_loop;
pub mod loss;

use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    ptr::null,
    sync::Arc,
    time::{Duration, Instant},
};

use screen_fix_protocol::ipc::{
    HelperFailure, HelperFailureStage, HelperToCore, HookTimeoutEvidence, InputRegistration, Producer,
    RawInputRegistrationMismatch,
};
use screen_fix_protocol::{QpcFrequency, WindowId};
use screen_fix_protocol::ipc::elevated::WindowInstance;
use windows_sys::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Performance::QueryPerformanceFrequency,
            Registry::{HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW},
            Threading::{CreateEventW, GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
                ProcessPowerThrottling, SetProcessInformation, WaitForMultipleObjects,
                WaitForSingleObject},
        },
    },
    w,
};

use self::{hooks::Hooks, raw_loop::RawOwner};

const HOOK_PRODUCERS: [Producer; 2] = [Producer::KeyboardHook, Producer::PointerHook];
const MAX_BATCH: usize = 512;
const HOOK_POLL_MS: u32 = 4;

#[derive(Debug, Clone, Copy)]
pub struct NativeError {
    pub stage: HelperFailureStage,
    pub code: Option<i32>,
    raw_input_registration: Option<RawInputRegistrationMismatch>,
}

impl NativeError {
    pub fn without_code(stage: HelperFailureStage) -> Self {
        Self { stage, code: None, raw_input_registration: None }
    }

    /// An I/O failure at `stage`, with its operating-system code if any.
    pub(crate) fn io(stage: HelperFailureStage, error: &io::Error) -> Self {
        Self { code: error.raw_os_error(), ..Self::without_code(stage) }
    }

    pub fn into_helper_failure(self) -> HelperFailure {
        HelperFailure {
            stage: self.stage,
            native_code: self.code,
            raw_input_registration: self.raw_input_registration,
        }
    }
}

pub fn last_error(stage: HelperFailureStage) -> NativeError {
    NativeError::io(stage, &io::Error::last_os_error())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct HookForeground {
    pub window: WindowId,
    pub thread: u32,
    pub pid: u32,
    pub serial: u64,
}

pub struct CaptureScope {
    pub foreground: WindowInstance,
    /// Cheap callback check, restricted to local atomic state.
    pub hook_open: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
    /// The raw owner can recheck desktop, target and visit away from callbacks.
    pub raw_open: Arc<dyn Fn() -> bool + Send + Sync>,
    pub serial: Arc<dyn Fn() -> u64 + Send + Sync>,
}

pub struct Settings {
    pub qpc_frequency: QpcFrequency,
    pub pen_hid: bool,
}

/// The collector owns delivery and backpressure. Producers never call it
/// from hook callbacks or Raw Input message dispatch.
pub trait Collector {
    fn stop_requested(&self) -> bool;
    fn take_environment(&mut self) -> u32;
    fn ready(&mut self, registration: InputRegistration) -> Result<(), NativeError>;
    fn offer(&mut self, record: HelperToCore) -> Result<(), NativeError>;
    fn offer_hook(&mut self, record: HelperToCore, _proof: Option<HookForeground>) -> Result<(), NativeError> {
        self.offer(record)
    }
    fn flush(&mut self) -> Result<(), NativeError>;
    fn finish(&mut self, owner: BorrowedHandle<'_>) -> Result<(), NativeError>;
}

/// Run the native producer owners until the collector stops or its owner exits.
/// The caller supplies a process/lifetime handle and a wake event for control.
pub fn run(
    settings: Settings,
    owner: BorrowedHandle<'_>,
    control_wake: BorrowedHandle<'_>,
    output: &mut impl Collector,
) -> Result<(), NativeError> {
    run_scoped(settings, owner, control_wake, output, None)
}

pub fn run_scoped(
    settings: Settings,
    owner: BorrowedHandle<'_>,
    control_wake: BorrowedHandle<'_>,
    output: &mut impl Collector,
    scope: Option<Arc<CaptureScope>>,
) -> Result<(), NativeError> {
    let mut frequency = 0;
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 {
        return Err(last_error(HelperFailureStage::ClockMismatch));
    }
    if u64::try_from(frequency).ok() != Some(settings.qpc_frequency.0.get()) {
        return Err(NativeError::without_code(HelperFailureStage::ClockMismatch));
    }
    if owner_exited(owner)? || output.stop_requested() { return Ok(()); }

    let throttle = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    if unsafe {
        SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling,
            (&throttle as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32)
    } == 0 {
        return Err(last_error(HelperFailureStage::PowerThrottling));
    }
    let hook_timeout = hook_timeout();
    let data_wake = unsafe { CreateEventW(null(), 0, 0, null()) };
    if data_wake.is_null() { return Err(last_error(HelperFailureStage::Startup)); }
    // CreateEventW returned a new unnamed auto-reset event.
    let data_wake = unsafe { OwnedHandle::from_raw_handle(data_wake) };
    let raw_wake = data_wake.try_clone().map_err(|error| NativeError::io(HelperFailureStage::Startup, &error))?;
    let mut hooks = Hooks::start(scope.clone())?;
    let mut raw = match RawOwner::start(raw_wake, frequency, settings.pen_hid, scope) {
        Ok(raw) => raw,
        Err(error) => { let _ = hooks.stop(); return Err(error); }
    };
    let [touchpad, multi_axis] = raw.optional();
    let ready = InputRegistration {
        raw_keyboard: true, raw_pointer: true,
        keyboard_hook: true, pointer_hook: true, pen_hid: settings.pen_hid, touchpad, multi_axis, hook_timeout,
    };
    let handles = [owner.as_raw_handle(), control_wake.as_raw_handle(), data_wake.as_raw_handle()];
    let result = output.ready(ready).and_then(|()| collect(output, &handles, &mut raw, &mut hooks));

    let stopped_hooks = hooks.stop();
    let stopped_raw = raw.finish(output.take_environment());
    if owner_exited(owner)? { return result; }
    // Stop both native owners first, then flush the records already collected.
    // The caller retains records already accepted before a producer failure.
    let raw_result = stopped_raw.and_then(|raw| {
        for record in raw.records.try_iter() { output.offer(record)?; }
        for gap in raw.losses.finish() { output.offer(HelperToCore::Gap(gap))?; }
        for record in raw.status.finish() { output.offer(record)?; }
        raw.result
    });
    let hooks_result = stopped_hooks.and_then(|mut hooks| {
        for producer in HOOK_PRODUCERS {
            while let Some((record, proof)) = hooks.read(producer) { output.offer_hook(record, proof)?; }
        }
        Ok(())
    });
    let final_flush = output.finish(owner);
    result.and(raw_result).and(hooks_result).and(final_flush)
}

fn collect(
    output: &mut impl Collector, handles: &[HANDLE; 3], raw: &mut RawOwner, hooks: &mut Hooks,
) -> Result<(), NativeError> {
    let mut next_progress = Instant::now();
    loop {
        let wait = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, HOOK_POLL_MS) };
        if wait == WAIT_OBJECT_0 { return Ok(()); }
        if wait != WAIT_TIMEOUT && wait != WAIT_OBJECT_0 + 1 && wait != WAIT_OBJECT_0 + 2 {
            return Err(last_error(HelperFailureStage::Transport));
        }
        // The closing path passes any coalesced notices to the Raw Input
        // owner before it drops its registration. This includes session end.
        if output.stop_requested() { return Ok(()); }
        let settings = output.take_environment();
        if settings != 0 { raw.notify(settings)?; }
        for record in raw.records.try_iter().take(MAX_BATCH) { output.offer(record)?; }
        for producer in HOOK_PRODUCERS {
            for _ in 0..MAX_BATCH {
                let Some((record, proof)) = hooks.read(producer) else { break };
                output.offer_hook(record, proof)?;
            }
        }
        output.flush()?;
        if let Some(failure) = hooks.failure() { return Err(failure); }
        if Instant::now() >= next_progress {
            hooks.request_progress()?;
            next_progress = Instant::now() + Duration::from_millis(12);
        }
        if raw.ended() { return Ok(()); }
    }
}

fn owner_exited(owner: BorrowedHandle<'_>) -> Result<bool, NativeError> {
    match unsafe { WaitForSingleObject(owner.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(last_error(HelperFailureStage::Transport)),
    }
}

/// The value is a REG_SZ or a REG_DWORD; Windows applies at most 1,000 ms (INP-12).
fn hook_timeout() -> HookTimeoutEvidence {
    let mut bytes = [0_u16; 32];
    let mut size = size_of::<[u16; 32]>() as u32;
    let mut kind = 0;
    let result = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, w!("Control Panel\\Desktop"), w!("LowLevelHooksTimeout"),
            RRF_RT_REG_SZ | RRF_RT_REG_DWORD, &mut kind, bytes.as_mut_ptr().cast(), &mut size)
    };
    let milliseconds = match (result, kind) {
        (ERROR_FILE_NOT_FOUND, _) => return HookTimeoutEvidence::Absent,
        (ERROR_SUCCESS, REG_DWORD) if size == 4 => Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 16)),
        (ERROR_SUCCESS, REG_SZ) => {
            let len = bytes.iter().position(|unit| *unit == 0).unwrap_or(bytes.len());
            String::from_utf16(&bytes[..len]).ok().and_then(|text| text.trim().parse::<u32>().ok())
        }
        _ => None,
    };
    milliseconds.map_or(HookTimeoutEvidence::Unavailable, |value| HookTimeoutEvidence::ConfiguredMs(value.min(1_000)))
}

