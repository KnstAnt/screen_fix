pub(crate) mod acl;

use std::{fmt, io, os::windows::ffi::OsStrExt, ffi::OsStr};

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

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DeploymentRequired => "Install the protected Windows release to use elevated capture",
            Self::InstalledPath => "Could not verify the protected installation paths",
            Self::InstalledPermissions => "The installed paths have unexpected permissions",
            Self::InstalledSignature => "Could not verify the installed publisher signature",
            Self::RevocationUnknown => "Could not confirm that the installed publisher certificate is not revoked",
            Self::ReleaseMismatch => "The installed Recorder and collector releases do not match",
            Self::ProcessIdentity => "Could not verify the original process image and instance",
            Self::TokenIdentity(_) => "Could not verify the process token or interactive access",
            Self::Arguments => "The protected startup arguments are invalid",
            Self::Busy => "An elevation request is already in flight",
            Self::ConsentDenied => "Windows elevation consent was declined",
            Self::ConsentLaunch => "Windows could not launch the protected collector",
            Self::ConsentDeadline => "Windows elevation consent did not complete before the deadline",
            Self::ReadinessDeadline => "Protected attachment did not complete before the deadline",
            Self::CleanupDeadline => "The elevated capture source did not finish cleanup before the deadline",
            Self::Cancelled => "Protected attachment was cancelled",
            Self::PeerExited => "The original peer process exited",
            Self::PipeSecurity => "Could not verify the local pipe permissions",
            Self::PipeConnection => "Could not connect the protected local pipes",
            Self::PipePeer => "A local pipe client does not match the original process",
            Self::Transport => "Protected local pipe I/O failed",
            Self::Protocol => "Protected attachment framing or binding failed",
            Self::Cryptography => "Windows cryptography failed during protected attachment",
            Self::LeaseExpired => "The elevated capture controller lease expired",
            Self::Witness => "The elevated capture desktop witness became unavailable",
            Self::ScopeClosed => "The elevated capture scope was revoked",
            Self::TargetAccess => "The elevated collector could not verify access to the active desktop",
            Self::TargetChanged => "The approved elevated target could not be verified",
            Self::Supervisor => "Could not start the protected lifetime supervisor",
            Self::RequiredRegistration => "The collector did not confirm the required input registrations",
            Self::StandardHelperUnavailable => "The Recorder input helper could not resume capture",
            Self::NativeOwnerLost | Self::NativeProducer(_) => "An elevated input producer failed",
        })
    }
}

impl std::error::Error for Failure {}

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
