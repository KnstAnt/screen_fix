use std::{mem::size_of, num::NonZeroU32};

use windows::{
    Win32::{
        Foundation::{HWND, POINT, RECT},
        Graphics::{Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute}, Gdi::ClientToScreen},
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT, GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetWindowDpiAwarenessContext,
                LogicalToPhysicalPointForPerMonitorDPI, SetThreadDpiAwarenessContext},
            WindowsAndMessaging::{GetClassNameW, GetWindowTextW},
        },
    },
};
use windows_sys::Win32::UI::{
    Input::KeyboardAndMouse::{GetKeyboardLayout, IsWindowEnabled},
    WindowsAndMessaging::{
        GA_ROOT, GUITHREADINFO, GW_OWNER, GetAncestor, GetGUIThreadInfo, GetWindow, GetWindowThreadProcessId,
        IsHungAppWindow, IsWindowVisible,
    },
};

use screen_fix_protocol::{
    PayloadLimit, PhysicalRect, WindowId,
    ipc::{FocusAtReceipt, elevated::{FocusEvidence, MAX_METADATA_BYTES, Metadata, WindowEvidence}},
};

use super::identity::{MetadataScrubber, WindowProcess};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FocusSnapshot {
    pub focus: FocusAtReceipt,
    pub caret: Option<PhysicalRect>,
    pub caret_window: Option<WindowId>,
    pub gui_flags: u32,
    pub hung: bool,
}

impl FocusSnapshot {
    pub fn matches_identity(self, expected: FocusAtReceipt) -> bool {
        self.focus.window == expected.window && self.focus.thread == expected.thread
            && self.focus.keyboard_layout == expected.keyboard_layout
    }

    pub fn read(foreground: WindowProcess) -> Option<Self> {
        let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        if unsafe { GetGUIThreadInfo(foreground.thread.get(), &mut gui) } == 0 || gui.hwndFocus.is_null() {
            return None;
        }
        let mut pid = 0;
        let thread = NonZeroU32::new(unsafe { GetWindowThreadProcessId(gui.hwndFocus, &mut pid) })?;
        if pid != foreground.process.pid.get() {
            return None;
        }
        let layout = unsafe { GetKeyboardLayout(thread.get()) };
        if layout.is_null() || WindowProcess::foreground() != Some(foreground) {
            return None;
        }
        let hwnd = HWND(gui.hwndFocus);
        let caret = (!gui.hwndCaret.is_null()).then(|| {
            let window = HWND(gui.hwndCaret);
            let start = physical_client_point(window, gui.rcCaret.left, gui.rcCaret.top)?;
            let end = physical_client_point(window, gui.rcCaret.right, gui.rcCaret.bottom)?;
            PhysicalRect::new(start.x, start.y, end.x, end.y)
        }).flatten();
        Some(Self {
            focus: FocusAtReceipt {
                window: WindowId(gui.hwndFocus as usize as u64), thread, keyboard_layout: layout as usize as u64,
                password_style: password_style(hwnd),
            },
            caret, caret_window: (!gui.hwndCaret.is_null()).then_some(WindowId(gui.hwndCaret as usize as u64)),
            gui_flags: gui.flags, hung: unsafe { IsHungAppWindow(gui.hwndFocus) != 0 },
        })
    }
}

fn physical_client_point(window: HWND, x: i32, y: i32) -> Option<POINT> {
    struct PreviousDpi(DPI_AWARENESS_CONTEXT);
    impl Drop for PreviousDpi {
        fn drop(&mut self) {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
    let awareness = unsafe { GetWindowDpiAwarenessContext(window) };
    let previous = unsafe { SetThreadDpiAwarenessContext(awareness) };
    if previous.0.is_null() {
        return None;
    }
    let restore = PreviousDpi(previous);
    let mut point = POINT { x, y };
    if !unsafe { ClientToScreen(window, &mut point) }.as_bool() {
        return None;
    }
    drop(restore);
    unsafe { LogicalToPhysicalPointForPerMonitorDPI(Some(window), &mut point) }.as_bool().then_some(point)
}

pub fn password_style(hwnd: HWND) -> Option<bool> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, SetLastError},
        UI::WindowsAndMessaging::{ES_PASSWORD, GWL_STYLE, GetWindowLongW},
    };
    unsafe {
        SetLastError(0);
        let style = GetWindowLongW(hwnd.0, GWL_STYLE);
        (style != 0 || GetLastError() == 0).then_some((style as u32) & (ES_PASSWORD as u32) != 0)
    }
}

