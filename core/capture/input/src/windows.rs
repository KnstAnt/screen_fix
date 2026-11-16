mod hooks;
mod hid;
mod device;
mod raw;
mod raw_loop;
mod loss;

use std::io;
use screen_fix_protocol::ipc::HelperFailureStage;

#[derive(Debug, Clone, Copy)]
struct NativeError {
    stage: HelperFailureStage,
    code: Option<i32>,
}

impl NativeError {
    fn without_code(stage: HelperFailureStage) -> Self {
        Self { stage, code: None }
    }
}

fn last_error(stage: HelperFailureStage) -> NativeError {
    NativeError { stage, code: io::Error::last_os_error().raw_os_error() }
}

use std::{
    mem::size_of,
    os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    process::ExitCode,
    ptr::null,
    time::{Duration, Instant},
};

use screen_fix_protocol::ipc::{
    CoreToHelper, EnvironmentState, HelperFailure, HelperToCore, HookTimeoutEvidence,
    InputRegistration, Producer, shared::HelperEndpoint,
};
use windows_sys::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetHandleInformation, HANDLE,
            HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Performance::QueryPerformanceFrequency,
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW},
            Threading::{CreateEventW, GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
                ProcessPowerThrottling, SetProcessInformation, WaitForMultipleObjects,
                WaitForSingleObject},
        },
    },
    w,
};

use self::{hooks::Hooks, loss::Losses, raw_loop::RawOwner};

const HOOK_PRODUCERS: [Producer; 2] = [Producer::KeyboardHook, Producer::PointerHook];
const MAX_BATCH: usize = 512;
const HOOK_POLL_MS: u32 = 4;
const FINAL_FLUSH: Duration = Duration::from_millis(400);

