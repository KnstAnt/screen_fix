use std::{ffi::c_void, mem::size_of};

use windows::{
    Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW},
    core::{PCWSTR, w},
};

use super::identity::MetadataScrubber;

const MAX_VERSION_BYTES: usize = 256 * 1024;

#[derive(serde::Serialize)]
pub(super) struct ExecutableVersion {
    product_name: Option<String>,
    version: Option<[u16; 4]>,
    version_string: Option<String>,
}

impl ExecutableVersion {
    pub fn read(path: &str, scrubber: &MetadataScrubber) -> Option<Self> {
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let path = PCWSTR(path.as_ptr());
        let size = unsafe { GetFileVersionInfoSizeW(path, None) } as usize;
        if size == 0 || size > MAX_VERSION_BYTES {
            return None;
        }
        let mut bytes = vec![0u32; size.div_ceil(size_of::<u32>())];
        let buffer = bytes.as_mut_ptr().cast();
        unsafe { GetFileVersionInfoW(path, None, size as u32, buffer) }.ok()?;
        let resource = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), size) };
        let root = query(resource, w!("\\"), size_of::<VS_FIXEDFILEINFO>(), 1)
            .filter(|bytes| bytes.len() >= size_of::<VS_FIXEDFILEINFO>())
            .map(|bytes| unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<VS_FIXEDFILEINFO>()) });
        let version = root.filter(|fixed| fixed.dwSignature == 0xfeef_04bd).map(|fixed| [
            (fixed.dwFileVersionMS >> 16) as u16, fixed.dwFileVersionMS as u16,
            (fixed.dwFileVersionLS >> 16) as u16, fixed.dwFileVersionLS as u16,
        ]);
        let translation = query(resource, w!("\\VarFileInfo\\Translation"), 4, 1).and_then(|data| {
            let language = u16::from_le_bytes(data.get(..2)?.try_into().ok()?);
            let codepage = u16::from_le_bytes(data.get(2..4)?.try_into().ok()?);
            Some((language, codepage))
        });
        let text = |name: &'static str| {
            let (language, codepage) = translation?;
            let name = format!("\\StringFileInfo\\{language:04x}{codepage:04x}\\{name}");
            let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let text = query(resource, PCWSTR(name.as_ptr()), 2, 2)?;
            if text.len() > 2048 {
                return None;
            }
            let units: Vec<_> = text.chunks_exact(2).map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                .take_while(|unit| *unit != 0).collect();
            let text = String::from_utf16(&units).ok()?;
            Some(scrubber.scrub(&text, 4096))
        };
        Some(Self { product_name: text("ProductName"), version, version_string: text("FileVersion") })
    }
}

fn query(resource: &[u8], key: PCWSTR, min: usize, unit: usize) -> Option<&[u8]> {
    let buffer = resource.as_ptr().cast::<c_void>();
    let mut found = std::ptr::null_mut();
    let mut count = 0;
    if !unsafe { VerQueryValueW(buffer, key, &mut found, &mut count) }.as_bool() {
        return None;
    }
    let size = (count as usize).checked_mul(unit)?;
    let start = (found as usize).checked_sub(buffer as usize)?;
    if size < min {
        return None;
    }
    resource.get(start..start.checked_add(size)?)
}
