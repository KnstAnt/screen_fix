use std::{mem::size_of, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorSacl,
        LABEL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SE_DACL_PROTECTED,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT,
        },
    },
    Storage::FileSystem::FILE_ALL_ACCESS,
};

use crate::{
    process::Sid,
    trust::permissions::{LocalAllocation, acl_entries, parse_sid},
    win::{Failure, Handle, Result, wide},
};

use super::CLIENT_ACCESS;

#[derive(Clone, Copy)]
pub(super) enum Owner<'a> {
    Bootstrap { user: &'a Sid, server_logon: &'a Sid },
    Collector(&'a Sid),
}

impl Owner<'_> {
    pub(super) fn descriptor(self) -> Result<LocalAllocation> {
        let dacl = match self {
            Self::Bootstrap { user, server_logon } => {
                let user = sid_string(user)?;
                let server_logon = sid_string(server_logon)?;
                // The bootstrap server only reads, writes and waits (E4).
                format!("O:{user}D:P(A;;FA;;;SY)(A;;0x{CLIENT_ACCESS:08x};;;{server_logon})(A;;0x{CLIENT_ACCESS:08x};;;{user})")
            }
            Self::Collector(user) => {
                let user = sid_string(user)?;
                format!("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x{CLIENT_ACCESS:08x};;;{user})")
            }
        };
        let sddl = wide(format!("{dacl}S:(ML;;NW;;;ME)"));
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut descriptor, null_mut()) } == 0
            || descriptor.is_null()
        {
            return Err(Failure::PipeSecurity);
        }
        Ok(LocalAllocation(descriptor))
    }

    pub(super) fn verify(self, handle: &Handle) -> Result<()> {
        let system = parse_sid("S-1-5-18").map_err(|_| Failure::PipeSecurity)?;
        let administrators = parse_sid("S-1-5-32-544").map_err(|_| Failure::PipeSecurity)?;
        let medium = parse_sid("S-1-16-8192").map_err(|_| Failure::PipeSecurity)?;
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetSecurityInfo(handle.raw(), SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
                &mut owner, null_mut(), &mut dacl, null_mut(), &mut descriptor)
        };
        let _descriptor = LocalAllocation(descriptor);
        if status != 0 || descriptor.is_null() {
            return Err(Failure::PipeSecurity);
        }
        let mut control = 0;
        let mut revision = 0;
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || control & SE_DACL_PROTECTED as u16 == 0
        {
            return Err(Failure::PipeSecurity);
        }
        // GetSecurityInfo's SACL out pointer requires SACL_SECURITY_INFORMATION.
        // Extract the mandatory label from the descriptor queried with READ_CONTROL.
        let mut label_present = 0;
        let mut label_defaulted = 0;
        let mut sacl = null_mut();
        if unsafe { GetSecurityDescriptorSacl(descriptor, &mut label_present, &mut sacl, &mut label_defaulted) } == 0
            || label_present == 0
        {
            return Err(Failure::PipeSecurity);
        }
        let mut expected = vec![(&system, FILE_ALL_ACCESS)];
        let owner_matches = match self {
            Self::Bootstrap { user, server_logon } => {
                expected.push((server_logon, CLIENT_ACCESS));
                expected.push((user, CLIENT_ACCESS));
                user.matches(owner)
            }
            Self::Collector(user) => {
                expected.push((&administrators, FILE_ALL_ACCESS));
                expected.push((user, CLIENT_ACCESS));
                administrators.matches(owner)
            }
        };
        let dacl = acl_entries(unsafe { dacl.as_ref() }.ok_or(Failure::PipeSecurity)?)?;
        let labels = acl_entries(unsafe { sacl.as_ref() }.ok_or(Failure::PipeSecurity)?)?;
        if !owner_matches || dacl.len() != expected.len() || labels.len() != 1 {
            return Err(Failure::PipeSecurity);
        }
        for (ace, (sid, mask)) in dacl.iter().zip(expected) {
            if ace.kind != 0 || ace.flags != 0 || ace.mask != mask || !sid.matches(ace.sid) {
                return Err(Failure::PipeSecurity);
            }
        }
        let label = &labels[0];
        if label.kind != 0x11 || label.flags != 0 || label.mask != 1 || !medium.matches(label.sid) {
            return Err(Failure::PipeSecurity);
        }
        Ok(())
    }
}

pub(super) fn attributes(descriptor: &LocalAllocation) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

fn sid_string(sid: &Sid) -> Result<String> {
    let mut output = null_mut();
    if unsafe { ConvertSidToStringSidW(sid.raw(), &mut output) } == 0 || output.is_null() {
        return Err(Failure::PipeSecurity);
    }
    let length = (0..256).find(|&i| unsafe { *output.add(i) == 0 });
    let value = length.and_then(|length| {
        String::from_utf16(unsafe { std::slice::from_raw_parts(output, length) }).ok()
    });
    unsafe { LocalFree(output.cast()) };
    value.ok_or(Failure::PipeSecurity)
}
