use std::path::Path;

use crate::{CaptureError, config::{DisplayKey, ResolvedCaptureConfig}, session::SessionHandle};

pub(crate) mod windows;

pub use screen_fix_elevated::restrict_dll_search as restrict_release_dll_search;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WindowsEvidence {
    pub product: String,
    pub product_type: u8,
    pub build: u32,
    pub update_revision: Option<u32>,
    pub edition_id: Option<String>,
    pub version: Option<String>,
    pub architecture: String,
    pub security: SecuritySupport,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecuritySupport {
    Eligible,
    Ended,
    EsuUnknown,
    Unknown,
}

#[derive(Clone)]
pub struct Display {
    pub key: DisplayKey,
    pub label: String,
}

pub fn displays() -> Result<Vec<Display>, CaptureError> {
    windows::displays().map_err(|error| error.to_error(crate::package::diagnostics::FailureComponent::Capture))
}

pub struct ApprovedWindows {
    pub monitors: Vec<DisplayKey>,
    pub evidence_incomplete: bool,
}

pub fn resolve_executable(path: &Path) -> Result<crate::config::ExecutableIdentity, CaptureError> {
    windows::preflight::resolve_executable(path)
}

pub fn approved_windows(approved: &crate::config::ExecutableIdentity) -> Result<ApprovedWindows, CaptureError> {
    windows::preflight::approved_windows(approved)
}

pub use windows::ui_window::RecorderWindow;

pub fn attach_recorder_window(id: crate::protocol::WindowId) -> Result<RecorderWindow, CaptureError> {
    RecorderWindow::attach(id)
}

pub use windows::hotkeys::RecorderHotkeys;

pub fn start_recording_with_hotkeys(
    config: ResolvedCaptureConfig,
    hotkeys: &RecorderHotkeys,
    trigger: Option<crate::session::hotkey::HotkeyActivation>,
) -> Result<SessionHandle, CaptureError> {
    use crate::platform::windows::controller;

    if !config.calibrations().is_empty() {
        return Err(CaptureError::unsupported_configuration(
            "The selected capture configuration requires the M1 recording backend."
        ));
    }
    if !hotkeys.available() {
        return Err(CaptureError::owned(crate::package::diagnostics::FailureComponent::Controller, "Register recorder hotkeys", None, "The recorder hotkey owner is unavailable.".into()));
    }
    controller::spawn(config, hotkeys.claim()?, trigger, windows::record_attempt)
}
