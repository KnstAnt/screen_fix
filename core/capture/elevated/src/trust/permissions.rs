use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::{GENERIC_ALL, GENERIC_WRITE},
    Security::{
        DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
    },
    Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
    },
};

use crate::win::{
    Failure, Handle, Result,
    acl::{ACCESS_ALLOWED, ACCESS_DENIED, ADMINISTRATORS, LocalAllocation, SYSTEM, acl_entries, parse_sid},
};

const SERVICING: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
const INHERIT_ONLY: u8 = INHERIT_ONLY_ACE as u8;

pub(super) fn verify(handle: &Handle, ancestor: bool) -> Result<()> {
    let failure = Failure::InstalledPermissions;
    let trusted = [parse_sid(SYSTEM, failure)?, parse_sid(ADMINISTRATORS, failure)?, parse_sid(SERVICING, failure)?];
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
        return Err(failure);
    }
    let acl = unsafe { acl.as_ref() }.ok_or(failure)?;
    let entries = acl_entries(acl, failure)?;
    // Directory FILE_WRITE_DATA/FILE_APPEND_DATA name unrelated children on ancestors.
    // Reparse setting, delete-child, ACL mutations and replacement remain forbidden.
    let mut destructive = GENERIC_ALL | GENERIC_WRITE | DELETE | WRITE_DAC | WRITE_OWNER
        | FILE_DELETE_CHILD | FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA;
    if !ancestor {
        destructive |= FILE_WRITE_DATA | FILE_APPEND_DATA;
    }
    for ace in entries {
        if ace.kind != ACCESS_ALLOWED && ace.kind != ACCESS_DENIED {
            return Err(failure);
        }
        if ace.kind == ACCESS_ALLOWED
            && ace.flags & INHERIT_ONLY == 0
            && ace.mask & destructive != 0
            && !trusted(ace.sid)
        {
            return Err(failure);
        }
    }
    Ok(())
}