#[derive(serde::Serialize)]
pub struct WindowMetadata {
    pub bounds: Option<PhysicalRect>,
    pub title: Option<String>,
    pub class: Option<String>,
    pub dpi: Option<NonZeroU32>,
    pub dpi_awareness: Option<i32>,
    pub enabled: bool,
    pub owned_visible: bool,
}

pub fn window_dpi(proof: WindowProcess) -> (Option<NonZeroU32>, Option<i32>) {
    let hwnd = proof.hwnd();
    let dpi = NonZeroU32::new(unsafe { GetDpiForWindow(hwnd) });
    let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(hwnd)) }.0;
    (dpi, (0..=2).contains(&awareness).then_some(awareness))
}

impl WindowMetadata {
    pub fn read(proof: WindowProcess, scrubber: &MetadataScrubber) -> Option<Self> {
        let hwnd = proof.hwnd();
        let mut title = [0u16; 1024];
        let mut class = [0u16; 256];
        let title_length = unsafe { GetWindowTextW(hwnd, &mut title) };
        let class_length = unsafe { GetClassNameW(hwnd, &mut class) };
        let mut bounds = RECT::default();
        let have_bounds = unsafe {
            DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, (&mut bounds as *mut RECT).cast(), size_of::<RECT>() as u32)
        }.is_ok();
        let (dpi, dpi_awareness) = window_dpi(proof);
        let enabled = unsafe { IsWindowEnabled(hwnd.0) } != 0;
        let owned_visible = unsafe { IsWindowVisible(hwnd.0) != 0 && GetAncestor(hwnd.0, GA_ROOT) == hwnd.0 }
            && WindowProcess::read(HWND(unsafe { GetWindow(hwnd.0, GW_OWNER) }))
                .is_some_and(|owner| owner.process == proof.process);
        if WindowProcess::read(hwnd) != Some(proof) {
            return None;
        }
        let bounded = |data: &[u16], len: i32| data.get(..len as usize)
            .filter(|units| !units.is_empty()).map(|units| scrubber.scrub(&String::from_utf16_lossy(units), MAX_METADATA_BYTES));
        Some(Self {
            bounds: have_bounds.then(|| PhysicalRect::new(bounds.left, bounds.top, bounds.right, bounds.bottom)).flatten(),
            title: bounded(&title, title_length), class: bounded(&class, class_length), dpi, dpi_awareness, enabled, owned_visible,
        })
    }
}

impl From<FocusSnapshot> for FocusEvidence {
    fn from(snapshot: FocusSnapshot) -> Self {
        Self {
            focus: snapshot.focus, caret: snapshot.caret, caret_window: snapshot.caret_window,
            gui_flags: snapshot.gui_flags, hung: snapshot.hung,
        }
    }
}

impl TryFrom<WindowMetadata> for WindowEvidence {
    type Error = PayloadLimit;

    fn try_from(window: WindowMetadata) -> Result<Self, Self::Error> {
        Ok(Self {
            bounds: window.bounds,
            title: window.title.as_deref().map(Metadata::new).transpose()?,
            class: window.class.as_deref().map(Metadata::new).transpose()?,
            dpi: window.dpi, dpi_awareness: window.dpi_awareness,
            enabled: window.enabled, owned_visible: window.owned_visible,
        })
    }
}
