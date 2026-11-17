use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::{GENERIC_ALL, GENERIC_WRITE, LocalFree},
    Security::{
        ACL, ACE_HEADER, DACL_SECURITY_INFORMATION, GetAce, IsValidAcl,
        IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        Authorization::{ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT},
    },
    Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
    },
};

use crate::{process::Sid, win::{Failure, Handle, Result, wide}};

const SYSTEM: &str = "S-1-5-18";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const SERVICING: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
const ACCESS_ALLOWED: u8 = 0;
const ACCESS_DENIED: u8 = 1;
const INHERIT_ONLY: u8 = 8;

pub(crate) struct LocalAllocation(pub(crate) *mut core::ffi::c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

pub(crate) fn parse_sid(text: &str) -> Result<Sid> {
    let mut raw = null_mut();
    if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut raw) } == 0 {
        return Err(Failure::InstalledPermissions);
    }
    let _allocation = LocalAllocation(raw);
    unsafe { Sid::copy(raw) }.ok_or(Failure::InstalledPermissions)
}

pub(super) fn verify(handle: &Handle, ancestor: bool) -> Result<()> {
    let trusted = [parse_sid(SYSTEM)?, parse_sid(ADMINISTRATORS)?, parse_sid(SERVICING)?];
    let trusted = |raw| trusted.iter().any(|sid| sid.matches(raw));
    let mut owner = null_mut();
    let mut acl = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let error = unsafe {
        GetSecurityInfo(handle.raw(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            &mut owner, null_mut(), &mut acl, null_mut(), &mut descriptor)
    };
    let _descriptor = LocalAllocation(descriptor);
    if error != 0 || owner.is_null() || unsafe { IsValidSid(owner) } == 0 || !trusted(owner) {
        return Err(Failure::InstalledPermissions);
    }
    let acl = unsafe { acl.as_ref() }.ok_or(Failure::InstalledPermissions)?;
    let entries = acl_entries(acl).map_err(|_| Failure::InstalledPermissions)?;
    // Directory FILE_WRITE_DATA/FILE_APPEND_DATA name unrelated children on ancestors.
    // Reparse setting, delete-child, ACL mutations and replacement remain forbidden.
    let mut destructive = GENERIC_ALL | GENERIC_WRITE | DELETE | WRITE_DAC | WRITE_OWNER
        | FILE_DELETE_CHILD | FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA;
    if !ancestor {
        destructive |= FILE_WRITE_DATA | FILE_APPEND_DATA;
    }
    for ace in entries {
        if ace.kind != ACCESS_ALLOWED && ace.kind != ACCESS_DENIED {
            return Err(Failure::InstalledPermissions);
        }
        if ace.kind == ACCESS_ALLOWED
            && ace.flags & INHERIT_ONLY == 0
            && ace.mask & destructive != 0
            && !trusted(ace.sid)
        {
            return Err(Failure::InstalledPermissions);
        }
    }
    Ok(())
}

pub(crate) struct Ace {
    pub(crate) kind: u8,
    pub(crate) flags: u8,
    pub(crate) mask: u32,
    pub(crate) sid: PSID,
}

pub(crate) fn acl_entries(acl: &ACL) -> Result<Vec<Ace>> {
    // Both ACL APIs only inspect this descriptor. Some SDK declarations
    // retain mutable pointer types for these read-only arguments.
    let acl_ptr = (acl as *const ACL).cast_mut();
    if unsafe { IsValidAcl(acl_ptr) } == 0 {
        return Err(Failure::PipeSecurity);
    }
    let mut entries = Vec::with_capacity(usize::from(acl.AceCount));
    for i in 0..u32::from(acl.AceCount) {
        let mut raw = null_mut();
        if unsafe { GetAce(acl_ptr, i, &mut raw) } == 0 || raw.is_null() {
            return Err(Failure::PipeSecurity);
        }
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        // SID-bearing basic allow, deny and mandatory-label ACEs all use this layout.
        if !matches!(header.AceType, 0 | 1 | 0x11) || header.AceSize < 16 {
            return Err(Failure::PipeSecurity);
        }
        let mask = unsafe { *raw.cast::<u32>().add(1) };
        let sid = unsafe { raw.cast::<u8>().add(8) };
        let count = unsafe { *sid.add(1) };
        if 16 + u16::from(count) * 4 != header.AceSize || unsafe { IsValidSid(sid.cast()) } == 0 {
            return Err(Failure::PipeSecurity);
        }
        entries.push(Ace { kind: header.AceType, flags: header.AceFlags, mask, sid: sid.cast() });
    }
    Ok(entries)
}
