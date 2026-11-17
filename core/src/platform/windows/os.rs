use std::{mem::size_of, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, SYSTEMTIME},
    System::{
        Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW},
        SystemInformation::GetSystemTime,
        Threading::{GetCurrentProcess, IsWow64Process2},
    },
};

use crate::{CaptureError, platform::{SecuritySupport, WindowsEvidence}};

use super::wide;

const VERSION_KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
const PRODUCT_KEY: &str = "SYSTEM\\CurrentControlSet\\Control\\ProductOptions";
const AMD64: u16 = 0x8664;

pub(super) fn check() -> Result<WindowsEvidence, CaptureError> {
    if !cfg!(target_arch = "x86_64") {
        return Err(unsupported("Screen Fix requires a native Windows x64 executable."));
    }
    let mut process_machine = 0;
    let mut native_machine = 0;
    if unsafe { IsWow64Process2(GetCurrentProcess(), &mut process_machine, &mut native_machine) } == 0 {
        return Err(unknown("The native Windows architecture could not be verified."));
    }
    if native_machine != AMD64 || process_machine != 0 {
        return Err(unsupported("Screen Fix requires native Windows x64. Emulated architectures are unsupported."));
    }
    // ProductOptions WinNT denotes VER_NT_WORKSTATION (ProductType 1).
    match string(PRODUCT_KEY, "ProductType").as_deref() {
        Some("WinNT") => {}
        Some("LanmanNT" | "ServerNT") => return Err(unsupported("Screen Fix requires a Windows client edition.")),
        _ => return Err(unknown("The Windows client product type could not be verified.")),
    }
    let build = string(VERSION_KEY, "CurrentBuildNumber").and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| unknown("The Windows build could not be verified."))?;
    if build < 19041 {
        return Err(unsupported("Screen Fix requires Windows 10 version 2004 (build 19041) or later."));
    }
    let edition_id = string(VERSION_KEY, "EditionID");
    let update_revision = dword(VERSION_KEY, "UBR");
    let version = string(VERSION_KEY, "DisplayVersion").or_else(|| string(VERSION_KEY, "ReleaseId"));
    let family = if build >= 22000 { "Windows 11" } else { "Windows 10" };
    let product = match edition_id.as_deref() {
        Some(edition) => format!("{family} {}", match edition {
            "Professional" => "Pro",
            "ProfessionalN" => "Pro N",
            "Core" => "Home",
            "CoreN" => "Home N",
            "EnterpriseS" => "Enterprise LTSC",
            "EnterpriseSN" => "Enterprise LTSC N",
            "IoTEnterpriseS" => "IoT Enterprise LTSC",
            other => other,
        }),
        None => family.into(),
    };
    let security = security(build, edition_id.as_deref());
    Ok(WindowsEvidence {
        product, product_type: 1, build, update_revision, edition_id, version, architecture: "x64".into(), security,
    })
}

fn security(build: u32, edition: Option<&str>) -> SecuritySupport {
    use SecuritySupport::{Eligible, Ended, EsuUnknown, Unknown};
    let Some(edition) = edition else { return Unknown; };
    let date = |end| {
        let mut now = SYSTEMTIME::default();
        unsafe { GetSystemTime(&mut now) };
        let today = u32::from(now.wYear) * 10_000 + u32::from(now.wMonth) * 100 + u32::from(now.wDay);
        if today <= end { Eligible } else { Ended }
    };
    let enterprise = matches!(edition, "Enterprise" | "EnterpriseN" | "EnterpriseG" | "EnterpriseGN"
        | "EnterpriseMultiSession" | "ServerRdsh" | "Education" | "EducationN" | "IoTEnterprise");
    let personal = matches!(edition, "Core" | "CoreN" | "CoreSingleLanguage" | "CoreCountrySpecific"
        | "Professional" | "ProfessionalN" | "ProfessionalEducation" | "ProfessionalEducationN"
        | "ProfessionalWorkstation" | "ProfessionalWorkstationN" | "CloudEdition" | "CloudEditionN");
    // Servicing channel eligibility is independent of UBR and installed updates.
    // https://learn.microsoft.com/en-us/windows/release-health/release-information
    // https://learn.microsoft.com/en-us/windows/release-health/windows11-release-information
    match (build, edition) {
        (19044, "IoTEnterpriseS") => date(20320113),
        (19044, "EnterpriseS" | "EnterpriseSN") => {
            match date(20270112) { Ended => EsuUnknown, current => current }
        }
        (19041..=19044, _) if enterprise || personal => Ended,
        (19045, _) if enterprise || personal => EsuUnknown,
        (22000, _) if enterprise || personal => date(if enterprise { 20241008 } else { 20231010 }),
        (22621, _) if enterprise || personal => date(if enterprise { 20251014 } else { 20241008 }),
        (22631, _) if enterprise || personal => date(if enterprise { 20261110 } else { 20251111 }),
        (26100, "IoTEnterpriseS") => date(20341010),
        (26100, "EnterpriseS" | "EnterpriseSN") => date(20291009),
        (26100, _) if enterprise || personal => date(if enterprise { 20271012 } else { 20261013 }),
        (26200, _) if enterprise || personal => date(if enterprise { 20281010 } else { 20271012 }),
        (26300, _) if enterprise || personal => date(if enterprise { 20291009 } else { 20281010 }),
        (28000, "IoTEnterprise") => Unknown,
        (28000, _) if enterprise || personal => date(if enterprise { 20290313 } else { 20280314 }),
        _ => Unknown,
    }
}

fn string(key: &str, value: &str) -> Option<String> {
    let mut units = [0_u16; 128];
    let mut bytes = size_of::<[u16; 128]>() as u32;
    if unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, wide(key).as_ptr(), wide(value).as_ptr(),
        RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, null_mut(), units.as_mut_ptr().cast(), &mut bytes) } != ERROR_SUCCESS
    {
        return None;
    }
    let text = String::from_utf16(units.get(..usize::try_from(bytes).ok()? / 2)?.split(|unit| *unit == 0).next()?).ok()?;
    (!text.is_empty()).then_some(text)
}

fn dword(key: &str, value: &str) -> Option<u32> {
    let mut data = 0_u32;
    let mut bytes = size_of::<u32>() as u32;
    (unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, wide(key).as_ptr(), wide(value).as_ptr(),
        RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, null_mut(), (&mut data as *mut u32).cast(), &mut bytes) } == ERROR_SUCCESS
        && bytes as usize == size_of::<u32>()).then_some(data)
}

fn unknown(message: &'static str) -> CaptureError {
    CaptureError::owned(crate::package::diagnostics::FailureComponent::Capture, "Verify Windows capability", None, message.into())
}

fn unsupported(message: &'static str) -> CaptureError {
    CaptureError::unsupported("Verify Windows capability", message)
}
