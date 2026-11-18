use windows_sys::Win32::{
    Foundation::{GENERIC_ALL, GENERIC_WRITE},
    Security::{DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, IsValidSid, OWNER_SECURITY_INFORMATION},
    Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
    },
};

use crate::win::{
    Failure, Handle, Result,
    acl::{ACCESS_ALLOWED, ACCESS_DENIED, ADMINISTRATORS, SYSTEM, acl_entries, parse_sid, security_info},
};

const SERVICING: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
const INHERIT_ONLY: u8 = INHERIT_ONLY_ACE as u8;

pub(super) fn verify(handle: &Handle, ancestor: bool) -> Result<()> {
    let failure = Failure::InstalledPermissions;
    let trusted = [parse_sid(SYSTEM, failure)?, parse_sid(ADMINISTRATORS, failure)?, parse_sid(SERVICING, failure)?];
    let trusted = |raw| trusted.iter().any(|sid| sid.matches(raw));
    let info = security_info(handle, DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION, failure)?;
    if info.owner.is_null() || unsafe { IsValidSid(info.owner) } == 0 || !trusted(info.owner) {
        return Err(failure);
    }
    let acl = unsafe { info.dacl.as_ref() }.ok_or(failure)?;
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
