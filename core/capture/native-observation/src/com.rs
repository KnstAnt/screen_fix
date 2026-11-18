//! COM initialization scoped to one thread.

use std::marker::PhantomData;

use windows::Win32::System::Com::{COINIT, CoInitializeEx, CoUninitialize};

/// COM initialized on the current thread until drop. It is neither `Send`
/// nor `Sync`, so it is released on the thread that initialized it.
pub struct ComApartment(PhantomData<*const ()>);

impl ComApartment {
    /// Initialize COM on the current thread with `mode`.
    pub fn initialize(mode: COINIT) -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, mode) }.ok()?;
        Ok(Self(PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
