//! Native window, registration and scratch buffers stay on the Raw Input
//! thread. Evidence is copied out before the next buffer operation.

use std::{mem::{offset_of, size_of}, num::NonZeroU32, ptr::{null, null_mut}};

use screen_fix_protocol::{
    MessageTickMs, PhysicalPoint, PhysicalRect, QpcTick, SourceSequence, WindowId,
    ipc::{CollectedInput, DeviceDescriptor, FocusAtReceipt, ForegroundAtReceipt, HelperFailureStage, InputSample, ProcessInstance, RawInputRegistrationMismatch, RawInputRegistrationObservation, RawKey, RawPointer, Receipt},
};
use windows_sys::{
    Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, GetLastError, HANDLE, HWND, POINT, SetLastError},
        System::{Performance::QueryPerformanceCounter, SystemInformation::GetTickCount64, Threading::{
            GetCurrentThread, GetProcessTimes, IsWow64Process, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        }},
        UI::{
            Input::{
                GetRawInputBuffer, GetRawInputData, GetRegisteredRawInputDevices, HRAWINPUT, RAWINPUT,
                RAWINPUTDEVICE, RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE, RID_INPUT, RIDEV_DEVNOTIFY,
                RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEHID, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
            },
            Input::KeyboardAndMouse::GetKeyboardLayout,
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, ES_PASSWORD, GWL_STYLE, GetForegroundWindow,
                GetWindowLongW,
                GetGUIThreadInfo, GetPhysicalCursorPos, GetSystemMetrics, GetWindowThreadProcessId,
                GUITHREADINFO, HWND_MESSAGE, MSG, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
                SM_CYVIRTUALSCREEN, SM_SWAPBUTTON, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
            },
        },
    },
    w,
};

use super::{NativeError, device, hid::HidCollections, last_error};

const INITIAL_RAW_BYTES: usize = 16 * 1024;
const MAX_RAW_BYTES: usize = 256 * 1024;
const HEADER_BYTES: usize = size_of::<RAWINPUTHEADER>();

pub(super) struct RawWindow {
    handle: HWND,
    pen_hid: bool,
    hid: HidCollections,
    single: Vec<u64>,
    buffered: Vec<u64>,
    wow64: bool,
    sequence: u64,
    last_registration_check: u64,
}

