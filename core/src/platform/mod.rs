#[cfg(target_os = "windows")]
use std::path::Path;

use crate::{CaptureError, config::DisplayKey};

#[cfg(target_os = "windows")]
use crate::{config::ResolvedCaptureConfig, session::SessionHandle};

#[cfg(target_os = "windows")]
pub(crate) mod windows;

#[cfg(target_os = "windows")]
pub use screen_fix_elevated::restrict_dll_search as restrict_release_dll_search;

pub const VIDEO_RECORDING_AVAILABLE: bool = cfg!(target_os = "windows");

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
    #[cfg(target_os = "windows")]
    {
        windows::displays().map_err(|error| error.into_error(crate::package::diagnostics::FailureComponent::Capture))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(Vec::new())
    }
}

#[cfg(target_os = "windows")]
pub struct ApprovedWindows {
    pub monitors: Vec<DisplayKey>,
    pub evidence_incomplete: bool,
}

#[cfg(target_os = "windows")]
pub fn resolve_executable(path: &Path) -> Result<crate::config::ExecutableIdentity, CaptureError> {
    windows::preflight::resolve_executable(path)
}

#[cfg(target_os = "windows")]
pub fn approved_windows(approved: &crate::config::ExecutableIdentity) -> Result<ApprovedWindows, CaptureError> {
    windows::preflight::approved_windows(approved)
}

#[cfg(target_os = "windows")]
pub use windows::ui_window::RecorderWindow;

#[cfg(target_os = "windows")]
pub fn attach_recorder_window(id: crate::protocol::WindowId) -> Result<RecorderWindow, CaptureError> {
    RecorderWindow::attach(id)
}

#[cfg(target_os = "windows")]
pub use windows::hotkeys::RecorderHotkeys;

#[cfg(target_os = "windows")]
pub fn start_recording_with_hotkeys(
    config: ResolvedCaptureConfig,
    hotkeys: &RecorderHotkeys,
    trigger: Option<crate::session::hotkey::HotkeyActivation>,
) -> Result<SessionHandle, CaptureError> {
    use crate::{config::EffectiveCapabilities, session::windows as controller};

    if !config.calibrations().is_empty()
        || config.capabilities() != EffectiveCapabilities::awaiting_probe()
    {
        return Err(CaptureError::unsupported_configuration(
            "The selected capture configuration requires the M1 recording backend."
        ));
    }
    if !hotkeys.available() {
        return Err(CaptureError::owned(crate::package::diagnostics::FailureComponent::Controller, "Register recorder hotkeys", None, "The recorder hotkey owner is unavailable.".into()));
    }
    controller::spawn(config, hotkeys.claim()?, trigger, windows::record_attempt)
}
