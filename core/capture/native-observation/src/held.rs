use std::{mem::{size_of, size_of_val}, os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle}, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::{HANDLE, HWND},
    Security::{GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel},
    System::{
        StationsAndDesktops::{CloseDesktop, DESKTOP_READOBJECTS, GetThreadDesktop, GetUserObjectInformationW,
            HDESK, OpenInputDesktop, UOI_NAME},
        Threading::{GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    },
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState, VK_CAPITAL, VK_LSHIFT, VK_RSHIFT, VK_LCONTROL,
            VK_RCONTROL, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN, VK_LBUTTON, VK_RBUTTON,
            VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2},
        WindowsAndMessaging::{GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SM_SWAPBUTTON},
    },
};

pub use screen_fix_protocol::ipc::elevated::HeldSnapshot;

struct Desktop(HDESK);

impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe { CloseDesktop(self.0) };
    }
}

pub fn read() -> Option<HeldSnapshot> {
    let foreground = unsafe { GetForegroundWindow() };
    let pid = process_id(foreground)?;
    if !on_input_desktop() || !foreground_integrity_allowed(pid) {
        return None;
    }
    let modifiers = [VK_LSHIFT, VK_RSHIFT, VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU,
        VK_LWIN, VK_RWIN].map(pressed);
    let physical_buttons = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2].map(pressed);
    let caps_lock = (unsafe { GetKeyState(i32::from(VK_CAPITAL)) } & 1) != 0;
    let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
    if unsafe { GetForegroundWindow() } != foreground || process_id(foreground) != Some(pid) || !on_input_desktop() {
        return None;
    }
    Some(HeldSnapshot { modifiers, physical_buttons, swapped, caps_lock })
}

fn pressed(vk: u16) -> bool {
    // Only the physical high bit is used, and only at a resync boundary.
    (unsafe { GetAsyncKeyState(i32::from(vk)) }) < 0
}

fn process_id(window: HWND) -> Option<u32> {
    if window.is_null() {
        return None;
    }
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    (thread != 0 && pid != 0).then_some(pid)
}

pub fn on_input_desktop() -> bool {
    // Both handles refer to desktops of this process's window station.
    let input = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input.is_null() {
        return false;
    }
    let input = Desktop(input);
    let owner = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    if owner.is_null() {
        return false;
    }
    let mut active_name = [0_u16; 128];
    let mut owner_name = [0_u16; 128];
    match (desktop_name(input.0, &mut active_name), desktop_name(owner, &mut owner_name)) {
        (Some(active), Some(owner)) => active == owner,
        _ => false,
    }
}

fn desktop_name(desktop: HDESK, buffer: &mut [u16; 128]) -> Option<&[u16]> {
    if unsafe {
        GetUserObjectInformationW(desktop as HANDLE, UOI_NAME, buffer.as_mut_ptr().cast(),
            size_of_val(buffer) as u32, null_mut())
    } == 0 {
        return None;
    }
    let end = buffer.iter().position(|unit| *unit == 0)?;
    (end != 0).then_some(&buffer[..end])
}

fn foreground_integrity_allowed(pid: u32) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    // OpenProcess returns a newly owned, non-inheritable handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    match (integrity(process.as_raw_handle()), integrity(unsafe { GetCurrentProcess() })) {
        (Some(target), Some(recorder)) => target <= recorder,
        _ => false,
    }
}

fn integrity(process: HANDLE) -> Option<u32> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    // This token handle is independent of the source process handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut data = [0_usize; 64];
    let mut used = 0;
    if unsafe { GetTokenInformation(token.as_raw_handle(), TokenIntegrityLevel, data.as_mut_ptr().cast(),
        size_of_val(&data) as u32, &mut used) } == 0
        || !(size_of::<TOKEN_MANDATORY_LABEL>()..=size_of_val(&data)).contains(&(used as usize))
    {
        return None;
    }
    // The aligned buffer now holds a native label and its SID. Validate that
    // the SID head and the requested subauthority both stay in returned data.
    let label = unsafe { &*data.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let sid = label.Label.Sid;
    let start = data.as_ptr() as usize;
    let end = start.checked_add(used as usize)?;
    if (sid as usize) < start || (sid as usize).checked_add(8)? > end || (sid as usize) % 4 != 0 {
        return None;
    }
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    if count == 0 || (sid as usize).checked_add(8 + usize::from(count) * 4)? > end {
        return None;
    }
    Some(unsafe { *GetSidSubAuthority(sid, u32::from(count - 1)) })
}
