use crate::protocol::{FocusGeneration, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextTarget {
    NonText,
    Editable,
    Password,
    PossiblySensitive,
    /// A non-password field with a mixed read-only attribute: keys are
    /// published with `typing:"unknown"` and no text is inferred (D-4c).
    TypingUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BusyLevel {
    Clear,
    Soft,
    Hard,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct BusyReasons {
    pub owned_window: bool,
    pub menu: bool,
    pub move_size: bool,
    pub text_entry: bool,
    pub hung: bool,
    pub pointer_held: bool,
    pub recent_input: bool,
}

impl BusyReasons {
    pub fn level(self) -> BusyLevel {
        if self.owned_window || self.menu || self.move_size || self.text_entry || self.hung {
            BusyLevel::Hard
        } else if self.pointer_held || self.recent_input {
            BusyLevel::Soft
        } else {
            BusyLevel::Clear
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BusyHint {
    pub generation: FocusGeneration,
    pub window: WindowId,
    pub level: BusyLevel,
    pub reasons: BusyReasons,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InputSettings {
    pub pointer_speed: Option<i32>,
    pub enhance_pointer_precision: Option<bool>,
    pub wheel_lines: Option<u32>,
    pub wheel_chars: Option<u32>,
    pub swapped_buttons: bool,
    pub double_click_ms: u32,
    pub double_click_size: [i32; 2],
    pub drag_threshold: [i32; 2],
    pub installed_keyboard_layouts: Option<Vec<u64>>,
    pub known_controller_processes: Option<Vec<ControllerHint>>,
    #[serde(default)]
    pub touchpad_parameters: TouchpadParameters,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum TouchpadParameters {
    #[default]
    Unavailable,
    SpiV1 {
        max_supported_contacts: u32,
        precision_touchpad_present: bool,
        legacy_touchpad_features: u32,
        status_flags: u32,
        settings_flags: u32,
        sensitivity_level: u32,
        cursor_speed: u32,
        feedback_intensity: u32,
        click_force_sensitivity: u32,
        right_click_zone: [u32; 2],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerHint {
    TabletDriver,
    ThreeDxWare,
    StreamDeck,
    TourBox,
    LogitechOptions,
    AutoHotkey,
    PowerToys,
}
