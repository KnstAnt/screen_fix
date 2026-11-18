use std::{mem::size_of, ptr::null_mut};

use windows_sys::Win32::UI::{
    Input::KeyboardAndMouse::{GetDoubleClickTime, GetKeyboardLayoutList},
    WindowsAndMessaging::*,
};

use crate::capture::context::{ControllerHint, InputSettings, TouchpadParameters};

/// A process inventory longer than this names no controllers.
const MAX_PROCESSES: usize = 16_384;

pub(super) fn read() -> InputSettings {
    let pointer: Option<[i32; 3]> = parameter(SPI_GETMOUSE);
    InputSettings {
        pointer_speed: parameter(SPI_GETMOUSESPEED),
        enhance_pointer_precision: pointer.map(|mouse| mouse[2] != 0),
        wheel_lines: parameter(SPI_GETWHEELSCROLLLINES), wheel_chars: parameter(SPI_GETWHEELSCROLLCHARS),
        swapped_buttons: unsafe { GetSystemMetrics(SM_SWAPBUTTON) != 0 },
        double_click_ms: unsafe { GetDoubleClickTime() },
        double_click_size: unsafe { [GetSystemMetrics(SM_CXDOUBLECLK), GetSystemMetrics(SM_CYDOUBLECLK)] },
        drag_threshold: unsafe { [GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)] },
        installed_keyboard_layouts: layouts(),
        known_controller_processes: controllers(),
        touchpad_parameters: touchpad_parameters(),
    }
}

fn touchpad_parameters() -> TouchpadParameters {
    const SPI_GETTOUCHPADPARAMETERS: u32 = 0x00AE;
    // Version 1 uses eleven 32-bit C slots. The two sets of BOOL
    // bitfields each occupy one slot under the Win32 ABI.
    #[repr(C)]
    struct NativeTouchpadParameters {
        version: u32,
        max_contacts: u32,
        legacy_features: u32,
        status: u32,
        settings: u32,
        sensitivity: u32,
        cursor_speed: u32,
        feedback: u32,
        click_force: u32,
        right_click_zone: [u32; 2],
    }
    let mut native = NativeTouchpadParameters {
        version: 1, max_contacts: 0, legacy_features: 0, status: 0, settings: 0,
        sensitivity: 0, cursor_speed: 0, feedback: 0, click_force: 0, right_click_zone: [0; 2],
    };
    if unsafe { SystemParametersInfoW(SPI_GETTOUCHPADPARAMETERS,
        size_of::<NativeTouchpadParameters>() as u32, (&mut native as *mut NativeTouchpadParameters).cast(), 0) } == 0
        || native.version != 1
    {
        return TouchpadParameters::Unavailable;
    }
    TouchpadParameters::SpiV1 {
        max_supported_contacts: native.max_contacts, precision_touchpad_present: native.status & 1 != 0,
        legacy_touchpad_features: native.legacy_features, status_flags: native.status & 0x7f,
        settings_flags: native.settings & 0x3ff, sensitivity_level: native.sensitivity,
        cursor_speed: native.cursor_speed, feedback_intensity: native.feedback,
        click_force_sensitivity: native.click_force, right_click_zone: native.right_click_zone,
    }
}

fn parameter<T: Default>(action: u32) -> Option<T> {
    let mut value = T::default();
    (unsafe { SystemParametersInfoW(action, 0, (&mut value as *mut T).cast(), 0) } != 0).then_some(value)
}

fn layouts() -> Option<Vec<u64>> {
    let count = unsafe { GetKeyboardLayoutList(0, null_mut()) };
    if !(1..=128).contains(&count) {
        return None;
    }
    let mut values = [null_mut(); 128];
    let written = unsafe { GetKeyboardLayoutList(count, values.as_mut_ptr()) };
    (written == count).then(|| values[..count as usize].iter().map(|entry| *entry as usize as u64).collect())
}

fn controllers() -> Option<Vec<ControllerHint>> {
    // A partial inventory still names the controllers it saw.
    let inventory = crate::platform::windows::os::process_image_names(Some(MAX_PROCESSES)).ok()?;
    if inventory.images.len() > MAX_PROCESSES {
        return None;
    }
    let mut result = Vec::new();
    for (_, name) in inventory.images {
        let hint = match name.to_ascii_lowercase().as_str() {
            "wacom_tablet.exe" | "wacom_tabletuser.exe" | "pentablet.exe" | "huiontablet.exe" => Some(ControllerHint::TabletDriver),
            "3dxservice.exe" => Some(ControllerHint::ThreeDxWare),
            "streamdeck.exe" => Some(ControllerHint::StreamDeck),
            "tourboxconsole.exe" => Some(ControllerHint::TourBox),
            "logioptions.exe" | "logioptionsplus_agent.exe" => Some(ControllerHint::LogitechOptions),
            "autohotkey.exe" | "autohotkey64.exe" | "autohotkey32.exe" => Some(ControllerHint::AutoHotkey),
            "powertoys.exe" => Some(ControllerHint::PowerToys),
            _ => None,
        };
        if let Some(hint) = hint && !result.contains(&hint) {
            result.push(hint);
        }
    }
    Some(result)
}
