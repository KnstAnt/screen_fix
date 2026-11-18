use std::{collections::BTreeSet, mem::{size_of, size_of_val}, num::NonZeroU32, path::Path};

use screen_fix_elevated::{ForegroundAccess, foreground_access};
use screen_fix_protocol::ipc::elevated::{ImagePath, runtime::{ApprovalKey, IdentityClaim}};
use windows::Win32::{Foundation::HWND as NativeWindow, Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute}};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible},
};

use crate::{
    CaptureError,
    config::{ExecutableIdentity, ResolvedCaptureConfig},
    platform::ApprovedWindows,
    protocol::PhysicalRect,
};

use super::{context::identity::{ProcessHandle, WindowProcess, approved_image_index, approved_index, canonical_executable, image_name}, display, os::{self, InventoryFailure}};

pub(crate) fn approved_windows(approved: &ExecutableIdentity) -> Result<ApprovedWindows, CaptureError> {
    let displays = display::enumerate().map_err(|error| error.to_error(crate::package::diagnostics::FailureComponent::Capture))?;
    let approved = std::slice::from_ref(approved);
    let mut scan = WindowScan { approved, candidates: process_candidates(approved)?, bounds: Vec::new(), incomplete: false };
    // EnumWindows calls back synchronously on this thread and does not retain the pointer.
    if unsafe { EnumWindows(Some(read_window), (&mut scan as *mut WindowScan<'_>) as LPARAM) } == 0 {
        return Err(error("Windows could not enumerate application windows."));
    }
    let mut monitors = Vec::new();
    for monitor in displays {
        if scan.bounds.iter().any(|bounds| bounds.intersects(monitor.bounds)) {
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
    let identity = ProcessHandle::open_instance(proof.process)
        .ok_or(()).and_then(|process| approved_index(&process, scan.approved));
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

fn process_candidates(approved: &[ExecutableIdentity]) -> Result<BTreeSet<u32>, CaptureError> {
    // The snapshot narrows candidate PIDs only. Authorization requires the
    // subsequently queried full path of a live process.
    let names: BTreeSet<_> = approved.iter().filter_map(|entry| image_name(entry.executable())).collect();
    let inventory = os::process_image_names(None).map_err(|failure| error(match failure {
        InventoryFailure::Snapshot => "Windows could not enumerate processes to verify approval.",
        InventoryFailure::FirstProcess => "Windows could not read the process inventory.",
    }))?;
    if !inventory.complete {
        return Err(error("Windows could not finish the process inventory."));
    }
    Ok(inventory.images.into_iter()
        .filter(|(pid, name)| *pid != std::process::id() && names.contains(&name.to_lowercase()))
        .map(|(pid, _)| pid)
        .collect())
}

pub(super) fn validate_processes(config: &ResolvedCaptureConfig) -> Result<(), CaptureError> {
    let approved = config.profile().approved_identities();
    if approved.is_empty() {
        return Err(error("Choose the executable whose recording is approved."));
    }
    // A candidate can exit between the snapshot and its open, so a candidate
    // that cannot be verified fails only while no approved process is found.
    let mut unverified = None;
    for pid in process_candidates(approved)? {
        let verified = NonZeroU32::new(pid).and_then(ProcessHandle::open)
            .ok_or_else(|| error("The identity of a possible approved application is unknown. Retry after closing it."))
            .and_then(|process| approved_index(&process, approved)
                .map_err(|()| error("The full path of a possible approved application could not be verified.")));
        match verified {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(failure) => { unverified.get_or_insert(failure); }
        }
    }
    Err(unverified.unwrap_or_else(|| {
        error("Open the approved application before starting the recording. Screen Fix cannot approve its own window.")
    }))
}

#[derive(Clone)]
pub(crate) struct ForegroundApproval {
    proof: WindowProcess,
    elevated: Option<IdentityClaim>,
}

impl ForegroundApproval {
    pub(crate) fn still_current(&self) -> bool {
        screen_fix_native_observation::held::on_input_desktop() && WindowProcess::foreground() == Some(self.proof)
    }

    pub(crate) fn elevated(&self) -> Option<&IdentityClaim> {
        self.elevated.as_ref()
    }

    pub(super) fn window(&self) -> WindowProcess { self.proof }

    pub(crate) fn focus_current(&self, expected: screen_fix_protocol::ipc::FocusAtReceipt) -> bool {
        screen_fix_native_observation::windows::FocusSnapshot::read(self.proof)
            .is_some_and(|snapshot| snapshot.matches_identity(expected))
    }
}

pub(super) enum Foreground {
    Approved(ForegroundApproval),
    Unapproved,
    Unknown(CaptureError),
}

impl Foreground {
    /// An approved foreground that requires the elevated collector.
    pub(super) fn is_elevated(&self) -> bool {
        matches!(self, Self::Approved(approval) if approval.elevated.is_some())
    }
}

pub(super) fn validate_foreground(config: &ResolvedCaptureConfig) -> Result<ForegroundApproval, CaptureError> {
    match classify_expected_foreground(config, None) {
        Foreground::Approved(approval) => Ok(approval),
        Foreground::Unapproved => Err(error("Bring the approved application to the foreground during the countdown, then retry.")),
        Foreground::Unknown(failure) => Err(failure),
    }
}

pub(super) fn classify_expected_foreground(config: &ResolvedCaptureConfig, expected: Option<WindowProcess>) -> Foreground {
    match foreground(config, expected) {
        Ok(Some(approval)) => Foreground::Approved(approval),
        Ok(None) => Foreground::Unapproved,
        Err(failure) => Foreground::Unknown(failure),
    }
}

fn foreground(config: &ResolvedCaptureConfig, expected: Option<WindowProcess>) -> Result<Option<ForegroundApproval>, CaptureError> {
    let proof = WindowProcess::foreground()
        .ok_or_else(|| error("The foreground window could not be verified. Bring the approved application forward and retry."))?;
    if expected.is_some_and(|expected| expected != proof) || proof.process.pid.get() == std::process::id() {
        return Ok(None);
    }
    let process = ProcessHandle::open_instance(proof.process)
        .ok_or_else(|| error("The foreground process identity could not be verified."))?;
    let image = process.image_path()
        .ok_or_else(|| error("The full path of the foreground application could not be verified."))?;
    let approved = config.profile().approved_identities();
    let Some(index) = approved_image_index(&image, approved)
        .map_err(|()| error("The full path of the foreground application could not be verified."))? else {
        return Ok(None);
    };
    let access = match foreground_access(proof.process, &image) {
        Ok(access) => access,
        Err(failure) => {
            if expected.is_some() && (failure == screen_fix_elevated::Failure::TargetChanged
                || !screen_fix_native_observation::held::on_input_desktop()
                || WindowProcess::foreground() != Some(proof))
            {
                return Err(foreground_changed());
            }
            return Err(foreground_access_error(failure));
        }
    };
    let elevated = match access {
        ForegroundAccess::Ordinary => None,
        ForegroundAccess::Elevated => Some(IdentityClaim {
            target: proof.into(),
            approval: ApprovalKey::new(approved[index].executable())
                .map_err(|_| error("The approved executable path exceeds the elevated capture limit."))?,
            image: ImagePath::new(&image)
                .map_err(|_| error("The observed executable path exceeds the elevated capture limit."))?,
        }),
    };
    let approval = ForegroundApproval { proof, elevated };
    if !approval.still_current() {
        return Err(if expected.is_none() && approval.elevated().is_some() {
            foreground_access_error(screen_fix_elevated::Failure::TargetChanged)
        } else {
            foreground_changed()
        });
    }
    Ok(Some(approval))
}

fn foreground_changed() -> CaptureError {
    error("The foreground window changed while approval was being verified. Retry with the approved application in front.")
}

fn foreground_access_error(failure: screen_fix_elevated::Failure) -> CaptureError {
    CaptureError::elevated(crate::session::SourceFailure::elevated(
        crate::session::SourceStage::Foreground, failure, false))
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

fn error(message: &'static str) -> CaptureError {
    CaptureError::owned(crate::package::diagnostics::FailureComponent::Context, "Verify approved application readiness", None, message.into())
}
