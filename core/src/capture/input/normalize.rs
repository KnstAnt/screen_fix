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

/// The side-specific virtual key of each modifier, in discriminant order.
const MODIFIER_VKS: [(Modifier, u16); 8] = [
    (Modifier::LeftShift, VK_LSHIFT), (Modifier::RightShift, VK_RSHIFT),
    (Modifier::LeftCtrl, VK_LCONTROL), (Modifier::RightCtrl, VK_RCONTROL),
    (Modifier::LeftAlt, VK_LMENU), (Modifier::RightAlt, VK_RMENU),
    (Modifier::LeftWin, VK_LWIN), (Modifier::RightWin, VK_RWIN),
];

impl Modifier {
    pub const ALL: [Self; 8] = [Self::LeftShift, Self::RightShift, Self::LeftCtrl, Self::RightCtrl,
        Self::LeftAlt, Self::RightAlt, Self::LeftWin, Self::RightWin];

    /// The side-specific virtual key.
    pub fn vk(self) -> u16 {
        MODIFIER_VKS[self as usize].1
    }

    /// The modifier of a side-specific virtual key.
    pub fn from_vk(vk: u16) -> Option<Self> {
        MODIFIER_VKS.iter().find(|(_, key)| *key == vk).map(|(modifier, _)| *modifier)
    }
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

/// `MOUSE_MOVE_ABSOLUTE`: the raw pointer packet carries absolute coordinates.
const MOUSE_MOVE_ABSOLUTE: u16 = 0x01;

/// `RI_KEY_BREAK`: the raw keyboard record is a key release.
pub(super) const RI_KEY_BREAK: u16 = 0x01;
/// `RI_KEY_E0` and `RI_KEY_E1`: the scan code prefix of a raw keyboard record.
const RI_KEY_E0: u16 = 0x02;
const RI_KEY_E1: u16 = 0x04;
/// `KEYBOARD_OVERRUN_MAKE_CODE`: a raw keyboard record without a key.
const KEYBOARD_OVERRUN_MAKE_CODE: u16 = 0xff;
/// `LLKHF_EXTENDED`: the low-level keyboard hook's E0 prefix flag.
const LLKHF_EXTENDED: u32 = 0x01;
/// `LLKHF_UP`: the low-level keyboard hook's key-release flag.
pub(crate) const LLKHF_UP: u32 = 0x80;
/// The low-level keyboard hook's scan code of the left-Ctrl companion that
/// the system sends with the right Alt of an AltGr press.
const ALTGR_COMPANION_SCAN: u32 = 0x21d;

/// The scan codes that disambiguate the virtual keys below.
const SCAN_CTRL: u8 = 0x1d;
const SCAN_LEFT_SHIFT: u8 = 0x2a;
const SCAN_RIGHT_SHIFT: u8 = 0x36;
const SCAN_NUM_LOCK: u8 = 0x45;

/// No virtual key: the record has no key to report.
const VK_NONE: u16 = 0xff;
/// `VK_PACKET`: a Unicode code unit sent as a key, not a physical key.
pub(crate) const VK_PACKET: u16 = 0xe7;
/// `VK_CAPITAL`: Caps Lock.
pub(crate) const VK_CAPITAL: u16 = 0x14;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_PAUSE: u16 = 0x13;
const VK_NUMLOCK: u16 = 0x90;
const VK_LSHIFT: u16 = 0xa0;
const VK_RSHIFT: u16 = 0xa1;
const VK_LCONTROL: u16 = 0xa2;
const VK_RCONTROL: u16 = 0xa3;
const VK_LMENU: u16 = 0xa4;
const VK_RMENU: u16 = 0xa5;
const VK_LWIN: u16 = 0x5b;
const VK_RWIN: u16 = 0x5c;

/// The pointer messages of the low-level mouse hook.
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020a;
const WM_XBUTTONDOWN: u32 = 0x020b;
const WM_XBUTTONUP: u32 = 0x020c;
const WM_MOUSEHWHEEL: u32 = 0x020e;
/// `XBUTTON1` and `XBUTTON2` in the high word of an X button message's mouse data.
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

/// `MI_WP_SIGNATURE`: the pen and touch promotion signature in the extra
/// information of a promoted mouse message, under `PROMOTION_SIGNATURE_MASK`.
const PROMOTION_SIGNATURE: u64 = 0xff51_5700;
const PROMOTION_SIGNATURE_MASK: u64 = 0xffff_ff00;
/// In the low byte of a promoted message's extra information: set for touch, clear for pen.
pub(crate) const PROMOTED_TOUCH: u8 = 0x80;
/// In the low byte of a promoted message's extra information: the pen or touch cursor id.
pub(crate) const PROMOTED_CURSOR_ID: u8 = 0x7f;

/// A mouse message Windows promoted from pen or touch input.
pub(crate) fn promoted(extra: u64) -> bool {
    extra & PROMOTION_SIGNATURE_MASK == PROMOTION_SIGNATURE
}

pub(crate) fn parts(
    input: &CollectedInput,
    fallback_scan: impl FnOnce(u16) -> Option<ScanCode>,
) -> [Option<Action>; MAX_PARTS] {
    let mut parts = [None; MAX_PARTS];
    match input.sample {
        InputSample::RawKey(key) => parts[0] = raw_key(key, fallback_scan).map(Action::Key),
        InputSample::HookKey(key) => {
            parts[0] = if key.scan_code == ALTGR_COMPANION_SCAN {
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
            if pointer.move_flags & MOUSE_MOVE_ABSOLUTE != 0 {
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
        return normalize_key(key.virtual_key, None, key.flags & RI_KEY_BREAK == 0);
    }
    if key.make_code == KEYBOARD_OVERRUN_MAKE_CODE || key.virtual_key == VK_NONE {
        return None;
    }
    let prefix = match key.flags & (RI_KEY_E0 | RI_KEY_E1) {
        RI_KEY_E0 => ScanPrefix::E0,
        RI_KEY_E1 => ScanPrefix::E1,
        _ => ScanPrefix::None,
    };
    let scan = if key.make_code == 0 {
        fallback_scan(key.virtual_key)
    } else {
        u8::try_from(key.make_code).ok().map(|code| ScanCode { prefix, code })
    };
    normalize_key(key.virtual_key, scan, key.flags & RI_KEY_BREAK == 0)
}

fn hook_key(key: HookKey) -> Option<Key> {
    if key.virtual_key == u32::from(VK_PACKET) {
        return normalize_key(VK_PACKET, None, key.flags & LLKHF_UP == 0);
    }
    if key.virtual_key == u32::from(VK_NONE) {
        return None;
    }
    let prefix = if key.flags & LLKHF_EXTENDED == 0 { ScanPrefix::None } else { ScanPrefix::E0 };
    let scan = u8::try_from(key.scan_code).ok().filter(|code| *code != 0)
        .map(|code| ScanCode { prefix, code });
    normalize_key(u16::try_from(key.virtual_key).ok()?, scan, key.flags & LLKHF_UP == 0)
}

fn normalize_key(mut vk: u16, mut scan: Option<ScanCode>, down: bool) -> Option<Key> {
    // An E0-prefixed shift is a fake shift. Pause and Num Lock share the make code 0x45.
    match scan {
        Some(ScanCode { prefix: ScanPrefix::E0, code: SCAN_LEFT_SHIFT | SCAN_RIGHT_SHIFT }) => return None,
        Some(ScanCode { prefix: ScanPrefix::None, code: SCAN_NUM_LOCK }) if vk == VK_PAUSE => {
            scan = Some(ScanCode { prefix: ScanPrefix::E1, code: SCAN_NUM_LOCK });
        }
        Some(ScanCode { prefix: ScanPrefix::E1, code: SCAN_CTRL | SCAN_NUM_LOCK }) if vk == VK_PAUSE => {
            scan = Some(ScanCode { prefix: ScanPrefix::E1, code: SCAN_NUM_LOCK });
        }
        Some(ScanCode { code: SCAN_NUM_LOCK, .. }) if vk == VK_NUMLOCK => {
            scan = Some(ScanCode { prefix: ScanPrefix::E0, code: SCAN_NUM_LOCK });
        }
        _ => {}
    }
    let extended = scan.is_some_and(|scan| scan.prefix == ScanPrefix::E0);
    vk = match vk {
        VK_SHIFT if scan.is_some_and(|scan| scan.code == SCAN_RIGHT_SHIFT) => VK_RSHIFT,
        VK_SHIFT if scan.is_some() => VK_LSHIFT,
        VK_CONTROL if extended => VK_RCONTROL,
        VK_CONTROL if scan.is_some() => VK_LCONTROL,
        VK_MENU if extended => VK_RMENU,
        VK_MENU if scan.is_some() => VK_LMENU,
        _ => vk,
    };
    Some(Key { scan, virtual_key: vk, down, modifier: Modifier::from_vk(vk) })
}

fn hook_pointer(pointer: HookPointer) -> Option<Action> {
    let (logical, down) = match pointer.message {
        WM_MOUSEMOVE => return Some(Action::Motion(Motion::HookPoint)),
        WM_LBUTTONDOWN => (Button::Left, true),
        WM_LBUTTONUP => (Button::Left, false),
        WM_RBUTTONDOWN => (Button::Right, true),
        WM_RBUTTONUP => (Button::Right, false),
        WM_MBUTTONDOWN => (Button::Middle, true),
        WM_MBUTTONUP => (Button::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = match pointer.mouse_data >> 16 {
                XBUTTON1 => Button::X1,
                XBUTTON2 => Button::X2,
                _ => return None,
            };
            (button, pointer.message == WM_XBUTTONDOWN)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let axis = if pointer.message == WM_MOUSEWHEEL { WheelAxis::Vertical } else { WheelAxis::Horizontal };
            return Some(Action::Wheel { axis, value: (pointer.mouse_data >> 16) as i16 });
        }
        _ => return None,
    };
    Some(Action::Button { logical, physical: None, down })
}