impl RawWindow {
    pub(super) fn open(pen_hid: bool) -> Result<Self, NativeError> {
        if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) } == 0 {
            return Err(last_error(HelperFailureStage::Startup));
        }
        let mut wow64 = 0;
        if unsafe { IsWow64Process(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut wow64) } == 0 {
            return Err(last_error(HelperFailureStage::Startup));
        }
        let handle = unsafe {
            CreateWindowExW(0, w!("STATIC"), null(), 0, 0, 0, 0, 0,
                HWND_MESSAGE, null_mut(), null_mut(), null())
        };
        if handle.is_null() {
            return Err(last_error(HelperFailureStage::RawInputRegistration));
        }
        let window = Self {
            handle, pen_hid, hid: HidCollections::default(), single: vec![0; INITIAL_RAW_BYTES / 8], buffered: vec![0; INITIAL_RAW_BYTES / 8],
            wow64: wow64 != 0 && cfg!(target_pointer_width = "32"), sequence: 0,
            last_registration_check: unsafe { GetTickCount64() },
        };
        let devices = registrations(handle, RIDEV_INPUTSINK | RIDEV_DEVNOTIFY, pen_hid);
        if unsafe { RegisterRawInputDevices(devices.as_ptr(), devices.len() as u32, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
            return Err(last_error(HelperFailureStage::RawInputRegistration));
        }
        window.verify_registration()?;
        Ok(window)
    }

    pub(super) fn handle(&self) -> HWND {
        self.handle
    }

    pub(super) fn reset_devices(&mut self) { self.hid = HidCollections::default(); }

    pub(super) fn observe_device(&mut self, handle: HANDLE, descriptor: &mut DeviceDescriptor) {
        self.hid.observe(handle, descriptor, self.pen_hid);
    }

    pub(super) fn remove_device(&mut self, handle: HANDLE) {
        self.hid.remove(handle);
    }

    pub(super) fn check_registration(&mut self) -> Result<(), NativeError> {
        let now = unsafe { GetTickCount64() };
        if now.wrapping_sub(self.last_registration_check) < 5_000 { return Ok(()); }
        self.last_registration_check = now;
        self.verify_registration()
    }

    fn verify_registration(&self) -> Result<(), NativeError> {
        let mut devices = [RAWINPUTDEVICE::default(); 16];
        let mut count = devices.len() as u32;
        let copied = unsafe {
            GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count, size_of::<RAWINPUTDEVICE>() as u32)
        };
        if copied == u32::MAX {
            return Err(last_error(HelperFailureStage::RawInputRegistration));
        }
        let registered = devices.get(..copied as usize).ok_or_else(invalid_raw)?;
        let flags = RIDEV_INPUTSINK | RIDEV_DEVNOTIFY;
        for expected in registrations(self.handle, flags, self.pen_hid) {
            let mut entries = registered.iter().filter(|entry| {
                entry.usUsagePage == expected.usUsagePage && entry.usUsage == expected.usUsage
            });
            // Windows can omit DEVNOTIFY from registration readback. Registration
            // still requests notifications, and every other readback flag must match.
            let first = entries.next();
            let observed = match first {
                None => RawInputRegistrationObservation::Missing,
                Some(first) => {
                    let mut same_window = std::iter::once(first).chain(entries)
                        .filter(|entry| entry.hwndTarget == expected.hwndTarget);
                    if let Some(entry) = same_window.next() {
                        if entry.dwFlags & !RIDEV_DEVNOTIFY == RIDEV_INPUTSINK
                            || same_window.any(|entry| entry.dwFlags & !RIDEV_DEVNOTIFY == RIDEV_INPUTSINK)
                        {
                            continue;
                        }
                        RawInputRegistrationObservation::WrongFlags { flags: entry.dwFlags }
                    } else {
                        RawInputRegistrationObservation::WrongWindow { flags: first.dwFlags }
                    }
                }
            };
            return Err(NativeError {
                raw_input_registration: Some(RawInputRegistrationMismatch {
                    usage_page: expected.usUsagePage,
                    usage: expected.usUsage,
                    expected_flags: expected.dwFlags,
                    observed,
                }),
                ..NativeError::without_code(HelperFailureStage::RawInputRegistration)
            });
        }
        Ok(())
    }

    pub(super) fn read(
        &mut self, message: &MSG, receipt_qpc: QpcTick, buffered: bool, mut publish: impl FnMut(CollectedInput),
        stopping: impl Fn() -> bool,
    ) -> Result<(), NativeError> {
        let result = self.read_current(message, receipt_qpc, &mut publish)
            .and_then(|()| if buffered { self.drain(&mut publish, stopping) } else { Ok(()) });
        // Always give the retrieved message its default cleanup, even when
        // the packet fails validation or the session ends during a burst.
        unsafe { DefWindowProcW(self.handle, message.message, message.wParam, message.lParam) };
        result
    }

    fn read_current(
        &mut self, message: &MSG, qpc: QpcTick, publish: &mut impl FnMut(CollectedInput),
    ) -> Result<(), NativeError> {
        let foreground = receipt_foreground();
        let native = message.lParam as HRAWINPUT;
        let mut needed = 0;
        if unsafe { GetRawInputData(native, RID_INPUT, null_mut(), &mut needed, HEADER_BYTES as u32) } == u32::MAX {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        if needed as usize > self.single.len() * 8 {
            grow(&mut self.single, needed as usize)?;
        }
        let mut capacity = (self.single.len() * 8) as u32;
        let copied = unsafe {
            GetRawInputData(native, RID_INPUT, self.single.as_mut_ptr().cast(), &mut capacity, HEADER_BYTES as u32)
        };
        if copied == u32::MAX {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        let bytes = as_bytes(&self.single).get(..copied as usize).ok_or_else(invalid_raw)?;
        let (sample, _) = packet(bytes, false)?;
        emit_packet(sample, &mut self.hid, self.pen_hid, |sample| {
            publish(stamp(&mut self.sequence, qpc, Some(MessageTickMs(message.time)), foreground, sample)?);
            Ok(())
        })
    }

    fn drain(
        &mut self, publish: &mut impl FnMut(CollectedInput), stopping: impl Fn() -> bool,
    ) -> Result<(), NativeError> {
        while !stopping() {
            self.check_registration()?;
            let mut capacity = (self.buffered.len() * 8) as u32;
            let count = unsafe {
                GetRawInputBuffer(self.buffered.as_mut_ptr().cast(), &mut capacity, HEADER_BYTES as u32)
            };
            if count == u32::MAX {
                let error = last_error(HelperFailureStage::MessageLoop);
                if error.code != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
                    return Err(error);
                }
                let required = if self.wow64 { (capacity as usize).saturating_mul(8) } else { capacity as usize };
                let new_bytes = required.max((self.buffered.len() * 16).min(MAX_RAW_BYTES));
                if new_bytes <= self.buffered.len() * 8 { return Err(error); }
                grow(&mut self.buffered, new_bytes)?;
                continue;
            }
            if count == 0 { return Ok(()); }
            // The retrieval instant is shared by the batch; each packet still
            // gets its native order and a distinct sequence. Buffered records
            // do not have a corresponding MSG.time.
            let qpc = qpc()?;
            let foreground = receipt_foreground();
            let mut bytes = as_bytes(&self.buffered);
            for index in 0..count {
                let (sample, size) = packet(bytes, self.wow64)?;
                emit_packet(sample, &mut self.hid, self.pen_hid, |sample| {
                    publish(stamp(&mut self.sequence, qpc, None, foreground, sample)?);
                    Ok(())
                })?;
                if index + 1 < count {
                    bytes = bytes.get(size.next_multiple_of(8)..).ok_or_else(invalid_raw)?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for RawWindow {
    fn drop(&mut self) {
        let entries = registrations(null_mut(), RIDEV_REMOVE, self.pen_hid);
        unsafe {
            RegisterRawInputDevices(entries.as_ptr(), entries.len() as u32, size_of::<RAWINPUTDEVICE>() as u32);
            DestroyWindow(self.handle);
        }
    }
}

fn registrations(window: HWND, flags: u32, pen_hid: bool) -> Vec<RAWINPUTDEVICE> {
    let mut collections = vec![(1, 2), (1, 6), (0x0D, 5), (1, 8)];
    if pen_hid {
        collections.extend([(0x0D, 1), (0x0D, 2)]);
    }
    collections.into_iter().map(|(page, usage)| RAWINPUTDEVICE {
        usUsagePage: page, usUsage: usage, dwFlags: flags, hwndTarget: window,
    }).collect()
}

pub(super) fn qpc() -> Result<QpcTick, NativeError> {
    let mut qpc = 0;
    if unsafe { QueryPerformanceCounter(&mut qpc) } == 0 {
        return Err(last_error(HelperFailureStage::MessageLoop));
    }
    Ok(QpcTick(qpc))
}

fn stamp(
    sequence: &mut u64, qpc: QpcTick, message_tick_ms: Option<MessageTickMs>,
    foreground_at_receipt: Option<ForegroundAtReceipt>, sample: InputSample,
) -> Result<CollectedInput, NativeError> {
    *sequence = sequence.checked_add(1).ok_or_else(invalid_raw)?;
    let mut point = POINT::default();
    let position_at_receipt = (matches!(&sample, InputSample::RawPointer(_))
        || matches!(&sample, InputSample::Hid(report) if report.is_pen()))
        .then(|| unsafe { GetPhysicalCursorPos(&mut point) } != 0)
        .filter(|valid| *valid).map(|_| PhysicalPoint { x: point.x, y: point.y });
    let (swapped_buttons, absolute_desktop) = match &sample {
        InputSample::RawPointer(raw) => (
            Some(unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0),
            absolute_desktop(raw.move_flags),
        ),
        _ => (None, None),
    };
    Ok(CollectedInput {
        receipt: Receipt { sequence: SourceSequence(*sequence), qpc, message_tick_ms },
        foreground_at_receipt,
        position_at_receipt,
        swapped_buttons,
        absolute_desktop,
        sample,
    })
}

fn receipt_foreground() -> Option<ForegroundAtReceipt> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return None;
    }
    let mut pid = 0;
    let thread = NonZeroU32::new(unsafe { GetWindowThreadProcessId(foreground, &mut pid) })?;
    let pid = NonZeroU32::new(pid)?;
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid.get()) };
    if process.is_null() {
        return None;
    }
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // Keep the process object open across the window checks; the PID cannot
    // be recycled while this handle still identifies the original process.
    let creation_available = unsafe {
        GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) != 0
    };
    let focus = sample_focus(thread, pid);
    let mut checked_pid = 0;
    let stable = unsafe {
        GetForegroundWindow() == foreground
            && GetWindowThreadProcessId(foreground, &mut checked_pid) == thread.get()
            && checked_pid == pid.get()
    };
    unsafe { CloseHandle(process) };
    if !creation_available || !stable {
        return None;
    }
    let creation_filetime = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some(ForegroundAtReceipt {
        window: WindowId(foreground as usize as u64),
        process: ProcessInstance { pid, creation_filetime },
        focus,
    })
}

