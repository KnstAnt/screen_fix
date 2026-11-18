//! A click-through status label driven by the UI thread's native timer, even
//! when the main viewport is minimized and egui is not painting it.

use std::{cell::RefCell, mem::size_of, panic::{AssertUnwindSafe, catch_unwind}, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, RECT, SetLastError},
    Graphics::Gdi::{DEFAULT_GUI_FONT, GetMonitorInfoW, GetStockObject, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetForegroundWindow, GetWindowLongPtrW, GWLP_USERDATA,
        HWND_TOPMOST, KillTimer, LWA_ALPHA, SendMessageW, SetLayeredWindowAttributes,
        SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, SWP_HIDEWINDOW,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
        WM_SETFONT, WS_BORDER, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    },
};

use crate::{CaptureError, session::{SessionHandle, SessionOutcome, SessionPhase, SourceStatus, windows::Control}};

const TIMER: usize = 1;

pub(super) struct Indicator {
    window: HWND,
    // The native timer borrows this allocation only on its creating UI thread.
    state: Box<RefCell<State>>,
}

struct State {
    fallback: HWND,
    control: Option<Control>,
    error: Option<CaptureError>,
    failed: bool,
    label: Option<&'static str>,
    last_area: Option<RECT>,
}

impl Indicator {
    pub(super) fn new(fallback: HWND) -> Result<Self, CaptureError> {
        let state = Box::new(RefCell::new(State {
            fallback, control: None, error: None, failed: false, label: None, last_area: None,
        }));
        // No owner: minimizing the main window must not hide this indicator.
        // Layered + transparent passes mouse input to the application underneath.
        let window = unsafe { CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            windows::core::w!("STATIC").as_ptr(), windows::core::w!("").as_ptr(),
            WS_POPUP | WS_BORDER, 0, 0, 300, 30, null_mut(), null_mut(), null_mut(), null_mut(),
        ) };
        if window.is_null() { return Err(native_error("Create the recording indicator")); }
        let indicator = Self { window, state };
        unsafe { SetLastError(0); }
        let previous = unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, (&*indicator.state as *const RefCell<State>) as isize) };
        if previous == 0 && unsafe { GetLastError() } != 0 {
            return Err(native_error("Attach the recording indicator state"));
        }
        if unsafe { SetLayeredWindowAttributes(window, 0, 255, LWA_ALPHA) } == 0 {
            return Err(native_error("Configure the recording indicator"));
        }
        unsafe { SendMessageW(window, WM_SETFONT, GetStockObject(DEFAULT_GUI_FONT) as usize, 0); }
        if unsafe { SetTimer(window, TIMER, 100, Some(tick)) } == 0 {
            return Err(native_error("Start the recording indicator timer"));
        }
        Ok(indicator)
    }

    pub(super) fn observe(&mut self, session: &SessionHandle) -> Result<(), CaptureError> {
        let mut state = self.state.borrow_mut();
        if state.failed {
            return Err(CaptureError::owned(crate::package::diagnostics::FailureComponent::Interface, "Show the recording indicator", None,
                "The recording indicator failed. Restart Screen Fix before recording again.".into()));
        }
        state.control = Some(session.indicator_control());
        let result = state.refresh(self.window);
        if result.is_err() { state.stop_failed(self.window); }
        result
    }

    pub(super) fn clear(&mut self) {
        let mut state = self.state.borrow_mut();
        state.control = None;
        if let Err(error) = state.hide(self.window) { state.fail(self.window, error); }
    }

    pub(super) fn take_error(&mut self) -> Option<CaptureError> {
        self.state.borrow_mut().error.take()
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        // All operations and timer dispatch belong to the same UI thread.
        unsafe {
            KillTimer(self.window, TIMER);
            SetWindowLongPtrW(self.window, GWLP_USERDATA, 0);
            DestroyWindow(self.window);
        }
    }
}

impl State {
    fn fail(&mut self, window: HWND, error: CaptureError) {
        self.stop_failed(window);
        self.error = Some(error);
    }

    fn stop_failed(&mut self, window: HWND) {
        self.failed = true;
        // Do not leave a stale RECORDING label after its status source failed.
        // Hiding is best effort if Windows has already destroyed the HWND.
        let _ = self.hide(window);
        if let Some(control) = &self.control {
            control.stop_for_indicator_failure();
        }
    }

