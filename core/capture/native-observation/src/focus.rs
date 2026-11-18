//! The focus window that keyboard layout and focus evidence are read from.

use std::{mem::size_of, num::NonZeroU32};

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, SetLastError},
    UI::{
        Input::KeyboardAndMouse::{GetKeyboardLayout, HKL},
        WindowsAndMessaging::{
            ES_PASSWORD, GUITHREADINFO, GWL_STYLE, GetClassNameW, GetGUIThreadInfo, GetWindowLongW,
            GetWindowThreadProcessId,
        },
    },
};

/// ES_PASSWORD on an Edit or RichEdit window. The style bit means nothing on
/// other classes, which give no native password evidence (`Some(false)`) and
/// rely on UIA IsPassword (C4). `None` when the window cannot be read.
pub fn password_style(hwnd: HWND) -> Option<bool> {
    let mut class = [0_u16; 64];
    let length = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    let class = String::from_utf16_lossy(class.get(..usize::try_from(length).ok().filter(|length| *length > 0)?)?)
        .to_ascii_lowercase();
    if !(class == "edit" || class.starts_with("richedit") || class.contains(".edit.")) {
        return Some(false);
    }
    unsafe {
        SetLastError(0);
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        (style != 0 || GetLastError() == 0).then_some((style as u32) & (ES_PASSWORD as u32) != 0)
    }
}

#[derive(Clone, Copy)]
pub struct FocusWindow {
    pub hwnd: HWND,
    /// No window has keyboard focus; `hwnd` is the active window (INP-2).
    /// It identifies the focus state and supplies the keyboard layout only.
    /// It is never password, text-target or IME evidence.
    pub active_fallback: bool,
}

/// INP-2: the keyboard focus window, or the active window when no window has focus.
pub fn focus_window(gui: &GUITHREADINFO) -> Option<FocusWindow> {
    if !gui.hwndFocus.is_null() {
        Some(FocusWindow { hwnd: gui.hwndFocus, active_fallback: false })
    } else if !gui.hwndActive.is_null() {
        Some(FocusWindow { hwnd: gui.hwndActive, active_fallback: true })
    } else {
        None
    }
}

/// The focus window of GUI thread `thread` when process `pid` owns it.
pub struct FocusAt {
    pub gui: GUITHREADINFO,
    pub window: FocusWindow,
    /// The thread that owns the focus window.
    pub thread: NonZeroU32,
    /// That thread's keyboard layout; never null.
    pub keyboard_layout: HKL,
}

pub fn focus_at(thread: NonZeroU32, pid: NonZeroU32) -> Option<FocusAt> {
    let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    if unsafe { GetGUIThreadInfo(thread.get(), &mut gui) } == 0 {
        return None;
    }
    let window = focus_window(&gui)?;
    let mut owner = 0;
    let thread = NonZeroU32::new(unsafe { GetWindowThreadProcessId(window.hwnd, &mut owner) })?;
    if owner != pid.get() {
        return None;
    }
    let keyboard_layout = unsafe { GetKeyboardLayout(thread.get()) };
    (!keyboard_layout.is_null()).then_some(FocusAt { gui, window, thread, keyboard_layout })
}