fn sample_focus(thread: NonZeroU32, pid: NonZeroU32) -> Option<FocusAtReceipt> {
    let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    if unsafe { GetGUIThreadInfo(thread.get(), &mut gui) } == 0 || gui.hwndFocus.is_null() {
        return None;
    }
    let focus = gui.hwndFocus;
    let mut owner = 0;
    let focus_thread = NonZeroU32::new(unsafe { GetWindowThreadProcessId(focus, &mut owner) })?;
    if owner != pid.get() {
        return None;
    }
    let layout = unsafe { GetKeyboardLayout(focus_thread.get()) };
    let password_style = unsafe {
        SetLastError(0);
        let style = GetWindowLongW(focus, GWL_STYLE);
        (style != 0 || GetLastError() == 0).then_some((style as u32) & (ES_PASSWORD as u32) != 0)
    };
    if layout.is_null() || unsafe { GetGUIThreadInfo(thread.get(), &mut gui) } == 0
        || gui.hwndFocus != focus || unsafe { GetKeyboardLayout(focus_thread.get()) } != layout
    {
        return None;
    }
    Some(FocusAtReceipt {
        window: WindowId(focus as usize as u64), thread: focus_thread,
        keyboard_layout: layout as usize as u64, password_style,
    })
}

fn absolute_desktop(flags: u16) -> Option<PhysicalRect> {
    if flags & 1 == 0 {
        return None;
    }
    let (left, top, width, height) = if flags & 2 != 0 {
        unsafe { (
            GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN),
        ) }
    } else {
        unsafe { (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    };
    PhysicalRect::new(left, top, left.checked_add(width)?, top.checked_add(height)?)
}

fn grow(buffer: &mut Vec<u64>, required: usize) -> Result<(), NativeError> {
    if required > MAX_RAW_BYTES || required < HEADER_BYTES {
        return Err(invalid_raw());
    }
    buffer.resize(required.div_ceil(8), 0);
    Ok(())
}

fn as_bytes(buffer: &[u64]) -> &[u8] {
    // The u64 allocation provides eight-byte alignment to Win32. Its
    // initialized storage can also be inspected as bytes.
    unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), std::mem::size_of_val(buffer)) }
}

