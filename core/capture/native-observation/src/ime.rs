//! Native IME queries use fixed, bounded window messages.

use std::time::Instant;

use windows_sys::Win32::UI::{
    Input::{Ime::{ImmGetDefaultIMEWnd, ImmIsIME}, KeyboardAndMouse::HKL},
    WindowsAndMessaging::{GetWindowThreadProcessId, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW},
};

pub use screen_fix_protocol::ipc::elevated::ImeState as State;

use crate::{identity::WindowProcess, windows::FocusSnapshot};

const IME_CONTROL: u32 = 0x0283;
const GET_OPEN_STATUS: usize = 0x0005;
const GET_CONVERSION_MODE: usize = 0x0001;
const NATIVE_AND_FULLSHAPE: usize = 0x0001 | 0x0008;

/// Whether the focus thread's input language has an IME. Without a focus
/// window (no snapshot, or only the active-window fallback) the answer is
/// unknown, not "no IME".
pub fn has_ime(focus: Option<FocusSnapshot>) -> Option<bool> {
    let focus = focus.filter(|focus| !focus.active_fallback)?;
    Some(unsafe { ImmIsIME(focus.focus.keyboard_layout as usize as HKL) != 0 })
}

pub fn read(foreground: WindowProcess, focus: Option<FocusSnapshot>, expires: Instant) -> State {
    read_inner(foreground, focus, expires, || true, false)
}

pub fn read_scoped(
    foreground: WindowProcess, focus: Option<FocusSnapshot>, expires: Instant, guard: impl Fn() -> bool,
) -> State {
    read_inner(foreground, focus, expires, guard, true)
}

fn read_inner(
    foreground: WindowProcess, focus: Option<FocusSnapshot>, expires: Instant, guard: impl Fn() -> bool, scoped: bool,
) -> State {
    fn active(foreground: WindowProcess, focus: Option<FocusSnapshot>) -> bool {
        WindowProcess::foreground() == Some(foreground)
            && FocusSnapshot::read(foreground).map(|current| (current.focus, current.hung))
                == focus.map(|expected| (expected.focus, false))
    }

    let Some(focus) = focus.filter(|focus| !focus.hung && !focus.active_fallback) else { return State::Unknown };
    if Instant::now() >= expires || !guard() || !active(foreground, Some(focus)) {
        return State::Unknown;
    }
    let hwnd = focus.focus.window.0 as usize as *mut _;
    let window = unsafe { ImmGetDefaultIMEWnd(hwnd) };
    if window.is_null() {
        return State::Unknown;
    }
    let valid = || {
        if !guard() || !active(foreground, Some(focus)) { return false; }
        if !scoped { return true; }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) == focus.focus.thread.get() 
            && pid == foreground.process.pid.get() && ImmGetDefaultIMEWnd(hwnd) == window }
    };
    if !valid() { return State::Unknown; }
    let open = send(window, GET_OPEN_STATUS, expires);
    let conversion = (open == Some(1) && valid()).then(|| send(window, GET_CONVERSION_MODE, expires)).flatten();
    if Instant::now() > expires || !valid() || !active(foreground, Some(focus)) {
        return State::Unknown;
    }
    match (open, conversion) {
        (Some(0), _) => State::Closed,
        (Some(1), Some(mode)) if mode & NATIVE_AND_FULLSHAPE == 0 => State::PassThrough,
        (Some(1), Some(_)) => State::Composing,
        _ => State::Unknown,
    }
}

fn send(window: windows_sys::Win32::Foundation::HWND, command: usize, expires: Instant) -> Option<usize> {
    let remaining = expires.checked_duration_since(Instant::now())?;
    let timeout = u32::try_from(remaining.as_millis()).ok().filter(|value| *value != 0)?;
    let mut result = 0;
    let delivered = unsafe {
        SendMessageTimeoutW(window, IME_CONTROL, command, 0, SMTO_ABORTIFHUNG | SMTO_BLOCK, timeout, &mut result)
    };
    (delivered != 0).then_some(result)
}