    fn hide(&mut self, window: HWND) -> Result<(), CaptureError> {
        if self.label.is_some() {
            if unsafe { SetWindowPos(window, null_mut(), 0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_HIDEWINDOW) } == 0
            {
                return Err(native_error("Hide the recording indicator"));
            }
            self.label = None;
        }
        Ok(())
    }

    fn refresh(&mut self, window: HWND) -> Result<(), CaptureError> {
        let Some(control) = &self.control else { return self.hide(window); };
        let Some(status) = control.status() else { return Ok(()); };
        let label = match status.phase {
            SessionPhase::Starting | SessionPhase::Paused if status.source == SourceStatus::AwaitingConsent =>
                "Screen Fix: waiting for Windows consent",
            SessionPhase::Starting | SessionPhase::Paused if status.source == SourceStatus::AwaitingReadiness =>
                "Screen Fix: verifying enhanced capture",
            SessionPhase::Starting => "Screen Fix: preparing recording",
            SessionPhase::Recording if status.source == SourceStatus::Activating =>
                "Screen Fix: waiting for certified input",
            SessionPhase::Recording if matches!(status.source, SourceStatus::FailedPrefix(_)) =>
                "Screen Fix: finishing a failed recording",
            SessionPhase::Recording if status.input_unavailable.is_some() || !status.video_unavailable.is_empty() =>
                "Screen Fix: recording incomplete",
            SessionPhase::Recording if status.source == SourceStatus::Enhanced => "Screen Fix: RECORDING (enhanced)",
            SessionPhase::Recording => "Screen Fix: RECORDING",
            SessionPhase::Paused => "Screen Fix: PAUSED",
            SessionPhase::Stopping => "Screen Fix: finishing recording",
            SessionPhase::Failed => "Screen Fix: FAILED - see recorder",
            SessionPhase::Finished if status.outcome == Some(SessionOutcome::Incomplete) =>
                "Screen Fix: INCOMPLETE - see recorder",
            SessionPhase::Finished => return self.hide(window),
        };
        if self.label != Some(label) {
            let text = super::super::wide(label);
            if unsafe { SetWindowTextW(window, text.as_ptr()) } == 0 {
                return Err(native_error("Update the recording indicator"));
            }
        }
        let foreground = unsafe { GetForegroundWindow() };
        let area = work_area(foreground).or_else(|| work_area(self.fallback)).or(self.last_area)
            .ok_or_else(|| native_error("Locate the recording indicator monitor"))?;
        self.last_area = Some(area);
        let width = 300.min((area.right - area.left).max(1));
        let height = 30.min((area.bottom - area.top).max(1));
        let placement = [(area.right - width - 12).max(area.left), (area.top + 12).min(area.bottom - height), width, height];
        // Reassert topmost without activation even after another topmost window
        // was raised. No capture, file IO or waiting occurs in this callback.
        if unsafe { SetWindowPos(window, HWND_TOPMOST, placement[0], placement[1], width, height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW) } == 0
        {
            return Err(native_error("Show the recording indicator"));
        }
        self.label = Some(label);
        Ok(())
    }
}

unsafe extern "system" fn tick(window: HWND, _: u32, timer: usize, _: u32) {
    if timer != TIMER { return; }
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const RefCell<State>;
    if pointer.is_null() { return; }
    // The allocation outlives the timer; reentrant dispatch never borrows it twice.
    let Ok(mut state) = (unsafe { &*pointer }).try_borrow_mut() else { return; };
    if state.failed { return; }
    match catch_unwind(AssertUnwindSafe(|| state.refresh(window))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => state.fail(window, error),
        Err(_) => state.fail(window, CaptureError::owned(crate::package::diagnostics::FailureComponent::Interface, "Update the recording indicator", None,
            "The recording indicator stopped unexpectedly.".into())),
    }
}

fn work_area(window: HWND) -> Option<RECT> {
    if window.is_null() { return None; }
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() { return None; }
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    (unsafe { GetMonitorInfoW(monitor, &mut info) } != 0).then_some(info.rcWork)
}

fn native_error(operation: &'static str) -> CaptureError {
    let error = std::io::Error::last_os_error();
    CaptureError::owned(crate::package::diagnostics::FailureComponent::Interface, operation, error.raw_os_error(), format!("{operation}: {error}"))
}