pub(super) fn run_child() -> ExitCode {
    let Some([mapping, wake, parent]) = inherited_handles() else { return ExitCode::FAILURE };
    // The parent passed this mapping through CreateProcess's explicit
    // inheritance list. Each process has just one owning endpoint.
    let Ok(channel) = (unsafe { HelperEndpoint::attach(mapping.as_handle()) }) else {
        return ExitCode::FAILURE;
    };
    drop(mapping);
    let mut output = Output::new(channel);
    if let Err(error) = run(&mut output, wake, parent) {
        let _ = output.status(HelperToCore::Failed(HelperFailure { stage: error.stage, native_code: error.code }));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn inherited_handles() -> Option<[OwnedHandle; 3]> {
    let mut args = std::env::args_os().skip(1);
    if args.next()?.to_str()? != "--recorder-child" { return None; }
    let handles: [usize; 3] = [
        args.next()?.to_str()?.parse().ok()?,
        args.next()?.to_str()?.parse().ok()?,
        args.next()?.to_str()?.parse().ok()?,
    ];
    if args.next().is_some() || handles.contains(&0) || handles.contains(&usize::MAX)
        || handles[0] == handles[1] || handles[0] == handles[2] || handles[1] == handles[2]
    {
        return None;
    }
    for value in handles {
        let mut attributes = 0;
        let handle = value as HANDLE;
        if unsafe { GetHandleInformation(handle, &mut attributes) } == 0
            || attributes & HANDLE_FLAG_INHERIT == 0
            || unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0
        {
            return None;
        }
    }
    // The successful checks above prove three distinct, live inherited handles
    // in this child. Ownership transfers once and their inheritance is cleared.
    Some(handles.map(|value| unsafe { OwnedHandle::from_raw_handle(value as HANDLE) }))
}

fn run(output: &mut Output, wake: OwnedHandle, parent: OwnedHandle) -> Result<(), NativeError> {
    let Some(CoreToHelper::Configure(config)) = output.channel.receive_configuration()
        .map_err(|_| transport_error())? else {
        return Err(transport_error());
    };
    let mut frequency = 0;
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 {
        return Err(last_error(HelperFailureStage::ClockMismatch));
    }
    if u64::try_from(frequency).ok() != Some(config.qpc_frequency.0.get()) {
        return Err(NativeError::without_code(HelperFailureStage::ClockMismatch));
    }
    if parent_exited(&parent)? || output.channel.stop_requested() { return Ok(()); }

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
    let raw_wake = data_wake.try_clone().map_err(|error| NativeError {
        stage: HelperFailureStage::Startup, code: error.raw_os_error(),
    })?;
    let mut hooks = Hooks::start()?;
    let mut raw = match RawOwner::start(raw_wake, frequency, config.pen_hid) {
        Ok(raw) => raw,
        Err(error) => { let _ = hooks.stop(); return Err(error); }
    };
    let ready = HelperToCore::Ready(InputRegistration {
        raw_keyboard: true, raw_pointer: true,
        keyboard_hook: true, pointer_hook: true, pen_hid: config.pen_hid, hook_timeout,
    });
    let handles = [parent.as_raw_handle(), wake.as_raw_handle(), data_wake.as_raw_handle()];
    let result = output.status(ready).and_then(|()| collect(output, &handles, &mut raw, &mut hooks));

    let stopped_hooks = hooks.stop();
    let stopped_raw = raw.finish(output.channel.take_environment());
    if parent_exited(&parent)? { return result; }
    // Stop both native owners first, then flush the records already collected.
    // A failed owner still has any records committed to the shared ring.
    let raw_result = stopped_raw.and_then(|raw| {
        for record in raw.records.try_iter() { output.offer(record)?; }
        for gap in raw.losses.finish() { output.offer(HelperToCore::Gap(gap))?; }
        if let Some(environment) = raw.environment { output.offer(HelperToCore::Environment(environment))?; }
        raw.result
    });
    let hooks_result = stopped_hooks.and_then(|mut hooks| {
        for producer in HOOK_PRODUCERS {
            while let Some(record) = hooks.read(producer) { output.offer(record)?; }
        }
        Ok(())
    });
    let final_flush = output.flush_final(&parent);
    result.and(raw_result).and(hooks_result).and(final_flush)
}

fn collect(
    output: &mut Output, handles: &[HANDLE; 3], raw: &mut RawOwner, hooks: &mut Hooks,
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
        if output.channel.stop_requested() { return Ok(()); }
        let settings = output.channel.take_environment();
        if settings != 0 { raw.notify(settings)?; }
        for record in raw.records.try_iter().take(MAX_BATCH) { output.offer(record)?; }
        for producer in HOOK_PRODUCERS {
            for _ in 0..MAX_BATCH {
                let Some(record) = hooks.read(producer) else { break };
                output.offer(record)?;
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

fn parent_exited(parent: &OwnedHandle) -> Result<bool, NativeError> {
    match unsafe { WaitForSingleObject(parent.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(last_error(HelperFailureStage::Transport)),
    }
}

fn hook_timeout() -> HookTimeoutEvidence {
    let mut bytes = [0_u16; 32];
    let mut size = size_of::<[u16; 32]>() as u32;
    let result = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, w!("Control Panel\\Desktop"), w!("LowLevelHooksTimeout"),
            RRF_RT_REG_SZ, std::ptr::null_mut(), bytes.as_mut_ptr().cast(), &mut size)
    };
    match result {
        ERROR_FILE_NOT_FOUND => HookTimeoutEvidence::Absent,
        ERROR_SUCCESS => {
            let len = bytes.iter().position(|unit| *unit == 0).unwrap_or(bytes.len());
            String::from_utf16(&bytes[..len]).ok().and_then(|text| text.trim().parse().ok())
                .map(HookTimeoutEvidence::ConfiguredMs).unwrap_or(HookTimeoutEvidence::Unavailable)
        }
        _ => HookTimeoutEvidence::Unavailable,
    }
}

struct Output {
    channel: HelperEndpoint,
    losses: Losses,
    environment: Option<EnvironmentState>,
}

impl Output {
    fn new(channel: HelperEndpoint) -> Self {
        Self { channel, losses: Losses::default(), environment: None }
    }

    fn status(&mut self, record: HelperToCore) -> Result<(), NativeError> {
        if self.channel.send_status(&record).map_err(|_| transport_error())? {
            Ok(())
        } else {
            Err(transport_error())
        }
    }

    fn offer(&mut self, mut record: HelperToCore) -> Result<(), NativeError> {
        if let Some(producer) = loss::producer(&record)
            && !self.flush_loss(producer)?
        {
            self.losses.drop_record(&record);
            return Ok(());
        }
        if let HelperToCore::Environment(state) = &mut record
            && let Some(pending) = self.environment.take()
        {
            state.changes |= pending.changes;
        }
        if !self.channel.send(&record).map_err(|_| transport_error())? {
            if let HelperToCore::Environment(state) = record {
                self.environment = Some(state);
            } else {
                self.losses.drop_record(&record);
            }
        }
        Ok(())
    }

    fn flush_loss(&mut self, producer: Producer) -> Result<bool, NativeError> {
        let Some(gap) = self.losses.pending(producer) else { return Ok(true) };
        let sent = self.channel.send(&HelperToCore::Gap(gap)).map_err(|_| transport_error())?;
        if sent { self.losses.clear(producer); }
        Ok(sent)
    }

    fn flush(&mut self) -> Result<bool, NativeError> {
        for producer in [Producer::RawInput, Producer::DeviceNotifications, Producer::KeyboardHook, Producer::PointerHook] {
            if !self.flush_loss(producer)? { return Ok(false); }
        }
        if let Some(state) = self.environment {
            if !self.channel.send(&HelperToCore::Environment(state)).map_err(|_| transport_error())? {
                return Ok(false);
            }
            self.environment = None;
        }
        Ok(true)
    }

    fn flush_final(&mut self, parent: &OwnedHandle) -> Result<(), NativeError> {
        let deadline = Instant::now() + FINAL_FLUSH;
        while !self.flush()? {
            if Instant::now() >= deadline { return Err(transport_error()); }
            match unsafe { WaitForSingleObject(parent.as_raw_handle(), HOOK_POLL_MS) } {
                WAIT_OBJECT_0 => return Ok(()),
                WAIT_TIMEOUT => {}
                _ => return Err(last_error(HelperFailureStage::Transport)),
            }
        }
        Ok(())
    }
}

fn transport_error() -> NativeError {
    NativeError::without_code(HelperFailureStage::Transport)
}
