use std::{collections::BTreeSet, mem::{size_of, size_of_val}, num::NonZeroU32, os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle}, path::Path, ptr::null_mut};

use windows::Win32::{Foundation::HWND as NativeWindow, Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute}};
use windows_sys::Win32::{
    Foundation::{ERROR_NO_MORE_FILES, GetLastError, HWND, INVALID_HANDLE_VALUE, LPARAM, RECT},
    Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
    System::{
        Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS},
        Threading::OpenProcessToken,
    },
    UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible},
};

use crate::{
    CaptureError,
    config::{ExecutableIdentity, ResolvedCaptureConfig},
    platform::ApprovedWindows,
    protocol::PhysicalRect,
};

use super::{context::identity::{ProcessHandle, WindowProcess, canonical_executable, image_name}, display};

pub(crate) fn approved_windows(approved: &ExecutableIdentity) -> Result<ApprovedWindows, CaptureError> {
    let displays = display::enumerate()?;
    let approved = std::slice::from_ref(approved);
    let mut scan = WindowScan { approved, candidates: process_candidates(approved)?, bounds: Vec::new(), incomplete: false };
    // EnumWindows calls back synchronously on this thread and does not retain the pointer.
    if unsafe { EnumWindows(Some(read_window), (&mut scan as *mut WindowScan<'_>) as LPARAM) } == 0 {
        return Err(error("Windows could not enumerate application windows."));
    }
    let mut monitors = Vec::new();
    for monitor in displays {
        if scan.bounds.iter().any(|bounds| overlaps(*bounds, monitor.bounds)) {
            monitors.push(monitor.key);
        }
    }
    Ok(ApprovedWindows { monitors, evidence_incomplete: scan.incomplete })
}

struct WindowScan<'a> {
    approved: &'a [ExecutableIdentity],
    candidates: BTreeSet<u32>,
    bounds: Vec<PhysicalRect>,
    incomplete: bool,
}

unsafe extern "system" fn read_window(window: HWND, data: LPARAM) -> i32 {
    let scan = unsafe { &mut *(data as *mut WindowScan<'_>) };
    if unsafe { IsWindowVisible(window) } == 0 || unsafe { IsIconic(window) } != 0 {
        return 1;
    }
    let mut pid = 0;
    if unsafe { GetWindowThreadProcessId(window, &mut pid) } == 0 || !scan.candidates.contains(&pid) {
        return 1;
    }
    let hwnd = NativeWindow(window);
    let Some(proof) = WindowProcess::read(hwnd).filter(|proof| proof.process.pid.get() == pid) else {
        scan.incomplete = true;
        return 1;
    };
    let identity = ProcessHandle::open(proof.process.pid)
        .filter(|process| process.instance(proof.process.pid) == Some(proof.process))
        .ok_or(()).and_then(|process| process.approved_index(scan.approved));
    match identity {
        Ok(Some(_)) => {}
        Ok(None) => return 1,
        Err(()) => {
            scan.incomplete = true;
            return 1;
        }
    }
    let mut cloaked = 0_u32;
    if unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), size_of::<u32>() as u32) }.is_ok() && cloaked != 0 {
        return 1;
    }
    let mut bounds = windows::Win32::Foundation::RECT::default();
    let extended = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS,
        (&mut bounds as *mut windows::Win32::Foundation::RECT).cast(), size_of_val(&bounds) as u32) }.is_ok();
    let rect = if extended {
        PhysicalRect::new(bounds.left, bounds.top, bounds.right, bounds.bottom)
    } else {
        let mut fallback = RECT::default();
        (unsafe { GetWindowRect(window, &mut fallback) } != 0)
            .then(|| PhysicalRect::new(fallback.left, fallback.top, fallback.right, fallback.bottom)).flatten()
    };
    if let Some(rect) = rect.filter(|_| WindowProcess::read(hwnd) == Some(proof)) {
        scan.bounds.push(rect);
    } else {
        scan.incomplete = true;
    }
    1
}

fn overlaps(left: PhysicalRect, right: PhysicalRect) -> bool {
    let (x1, y1, x2, y2) = left.edges();
    let (a1, b1, a2, b2) = right.edges();
    x1 < a2 && a1 < x2 && y1 < b2 && b1 < y2
}

