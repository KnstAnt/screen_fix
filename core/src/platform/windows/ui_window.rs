use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::GetWindowThreadProcessId,
};

use crate::{CaptureError, protocol::WindowId, session::SessionHandle};

mod indicator;

/// Holds the native lifecycle of a recorder-owned viewport on its UI thread.
/// Attach is the platform boundary for installing future per-window policy.
pub struct RecorderWindow {
    id: WindowId,
    indicator: indicator::Indicator,
}

impl RecorderWindow {
    pub(crate) fn attach(id: WindowId) -> Result<Self, CaptureError> {
        let window = id.0 as usize as *mut _;
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(window, &mut process) };
        if thread == 0 || thread != unsafe { GetCurrentThreadId() } || process != std::process::id() {
            return Err(CaptureError::owned(crate::package::diagnostics::FailureComponent::Interface, "Track the recorder window", None,
                "The native recorder window does not belong to the current UI thread.".into()));
        }
        Ok(Self { id, indicator: indicator::Indicator::new(window)? })
    }

    pub fn id(&self) -> WindowId { self.id }

    pub fn observe(&mut self, session: &SessionHandle) -> Result<(), CaptureError> {
        self.indicator.observe(session)
    }

    pub fn clear_indicator(&mut self) { self.indicator.clear(); }

    pub fn take_indicator_error(&mut self) -> Option<CaptureError> { self.indicator.take_error() }
}
