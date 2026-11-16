use std::{fmt, num::NonZeroU16, path::{Path, PathBuf}};

use crate::protocol::{
    BoundedList, BoundedText, DeviceId, Evidence, MonitorId, UnavailableReason, UncertaintyUs,
};

pub const MAX_SELECTED_MONITORS: usize = 32;
const MAX_APPROVED_EXECUTABLES: usize = 64;
const MAX_CALIBRATED_DEVICES: usize = 128;
const MAX_EXECUTABLE_ID_BYTES: usize = 4096;

/// A stable display-selection key. Windows supplies the physical monitor device path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayKey(Box<str>);

impl DisplayKey {
    pub fn new(value: &str) -> Result<Self, ConfigError> {
        if value.is_empty() || value.len() > 512 || value.contains('\0') {
            return Err(ConfigError::InvalidDisplayKey);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardMode {
    Disabled,
    OptIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePace {
    Fps30,
    Fps60,
}

impl CapturePace {
    pub fn frames_per_second(self) -> u32 {
        match self {
            Self::Fps30 => 30,
            Self::Fps60 => 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableIdentity(BoundedText<MAX_EXECUTABLE_ID_BYTES>);

impl ExecutableIdentity {
    /// Takes the executable identity already resolved by the platform/profile provider.
    pub fn new(resolved_executable: &str) -> Result<Self, ConfigError> {
        if resolved_executable.trim().is_empty() || resolved_executable.contains('\0') {
            return Err(ConfigError::InvalidExecutableIdentity);
        }
        BoundedText::new(resolved_executable)
            .map(Self)
            .map_err(|_| ConfigError::InvalidExecutableIdentity)
    }

    pub fn executable(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    revision: NonZeroU16,
    approved: BoundedList<ExecutableIdentity, MAX_APPROVED_EXECUTABLES>,
    clipboard: ClipboardMode,
    capture_pace: CapturePace,
    pen_hid: bool,
}

impl ResolvedProfile {
    /// M1 supplies explicit identities through the same resolved value consumed by capture.
    /// An empty selection grants no application approval.
    pub fn m1_stub(
        revision: NonZeroU16,
        approved: Vec<ExecutableIdentity>,
        clipboard: ClipboardMode,
        capture_pace: CapturePace,
    ) -> Result<Self, ConfigError> {
        let approved = BoundedList::new(approved).map_err(|_| ConfigError::TooManyApprovedIdentities)?;
        if approved.as_slice().iter().enumerate().any(|(index, identity)| {
            approved.as_slice()[..index].contains(identity)
        }) {
            return Err(ConfigError::DuplicateApprovedIdentity);
        }
        Ok(Self { revision, approved, clipboard, capture_pace, pen_hid: false })
    }

    pub fn with_pen_hid(mut self, enabled: bool) -> Self {
        self.pen_hid = enabled;
        self
    }

    pub fn pen_hid(&self) -> bool {
        self.pen_hid
    }

    pub fn revision(&self) -> NonZeroU16 {
        self.revision
    }

    pub fn approved_identities(&self) -> &[ExecutableIdentity] {
        self.approved.as_slice()
    }

    pub fn clipboard(&self) -> ClipboardMode {
        self.clipboard
    }

    pub fn capture_pace(&self) -> CapturePace {
        self.capture_pace
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    Hardware30,
    Software15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveCapabilities {
    pub video: Evidence<VideoMode>,
    pub raw_input: Evidence<()>,
    pub low_level_hooks: Evidence<()>,
    pub pen_hid: Evidence<()>,
}

impl EffectiveCapabilities {
    pub fn awaiting_probe() -> Self {
        let reason = UnavailableReason::NotObserved;
        Self {
            video: Evidence::Unavailable(reason),
            raw_input: Evidence::Unavailable(reason),
            low_level_hooks: Evidence::Unavailable(reason),
            pen_hid: Evidence::Unavailable(reason),
        }
    }
}

/// Statistical p99.9 delay correction, with the source calibration revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceCalibration {
    pub device: DeviceId,
    pub receipt_delay_p999_us: UncertaintyUs,
    pub calibration_revision: NonZeroU16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedMonitor {
    pub id: MonitorId,
    pub key: DisplayKey,
}

#[derive(Debug, Clone)]
pub struct ResolvedCaptureConfig {
    output_root: PathBuf,
    monitors: BoundedList<SelectedMonitor, MAX_SELECTED_MONITORS>,
    profile: ResolvedProfile,
    calibrations: BoundedList<DeviceCalibration, MAX_CALIBRATED_DEVICES>,
    capabilities: EffectiveCapabilities,
}

impl ResolvedCaptureConfig {
    pub fn new(
        output_root: PathBuf,
        monitors: Vec<SelectedMonitor>,
        profile: ResolvedProfile,
        calibrations: Vec<DeviceCalibration>,
        capabilities: EffectiveCapabilities,
    ) -> Result<Self, ConfigError> {
        if output_root.as_os_str().is_empty() {
            return Err(ConfigError::EmptyOutputRoot);
        }
        if monitors.is_empty() {
            return Err(ConfigError::NoSelectedMonitors);
        }
        let monitors = BoundedList::new(monitors).map_err(|_| ConfigError::TooManySelectedMonitors)?;
        if monitors.as_slice().iter().enumerate().any(|(index, monitor)| {
            monitors.as_slice()[..index].iter().any(|previous| {
                previous.id == monitor.id || previous.key == monitor.key
            })
        }) {
            return Err(ConfigError::DuplicateSelectedMonitor);
        }
        let calibrations = BoundedList::new(calibrations).map_err(|_| ConfigError::TooManyCalibrations)?;
        if calibrations.as_slice().iter().enumerate().any(|(index, calibration)| {
            calibrations.as_slice()[..index].iter().any(|previous| previous.device == calibration.device)
        }) {
            return Err(ConfigError::DuplicateCalibration);
        }
        Ok(Self { output_root, monitors, profile, calibrations, capabilities })
    }

    pub fn output_root(&self) -> &Path {
        &self.output_root
    }

    pub fn monitors(&self) -> &[SelectedMonitor] {
        self.monitors.as_slice()
    }

    pub fn profile(&self) -> &ResolvedProfile {
        &self.profile
    }

    pub fn calibrations(&self) -> &[DeviceCalibration] {
        self.calibrations.as_slice()
    }

    pub fn calibration(&self, device: DeviceId) -> Evidence<DeviceCalibration> {
        self.calibrations().iter().find(|entry| entry.device == device).copied()
            .map_or(Evidence::Unavailable(UnavailableReason::MissingCalibration), Evidence::Observed)
    }

    pub fn capabilities(&self) -> EffectiveCapabilities {
        self.capabilities
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    EmptyOutputRoot,
    InvalidDisplayKey,
    NoSelectedMonitors,
    TooManySelectedMonitors,
    DuplicateSelectedMonitor,
    InvalidExecutableIdentity,
    TooManyApprovedIdentities,
    DuplicateApprovedIdentity,
    TooManyCalibrations,
    DuplicateCalibration,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyOutputRoot => "Choose a recording output directory.",
            Self::InvalidDisplayKey => "The display selection key is invalid.",
            Self::NoSelectedMonitors => "Select at least one display.",
            Self::TooManySelectedMonitors => "Too many displays are selected.",
            Self::DuplicateSelectedMonitor => "A display is selected more than once.",
            Self::InvalidExecutableIdentity => "An approved executable identity is invalid.",
            Self::TooManyApprovedIdentities => "Too many executable identities are approved.",
            Self::DuplicateApprovedIdentity => "An executable identity appears more than once.",
            Self::TooManyCalibrations => "Too many input device calibrations are supplied.",
            Self::DuplicateCalibration => "An input device has multiple calibrations.",
        })
    }
}

impl std::error::Error for ConfigError {}