fn process_candidates(approved: &[ExecutableIdentity]) -> Result<BTreeSet<u32>, CaptureError> {
    // The snapshot narrows candidate PIDs only. Authorization requires the
    // subsequently queried full path of a live process.
    let names: BTreeSet<_> = approved.iter().filter_map(|entry| image_name(entry.executable())).collect();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(error("Windows could not enumerate processes to verify approval and elevation."));
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    if unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(error("Windows could not read the process inventory."));
    }
    let mut candidates = BTreeSet::new();
    loop {
        let len = entry.szExeFile.iter().position(|unit| *unit == 0).unwrap_or(entry.szExeFile.len());
        if entry.th32ProcessID != std::process::id()
            && names.contains(&String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase())
        {
            candidates.insert(entry.th32ProcessID);
        }
        if unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } == 0 {
            if unsafe { GetLastError() } != ERROR_NO_MORE_FILES {
                return Err(error("Windows could not finish the process inventory."));
            }
            break;
        }
    }
    Ok(candidates)
}

pub(super) fn validate_processes(config: &ResolvedCaptureConfig) -> Result<(), CaptureError> {
    let approved = config.profile().approved_identities();
    if approved.is_empty() {
        return Err(error("Choose the executable whose recording is approved."));
    }
    let mut found = false;
    for pid in process_candidates(approved)? {
        let process = NonZeroU32::new(pid).and_then(ProcessHandle::open)
            .ok_or_else(|| error("The identity or elevation of a possible approved application is unknown. Retry after closing it."))?;
        let approved = process.approved_index(approved)
            .map_err(|()| error("The full path of a possible approved application could not be verified."))?;
        if approved.is_some() {
            check_elevation(&process)?;
            found = true;
        }
    }
    if !found {
        return Err(error("Open the approved application before starting the recording. Screen Fix cannot approve its own window."));
    }
    Ok(())
}

pub(crate) struct ForegroundApproval(WindowProcess);

impl ForegroundApproval {
    pub(crate) fn still_current(&self) -> bool {
        WindowProcess::foreground() == Some(self.0)
    }
}

pub(super) fn validate_foreground(config: &ResolvedCaptureConfig) -> Result<ForegroundApproval, CaptureError> {
    let proof = WindowProcess::foreground()
        .ok_or_else(|| error("The foreground window could not be verified. Bring the approved application forward and retry."))?;
    if proof.process.pid.get() == std::process::id() {
        return Err(error("Bring the approved application to the foreground during the countdown, then retry."));
    }
    let process = ProcessHandle::open(proof.process.pid)
        .filter(|process| process.instance(proof.process.pid) == Some(proof.process))
        .ok_or_else(|| error("The foreground process identity could not be verified."))?;
    if process.approved_index(config.profile().approved_identities())
        .map_err(|()| error("The full path of the foreground application could not be verified."))?.is_none()
    {
        return Err(error("Bring the approved application to the foreground during the countdown, then retry."));
    }
    check_elevation(&process)?;
    let approval = ForegroundApproval(proof);
    if !approval.still_current() {
        return Err(error("The foreground window changed while approval was being verified. Retry with the approved application in front."));
    }
    Ok(approval)
}

pub(crate) fn resolve_executable(path: &Path) -> Result<ExecutableIdentity, CaptureError> {
    canonical_executable(path).ok_or_else(|| error("The selected executable could not be resolved to its full Windows path."))
}

pub(super) fn window_coverage(config: &ResolvedCaptureConfig) -> crate::session::WindowCoverage {
    let mut incomplete = false;
    let mut unselected = BTreeSet::new();
    for approved in config.profile().approved_identities() {
        match approved_windows(approved) {
            Ok(windows) => {
                incomplete |= windows.evidence_incomplete;
                unselected.extend(windows.monitors.into_iter()
                    .filter(|key| !config.monitors().iter().any(|entry| entry.key == *key))
                    .map(|key| key.as_str().to_owned()));
            }
            Err(_) => incomplete = true,
        }
    }
    crate::session::WindowCoverage { unselected: unselected.len(), incomplete }
}

fn check_elevation(process: &ProcessHandle) -> Result<(), CaptureError> {
    let unknown = || error("Application elevation could not be verified. Restart the approved application without administrator rights.");
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process.raw().0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(unknown());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut used = 0;
    if unsafe { GetTokenInformation(token.as_raw_handle(), TokenElevation, (&mut elevation as *mut TOKEN_ELEVATION).cast(),
        size_of::<TOKEN_ELEVATION>() as u32, &mut used) } == 0 || used as usize != size_of::<TOKEN_ELEVATION>()
    {
        return Err(unknown());
    }
    if elevation.TokenIsElevated != 0 {
        return Err(error("The approved application is running with elevated rights. Restart it without administrator rights."));
    }
    Ok(())
}

fn error(message: &'static str) -> CaptureError {
    CaptureError::backend("Verify approved application readiness", None, message.into())
}
