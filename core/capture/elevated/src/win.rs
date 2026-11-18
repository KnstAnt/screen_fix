pub(crate) mod acl;

use std::{io, os::windows::ffi::OsStrExt, ffi::OsStr};

use screen_fix_protocol::SourceFailureReason;

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    DeploymentRequired,
    InstalledPath,
    InstalledPermissions,
    InstalledSignature,
    /// The revocation status of the installed publisher chain is unknown.
    RevocationUnknown,
    ReleaseMismatch,
    ProcessIdentity,
    TokenIdentity(screen_fix_protocol::TokenIdentityFailure),
    Arguments,
    Busy,
    ConsentDenied,
    ConsentLaunch,
    ConsentDeadline,
    ReadinessDeadline,
    CleanupDeadline,
    Cancelled,
    PeerExited,
    PipeSecurity,
    PipeConnection,
    PipePeer,
    Transport,
    Protocol,
    Cryptography,
    Supervisor,
    LeaseExpired,
    Witness,
    TargetChanged,
    TargetAccess,
    ScopeClosed,
    RequiredRegistration,
    StandardHelperUnavailable,
    NativeOwnerLost,
    NativeProducer(screen_fix_protocol::ipc::HelperFailure),
}

impl Failure {
    /// The reported reason; it drops the detail that `SourceFailure` keeps
    /// in its own fields.
    pub fn reason(self) -> SourceFailureReason {
        use SourceFailureReason as Reason;
        match self {
            Self::DeploymentRequired => Reason::DeploymentRequired,
            Self::InstalledPath => Reason::InstalledPath,
            Self::InstalledPermissions => Reason::InstalledPermissions,
            Self::InstalledSignature => Reason::InstalledSignature,
            Self::RevocationUnknown => Reason::RevocationUnknown,
            Self::ReleaseMismatch => Reason::ReleaseMismatch,
            Self::ProcessIdentity => Reason::ProcessIdentity,
            Self::TokenIdentity(_) => Reason::TokenIdentity,
            Self::Arguments => Reason::Arguments,
            Self::Busy => Reason::Busy,
            Self::ConsentDenied => Reason::ConsentDenied,
            Self::ConsentLaunch => Reason::ConsentLaunch,
            Self::ConsentDeadline => Reason::ConsentDeadline,
            Self::ReadinessDeadline => Reason::ReadinessDeadline,
            Self::CleanupDeadline => Reason::CleanupDeadline,
            Self::Cancelled => Reason::Cancelled,
            Self::PeerExited => Reason::PeerExited,
            Self::PipeSecurity => Reason::PipeSecurity,
            Self::PipeConnection => Reason::PipeConnection,
            Self::PipePeer => Reason::PipePeer,
            Self::Transport => Reason::Transport,
            Self::Protocol => Reason::Protocol,
            Self::Cryptography => Reason::Cryptography,
            Self::Supervisor => Reason::Supervisor,
            Self::LeaseExpired => Reason::LeaseExpired,
            Self::Witness => Reason::Witness,
            Self::TargetChanged => Reason::TargetChanged,
            Self::TargetAccess => Reason::TargetAccess,
            Self::ScopeClosed => Reason::ScopeClosed,
            Self::RequiredRegistration => Reason::RequiredRegistration,
            Self::StandardHelperUnavailable => Reason::StandardHelperUnavailable,
            Self::NativeOwnerLost | Self::NativeProducer(_) => Reason::NativeProducer,
        }
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

pub(crate) struct Handle(HANDLE);

// Each handle is owned, and Windows kernel handle operations used here
// support calls across threads. Shared references keep the handle live.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Take exclusive ownership of a newly returned kernel handle.
    pub(crate) unsafe fn take(raw: HANDLE, failure: Failure) -> Result<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(failure);
        }
        Ok(Self(raw))
    }

    pub(crate) fn raw(&self) -> HANDLE { self.0 }
}

impl Drop for Handle {
    fn drop(&mut self) { unsafe { CloseHandle(self.0) }; }
}

pub(crate) fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// The UTF-16 units of `text`, without a terminating NUL, for comparisons.
pub(crate) fn wide_no_nul(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().collect()
}

/// The longest path Windows reports, in UTF-16 units.
const MAX_WIDE_PATH: usize = 32768;

/// The UTF-16 string that `fill` writes into a buffer of `MAX_WIDE_PATH`
/// units, given the buffer and its capacity. `fill` returns the length
/// written, without the NUL; zero, or a length that leaves no room for the
/// NUL, reports a failure or a truncated string.
pub(crate) fn read_wide(fill: impl FnOnce(*mut u16, u32) -> u32) -> Option<Vec<u16>> {
    let mut units = vec![0_u16; MAX_WIDE_PATH];
    let length = fill(units.as_mut_ptr(), units.len() as u32) as usize;
    if length == 0 || length >= units.len() {
        return None;
    }
    units.truncate(length);
    Some(units)
}

pub(crate) fn last_error() -> u32 {
    io::Error::last_os_error().raw_os_error().unwrap_or_default() as u32
}

pub fn restrict_dll_search() -> Result<()> {
    let flags = LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR;
    if unsafe { SetDefaultDllDirectories(flags) } == 0 {
        return Err(Failure::InstalledPath);
    }
    Ok(())
}
