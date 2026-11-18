//! Portable interpretation of the private Windows input wire values.

use crate::protocol::{
    Evidence, PhysicalPoint, UnavailableReason,
    ipc::{CollectedInput, HookKey, HookPointer, InputSample, RawKey},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScanPrefix {
    None,
    E0,
    E1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ScanCode {
    pub prefix: ScanPrefix,
    pub code: u8,
}

impl ScanCode {
    /// INP-2: the make code with its E0/E1 prefix folded in (0xE0xx, 0xE1xx).
    pub fn folded(self) -> u16 {
        let prefix = match self.prefix {
            ScanPrefix::None => 0,
            ScanPrefix::E0 => 0xe000,
            ScanPrefix::E1 => 0xe100,
        };
        prefix | u16::from(self.code)
    }
}

/// Serialized by the side names of the journal `mods` lists (INP-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[repr(u8)]
pub(crate) enum Modifier {
    #[serde(rename = "shift_l")]
    LeftShift = 0,
    #[serde(rename = "shift_r")]
    RightShift = 1,
    #[serde(rename = "ctrl_l")]
    LeftCtrl = 2,
    #[serde(rename = "ctrl_r")]
    RightCtrl = 3,
    #[serde(rename = "alt_l")]
    LeftAlt = 4,
    #[serde(rename = "alt_r")]
    RightAlt = 5,
    #[serde(rename = "win_l")]
    LeftWin = 6,
    #[serde(rename = "win_r")]
    RightWin = 7,
}

impl Modifier {
    pub const ALL: [Self; 8] = [Self::LeftShift, Self::RightShift, Self::LeftCtrl, Self::RightCtrl,
        Self::LeftAlt, Self::RightAlt, Self::LeftWin, Self::RightWin];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct Key {
    pub scan: Option<ScanCode>,
    pub virtual_key: u16,
    pub down: bool,
    pub modifier: Option<Modifier>,
}

impl Key {
    /// Known incompatible scans never match. A missing scan can still match
    /// by virtual key, but correlation must carry that lesser confidence.
    pub fn compatible(self, other: Self) -> bool {
        self.down == other.down && self.virtual_key == other.virtual_key
            && match (self.scan, other.scan) {
                (Some(left), Some(right)) => left == right,
                _ => true,
            }
    }
}

/// Serialized as the button number 1–5 (INP-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(into = "u8")]
pub(crate) enum Button {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl From<Button> for u8 {
    fn from(button: Button) -> Self { button as u8 + 1 }
}

impl Button {
    pub const ALL: [Self; 5] = [Self::Left, Self::Right, Self::Middle, Self::X1, Self::X2];

    pub fn mapped(self, swapped: bool) -> Self {
        match (self, swapped) {
            (Self::Left, true) => Self::Right,
            (Self::Right, true) => Self::Left,
            _ => self,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WheelAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    Relative { x: i32, y: i32 },
    Absolute { x: i32, y: i32, desktop_point: Evidence<PhysicalPoint> },
    HookPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Key(Key),
    AltGrCompanion,
    Button { logical: Button, physical: Option<Button>, down: bool },
    Wheel { axis: WheelAxis, value: i16 },
    Motion(Motion),
    Hid,
}

/// `RI_MOUSE_WHEEL` and `RI_MOUSE_HWHEEL` with their axes.
const RAW_WHEELS: [(u16, WheelAxis); 2] = [(0x0400, WheelAxis::Vertical), (0x0800, WheelAxis::Horizontal)];

/// The parts of one packet at most: a raw pointer packet may carry a down and
/// an up per button, one tick per wheel axis and one motion.
pub(crate) const MAX_PARTS: usize = 2 * Button::ALL.len() + RAW_WHEELS.len() + 1;

/// `LLKHF_UP`: the low-level keyboard hook's key-release flag.
pub(crate) const LLKHF_UP: u32 = 0x80;
/// `VK_PACKET`: a Unicode code unit sent as a key, not a physical key.
pub(crate) const VK_PACKET: u16 = 0xe7;
/// `VK_CAPITAL`: Caps Lock.
pub(crate) const VK_CAPITAL: u16 = 0x14;

pub(crate) fn parts(
    input: &CollectedInput,
    fallback_scan: impl FnOnce(u16) -> Option<ScanCode>,
) -> [Option<Action>; MAX_PARTS] {
    let mut parts = [None; MAX_PARTS];
    match input.sample {
        InputSample::RawKey(key) => parts[0] = raw_key(key, fallback_scan).map(Action::Key),
        InputSample::HookKey(key) => {
            parts[0] = if key.scan_code == 0x21d {
                // Keep native evidence privately for P08. It is never a
                // physical left Ctrl transition or a recorder modifier.
                Some(Action::AltGrCompanion)
            } else {
                hook_key(key).map(Action::Key)
            };
        }
        InputSample::HookPointer(pointer) => parts[0] = hook_pointer(pointer),
        InputSample::Hid(_) => parts[0] = Some(Action::Hid),
        InputSample::RawPointer(pointer) => {
            let mut next = 0;
            // `Button::ALL` is in the order of the `RI_MOUSE_*_BUTTON_DOWN`/`UP` flag pairs.
            for (index, physical) in Button::ALL.into_iter().enumerate() {
                for (edge, down) in [(0, true), (1, false)] {
                    if pointer.button_flags & (1 << (2 * index + edge)) == 0 {
                        continue;
                    }
                    let logical = physical.mapped(pointer.device.is_some() && input.swapped_buttons == Some(true));
                    let physical = pointer.device.is_some().then_some(physical);
                    parts[next] = Some(Action::Button { logical, physical, down });
                    next += 1;
                }
            }
            // One RAWMOUSE may carry both position and transitions. Keep the
            // packet's declared order as deterministic subindices.
            for (flag, axis) in RAW_WHEELS {
                if pointer.button_flags & flag != 0 {
                    parts[next] = Some(Action::Wheel { axis, value: pointer.button_data as i16 });
                    next += 1;
                }
            }
            let (x, y) = (pointer.x, pointer.y);
            if pointer.move_flags & 1 != 0 {
                parts[next] = Some(Action::Motion(Motion::Absolute {
                    x, y, desktop_point: absolute_point(input, x, y),
                }));
            } else if x != 0 || y != 0 {
                parts[next] = Some(Action::Motion(Motion::Relative { x, y }));
            }
        }
    }
    parts
}

pub(crate) fn startup_modifier(input: &CollectedInput) -> Option<Modifier> {
    if !matches!(input.sample, InputSample::RawKey(_) | InputSample::HookKey(_)) {
        return None;
    }
    match parts(input, |_| None)[0] {
        Some(Action::Key(key)) => key.modifier,
        _ => None,
    }
}

fn absolute_point(input: &CollectedInput, x: i32, y: i32) -> Evidence<PhysicalPoint> {
    let Some(rect) = input.absolute_desktop else {
        return Evidence::Unavailable(UnavailableReason::NotObserved);
    };
    if !(0..=65_535).contains(&x) || !(0..=65_535).contains(&y) {
        return Evidence::Unavailable(UnavailableReason::Unsupported);
    }
    let (left, top, _, _) = rect.edges();
    let map = |raw: i32, start: i32, extent: u32| {
        (i64::from(start) + (i64::from(raw) * i64::from(extent) / 65_536)) as i32
    };
    Evidence::Observed(PhysicalPoint { x: map(x, left, rect.width()), y: map(y, top, rect.height()) })
}

fn raw_key(key: RawKey, fallback_scan: impl FnOnce(u16) -> Option<ScanCode>) -> Option<Key> {
    if key.virtual_key == VK_PACKET {
        return normalize_key(key.virtual_key, None, key.flags & 1 == 0);
    }
    if key.make_code == 0xff || key.virtual_key == 0xff {
        return None;
    }
    let prefix = match key.flags & 6 {
        2 => ScanPrefix::E0,
        4 => ScanPrefix::E1,
        _ => ScanPrefix::None,
    };
    let scan = if key.make_code == 0 {
        fallback_scan(key.virtual_key)
    } else {
        u8::try_from(key.make_code).ok().map(|code| ScanCode { prefix, code })
    };
    normalize_key(key.virtual_key, scan, key.flags & 1 == 0)
}

fn hook_key(key: HookKey) -> Option<Key> {
    if key.virtual_key == u32::from(VK_PACKET) {
        return normalize_key(VK_PACKET, None, key.flags & LLKHF_UP == 0);
    }
    if key.virtual_key == 0xff {
        return None;
    }
    let prefix = if key.flags & 1 == 0 { ScanPrefix::None } else { ScanPrefix::E0 };
    let scan = u8::try_from(key.scan_code).ok().filter(|code| *code != 0)
        .map(|code| ScanCode { prefix, code });
    normalize_key(u16::try_from(key.virtual_key).ok()?, scan, key.flags & LLKHF_UP == 0)
}

fn normalize_key(mut vk: u16, mut scan: Option<ScanCode>, down: bool) -> Option<Key> {
    match scan {
        Some(ScanCode { prefix: ScanPrefix::E0, code: 0x2a | 0x36 }) => return None,
        Some(ScanCode { prefix: ScanPrefix::None, code: 0x45 }) if vk == 0x13 => {
            scan = Some(ScanCode { prefix: ScanPrefix::E1, code: 0x45 });
        }
        Some(ScanCode { prefix: ScanPrefix::E1, code: 0x1d | 0x45 }) if vk == 0x13 => {
            scan = Some(ScanCode { prefix: ScanPrefix::E1, code: 0x45 });
        }
        Some(ScanCode { code: 0x45, .. }) if vk == 0x90 => {
            scan = Some(ScanCode { prefix: ScanPrefix::E0, code: 0x45 });
        }
        _ => {}
    }
    let extended = scan.is_some_and(|scan| scan.prefix == ScanPrefix::E0);
    vk = match vk {
        0x10 if scan.is_some_and(|scan| scan.code == 0x36) => 0xa1,
        0x10 if scan.is_some() => 0xa0,
        0x11 if extended => 0xa3,
        0x11 if scan.is_some() => 0xa2,
        0x12 if extended => 0xa5,
        0x12 if scan.is_some() => 0xa4,
        _ => vk,
    };
    let modifier = match vk {
        0xa0 => Some(Modifier::LeftShift),
        0xa1 => Some(Modifier::RightShift),
        0xa2 => Some(Modifier::LeftCtrl),
        0xa3 => Some(Modifier::RightCtrl),
        0xa4 => Some(Modifier::LeftAlt),
        0xa5 => Some(Modifier::RightAlt),
        0x5b => Some(Modifier::LeftWin),
        0x5c => Some(Modifier::RightWin),
        _ => None,
    };
    Some(Key { scan, virtual_key: vk, down, modifier })
}

fn hook_pointer(pointer: HookPointer) -> Option<Action> {
    let (logical, down) = match pointer.message {
        0x0200 => return Some(Action::Motion(Motion::HookPoint)),
        0x0201 => (Button::Left, true),
        0x0202 => (Button::Left, false),
        0x0204 => (Button::Right, true),
        0x0205 => (Button::Right, false),
        0x0207 => (Button::Middle, true),
        0x0208 => (Button::Middle, false),
        0x020b | 0x020c => {
            let button = match pointer.mouse_data >> 16 {
                1 => Button::X1,
                2 => Button::X2,
                _ => return None,
            };
            (button, pointer.message == 0x020b)
        }
        0x020a | 0x020e => {
            let axis = if pointer.message == 0x020a { WheelAxis::Vertical } else { WheelAxis::Horizontal };
            return Some(Action::Wheel { axis, value: (pointer.mouse_data >> 16) as i16 });
        }
        _ => return None,
    };
    Some(Action::Button { logical, physical: None, down })
}
