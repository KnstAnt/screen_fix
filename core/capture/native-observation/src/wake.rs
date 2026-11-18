//! An unnamed event that wakes an owner from its wait.

use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    ptr::null,
};

use windows_sys::Win32::{
    Foundation::WAIT_FAILED,
    System::Threading::{CreateEventW, SetEvent},
    UI::WindowsAndMessaging::{MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, QS_ALLINPUT},
};

pub struct WakeEvent(OwnedHandle);

impl WakeEvent {
    /// A new unsignalled event; `manual` keeps it signalled until it is reset.
    pub fn new(manual: bool) -> io::Result<Self> {
        let event = unsafe { CreateEventW(null(), i32::from(manual), 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateEventW returned a new unnamed event that this value alone closes.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(event) }))
    }

    pub fn set(&self) -> io::Result<()> {
        if unsafe { SetEvent(self.0.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wait up to `timeout_ms` for this event or for a message in the calling
    /// thread's queue, including one already seen but not removed.
    pub fn wait_messages(&self, timeout_ms: u32) -> io::Result<()> {
        let handle = self.0.as_raw_handle();
        if unsafe { MsgWaitForMultipleObjectsEx(1, &handle, timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE) } == WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl AsRawHandle for WakeEvent {
    fn as_raw_handle(&self) -> RawHandle {
        self.0.as_raw_handle()
    }
}