enum Packet<'a> {
    Input(InputSample),
    Hid(HANDLE, &'a [u8]),
    Other,
}

fn emit_packet(
    packet: Packet<'_>,
    hid: &mut HidCollections,
    pen_enabled: bool,
    mut emit: impl FnMut(InputSample) -> Result<(), NativeError>,
) -> Result<(), NativeError> {
    match packet {
        Packet::Input(sample) => emit(sample),
        Packet::Hid(handle, bytes) => {
            let mut result = Ok(());
            hid.reports(handle, bytes, pen_enabled, |report| {
                if result.is_ok() {
                    result = emit(InputSample::Hid(report));
                }
            });
            result
        }
        Packet::Other => Ok(()),
    }
}

fn packet(bytes: &[u8], wow64_buffered: bool) -> Result<(Packet<'_>, usize), NativeError> {
    let header = native_copy::<RAWINPUTHEADER>(bytes)?;
    let size = header.dwSize as usize;
    let data_offset = if wow64_buffered { HEADER_BYTES + 8 } else { offset_of!(RAWINPUT, data) };
    let payload = bytes.get(data_offset..size).ok_or_else(invalid_raw)?;
    let device = device::id(header.hDevice);
    let sample = match header.dwType {
        RIM_TYPEKEYBOARD => {
            let key = native_copy::<RAWKEYBOARD>(payload)?;
            Packet::Input(InputSample::RawKey(RawKey {
                device, make_code: key.MakeCode, virtual_key: key.VKey, flags: key.Flags,
                message: key.Message, extra_information: key.ExtraInformation,
            }))
        }
        RIM_TYPEMOUSE => {
            let raw = native_copy::<RAWMOUSE>(payload)?;
            // The button flag fields share storage with the packed u32.
            let buttons = unsafe { raw.Anonymous.Anonymous };
            Packet::Input(InputSample::RawPointer(RawPointer {
                device, move_flags: raw.usFlags, button_flags: buttons.usButtonFlags,
                button_data: buttons.usButtonData, raw_buttons: raw.ulRawButtons,
                x: raw.lLastX, y: raw.lLastY, extra_information: raw.ulExtraInformation,
            }))
        }
        RIM_TYPEHID if !header.hDevice.is_null() => Packet::Hid(header.hDevice, payload),
        RIM_TYPEHID => return Err(invalid_raw()),
        _ => Packet::Other,
    };
    Ok((sample, size))
}

// The only callers use Win32 integer/pointer PODs copied from initialized
// scratch memory. The length check covers the whole struct including padding.
fn native_copy<T: Copy>(bytes: &[u8]) -> Result<T, NativeError> {
    if bytes.len() < size_of::<T>() { return Err(invalid_raw()); }
    Ok(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}

fn invalid_raw() -> NativeError {
    NativeError::without_code(HelperFailureStage::MessageLoop)
}
