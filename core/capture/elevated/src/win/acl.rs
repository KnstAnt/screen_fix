//! Read-only inspection of security identifiers and access control lists.

use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        ACE_HEADER, ACL, GetAce, IsValidAcl, IsValidSid, OBJECT_SECURITY_INFORMATION, PSID,
        Authorization::{ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT},
    },
    System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SYSTEM_MANDATORY_LABEL_ACE_TYPE},
};

use crate::{process::Sid, win::{Failure, Handle, Result, wide}};

pub(crate) const SYSTEM: &str = "S-1-5-18";
pub(crate) const ADMINISTRATORS: &str = "S-1-5-32-544";
pub(crate) const MEDIUM_INTEGRITY: &str = "S-1-16-8192";

pub(crate) const ACCESS_ALLOWED: u8 = ACCESS_ALLOWED_ACE_TYPE as u8;
pub(crate) const ACCESS_DENIED: u8 = ACCESS_DENIED_ACE_TYPE as u8;
pub(crate) const MANDATORY_LABEL: u8 = SYSTEM_MANDATORY_LABEL_ACE_TYPE as u8;

/// The ACE header, access mask and SID header before the SID's
/// sub-authorities, in the layout shared by the ACE types above.
const ACE_FIXED_BYTES: u16 = 16;
const SUB_AUTHORITY_BYTES: u16 = 4;

/// Memory owned by `LocalFree`.
pub(crate) struct LocalAllocation(pub(crate) *mut core::ffi::c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The parts of a file object's security descriptor that `GetSecurityInfo`
/// returned. `owner` and `dacl` point into `descriptor` and are null unless
/// requested.
pub(crate) struct SecurityInfo {
    pub(crate) owner: PSID,
    pub(crate) dacl: *mut ACL,
    pub(crate) descriptor: LocalAllocation,
}

/// Query the security `information` of the file object `handle`; `failure`
/// reports a failed query.
pub(crate) fn security_info(handle: &Handle, information: OBJECT_SECURITY_INFORMATION, failure: Failure) -> Result<SecurityInfo> {
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetSecurityInfo(handle.raw(), SE_FILE_OBJECT, information, &mut owner, null_mut(), &mut dacl, null_mut(), &mut descriptor)
    };
    let descriptor = LocalAllocation(descriptor);
    if status != 0 || descriptor.0.is_null() {
        return Err(failure);
    }
    Ok(SecurityInfo { owner, dacl, descriptor })
}

/// Parse a SID string; `failure` reports an invalid one.
pub(crate) fn parse_sid(text: &str, failure: Failure) -> Result<Sid> {
    let mut raw = null_mut();
    if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut raw) } == 0 {
        return Err(failure);
    }
    let _allocation = LocalAllocation(raw);
    unsafe { Sid::copy(raw) }.ok_or(failure)
}

pub(crate) struct Ace {
    pub(crate) kind: u8,
    pub(crate) flags: u8,
    pub(crate) mask: u32,
    pub(crate) sid: PSID,
}

/// The entries of a valid ACL; `failure` reports a malformed one or an ACE
/// type other than allow, deny or mandatory label.
pub(crate) fn acl_entries(acl: &ACL, failure: Failure) -> Result<Vec<Ace>> {
    // Both ACL APIs only inspect this descriptor. Some SDK declarations
    // retain mutable pointer types for these read-only arguments.
    let acl_ptr = (acl as *const ACL).cast_mut();
    if unsafe { IsValidAcl(acl_ptr) } == 0 {
        return Err(failure);
    }
    let mut entries = Vec::with_capacity(usize::from(acl.AceCount));
    for i in 0..u32::from(acl.AceCount) {
        let mut raw = null_mut();
        if unsafe { GetAce(acl_ptr, i, &mut raw) } == 0 || raw.is_null() {
            return Err(failure);
        }
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        if !matches!(header.AceType, ACCESS_ALLOWED | ACCESS_DENIED | MANDATORY_LABEL) || header.AceSize < ACE_FIXED_BYTES {
            return Err(failure);
        }
        // The access mask follows the header; the SID follows the mask.
        let mask = unsafe { *raw.cast::<u32>().add(1) };
        let sid = unsafe { raw.cast::<u8>().add(8) };
        let count = unsafe { *sid.add(1) };
        if ACE_FIXED_BYTES + u16::from(count) * SUB_AUTHORITY_BYTES != header.AceSize
            || unsafe { IsValidSid(sid.cast()) } == 0
        {
            return Err(failure);
        }
        entries.push(Ace { kind: header.AceType, flags: header.AceFlags, mask, sid: sid.cast() });
    }
    Ok(entries)
}
