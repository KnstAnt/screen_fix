//! Private keyboard reconstruction. The gate is the only caller and only
//! emits text after independently verifying receipt approval and focus.

use std::collections::VecDeque;

use serde::Serialize;
use windows_sys::Win32::{
    Globalization::{NormalizationC, NormalizationD, NormalizeString},
    UI::Input::KeyboardAndMouse::{
        HKL, ToUnicodeEx, VK_CAPITAL, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU, VK_NUMLOCK, VK_RCONTROL,
        VK_RMENU, VK_RSHIFT, VK_SCROLL, VK_SHIFT,
    },
};

use crate::{
    capture::input::{Correlation, HeldInput, Key, Modifier, VK_PACKET, injected_hook, modifier_bit},
    protocol::ipc::InputSample,
};

use super::super::context::ImeState;

const DEAD_COMBINATIONS: usize = 64;
/// The high bit of a key state entry: the key is down.
const KEY_DOWN: u8 = 0x80;
/// `ToUnicodeEx` flags: bit 0, a menu is active (no Alt+Numpad entry), and
/// bit 2, the keyboard state of the thread is left unchanged.
const TO_UNICODE_FLAGS: u32 = 0x5;

/// The keyboard layout handle of a foreground focus record, when it fits a pointer.
pub(super) fn hkl(layout: u64) -> Option<HKL> {
    usize::try_from(layout).ok().map(|layout| layout as HKL)
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Typing {
    NonText,
    Inferred,
    Unknown,
    ImeComposing,
    DeadKey,
    InjectedUnicode,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TextOrigin {
    LayoutInferred,
    InjectedUnicode,
    TextEditCompositionFinalized,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TextStatus {
    Inferred,
    Confirmed,
    Injected,
    Invalid,
    /// Text was typed but could not be reconstructed (INP-3); `text` is null.
    Unknown,
}

#[derive(Serialize)]
pub(super) struct TextCommit {
    pub text: Option<String>,
    pub provenance: TextOrigin,
    pub status: TextStatus,
}

impl TextCommit {
    fn inferred(text: String) -> Self {
        Self { text: Some(text), provenance: TextOrigin::LayoutInferred, status: TextStatus::Inferred }
    }

    fn packet(ch: char, valid: bool) -> Self {
        Self {
            text: Some(ch.into()), provenance: TextOrigin::InjectedUnicode,
            status: if valid { TextStatus::Injected } else { TextStatus::Invalid },
        }
    }

    pub fn unknown() -> Self {
        Self { text: None, provenance: TextOrigin::LayoutInferred, status: TextStatus::Unknown }
    }
}

/// Virtual keys whose down can type a character: space, digits, letters,
/// the numeric keypad, OEM punctuation and VK_PACKET.
pub(super) fn text_key(vk: u16) -> bool {
    matches!(vk, 0x20 | 0x30..=0x39 | 0x41..=0x5a | 0x60..=0x6f | 0xba..=0xc0 | 0xdb..=0xdf | 0xe2 | 0xe7)
}

pub(super) struct Output {
    pub typing: Typing,
    pub commits: Vec<TextCommit>,
}

#[derive(Default)]
pub(super) struct KeyboardText {
    dead: Option<char>,
    layout: Option<u64>,
    combinations: VecDeque<((char, char), Option<char>)>,
    high_surrogate: Option<u16>,
    alt_numpad: bool,
    alt_numpad_unknown: bool,
}

impl KeyboardText {
    pub fn reset(&mut self) {
        self.dead = None;
        self.high_surrogate = None;
        self.alt_numpad = false;
        self.alt_numpad_unknown = false;
    }

    pub fn finish_packet(&mut self) -> Option<TextCommit> {
        self.high_surrogate.take().map(|_| TextCommit::packet(char::REPLACEMENT_CHARACTER, false))
    }

    pub fn key(&mut self, input: &HeldInput, key: Key, layout: u64, ime: ImeState) -> Output {
        if self.layout != Some(layout) {
            self.layout = Some(layout);
            self.combinations.clear();
            self.reset();
        }
        let mut commits = Vec::new();
        let typing = if !key.down {
            if matches!(key.modifier, Some(Modifier::LeftAlt | Modifier::RightAlt)) && self.alt_numpad {
                // Alt+Numpad entry is not rebuilt: it is written as unknown text (INP-3).
                self.alt_numpad = false;
                self.alt_numpad_unknown = true;
                commits.push(TextCommit::unknown());
                Typing::Unknown
            } else {
                Typing::NonText
            }
        } else if key.virtual_key == VK_PACKET {
            self.dead = None;
            if let Some(unit) = packet_unit(input) {
                self.packet(unit, &mut commits);
                Typing::InjectedUnicode
            } else {
                Typing::Unknown
            }
        } else {
            commits.extend(self.finish_packet());
            self.infer(input, key, layout, ime, &mut commits)
        };
        Output { typing, commits }
    }

    fn infer(&mut self, input: &HeldInput, key: Key, layout: u64, ime: ImeState, commits: &mut Vec<TextCommit>) -> Typing {
        if key.modifier.is_some() || matches!(key.virtual_key, VK_CAPITAL | VK_NUMLOCK | VK_SCROLL) {
            return Typing::NonText;
        }
        if ime == ImeState::Composing {
            self.dead = None;
            return Typing::ImeComposing;
        }
        if ime == ImeState::Unknown {
            self.dead = None;
            return Typing::Unknown;
        }
        let held = input.held;
        if held.modifiers_known != 0xff {
            self.dead = None;
            return Typing::Unknown;
        }
        let pressed = |modifier| held.modifiers & modifier_bit(modifier) != 0;
        let alt = pressed(Modifier::LeftAlt) || pressed(Modifier::RightAlt);
        let alt_gr = held.alt_gr && pressed(Modifier::RightAlt)
            && !pressed(Modifier::LeftAlt) && !pressed(Modifier::RightCtrl);
        let ctrl = pressed(Modifier::LeftCtrl) || pressed(Modifier::RightCtrl);
        let win = pressed(Modifier::LeftWin) || pressed(Modifier::RightWin);
        if alt && !alt_gr && (matches!(key.virtual_key, 0x60..=0x69 | 0x6b)
            || key.scan.is_some_and(|scan| scan.prefix == crate::capture::input::ScanPrefix::None
                && matches!(scan.code, 0x47..=0x49 | 0x4b..=0x4d | 0x4f..=0x52)))
        {
            self.alt_numpad = true;
            self.dead = None;
            return Typing::Unknown;
        }
        if std::mem::take(&mut self.alt_numpad_unknown) {
            self.dead = None;
        }
        if win || (alt || ctrl) && !alt_gr {
            self.dead = None;
            return Typing::NonText;
        }
        let Some(scan) = key.scan else { return Typing::Unknown };
        let mut state = [0_u8; 256];
        let index = |vk: u16| usize::from(vk);
        for modifier in Modifier::ALL {
            state[index(modifier.vk())] = if pressed(modifier) { KEY_DOWN } else { 0 };
        }
        state[index(VK_SHIFT)] = state[index(VK_LSHIFT)] | state[index(VK_RSHIFT)];
        state[index(VK_CONTROL)] = state[index(VK_LCONTROL)] | state[index(VK_RCONTROL)] | if alt_gr { KEY_DOWN } else { 0 };
        state[index(VK_MENU)] = state[index(VK_LMENU)] | state[index(VK_RMENU)];
        if alt_gr { state[index(VK_LCONTROL)] = KEY_DOWN; }
        let Some(key_state) = state.get_mut(index(key.virtual_key)) else { return Typing::Unknown };
        *key_state |= KEY_DOWN;
        state[index(VK_CAPITAL)] = u8::from(held.caps_lock.unwrap_or(false));
        // A 64-bit target always fits the layout handle.
        let Some(layout) = hkl(layout) else { return Typing::Unknown };
        let mut units = [0_u16; 16];
        // SAFETY: `state` is a 256-entry key state array and `units` a writable
        // buffer of the passed length; both outlive the call.
        let translate = |state: &[u8; 256], units: &mut [u16; 16]| unsafe {
            ToUnicodeEx(u32::from(key.virtual_key), u32::from(scan.code), state.as_ptr(), units.as_mut_ptr(),
                units.len() as i32, TO_UNICODE_FLAGS, layout)
        };
        let count = translate(&state, &mut units);
        if held.caps_lock.is_none() {
            let mut toggled = [0_u16; 16];
            state[index(VK_CAPITAL)] = 1;
            if translate(&state, &mut toggled) != count
                || toggled.get(..count.unsigned_abs() as usize) != units.get(..count.unsigned_abs() as usize)
            {
                self.dead = None;
                return Typing::Unknown;
            }
        }
        let Some(text) = units.get(..count.unsigned_abs() as usize).and_then(|units| String::from_utf16(units).ok()) else {
            self.dead = None;
            return Typing::Unknown;
        };
        if count < 0 {
            if let Some(next) = text.chars().next() {
                if let Some(previous) = self.dead.replace(next) {
                    commits.push(TextCommit::inferred(previous.into()));
                }
                return Typing::DeadKey;
            }
            return Typing::Unknown;
        }
        if count == 0 || text.chars().any(char::is_control) {
            return Typing::NonText;
        }
        let output = if let Some(dead) = self.dead.take() { self.compose(dead, text) } else { text };
        commits.push(TextCommit::inferred(output));
        Typing::Inferred
    }

    fn packet(&mut self, unit: u16, output: &mut Vec<TextCommit>) {
        match unit {
            0xd800..=0xdbff => {
                output.extend(self.finish_packet());
                self.high_surrogate = Some(unit);
            }
            0xdc00..=0xdfff => {
                let scalar = self.high_surrogate.take().and_then(|high| {
                    char::from_u32(0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00))
                });
                output.push(TextCommit::packet(scalar.unwrap_or(char::REPLACEMENT_CHARACTER), scalar.is_some()));
            }
            _ => {
                output.extend(self.finish_packet());
                let scalar = char::from_u32(u32::from(unit)).filter(|character| !character.is_control());
                output.push(TextCommit::packet(scalar.unwrap_or(char::REPLACEMENT_CHARACTER), scalar.is_some()));
            }
        }
    }

    fn compose(&mut self, dead: char, text: String) -> String {
        let Some(base) = text.chars().next() else { return dead.into() };
        if base == dead {
            return text;
        }
        if base == ' ' {
            let mut output = String::from(dead);
            output.push_str(&text[base.len_utf8()..]);
            return output;
        }
        let Some(mark) = combining(dead) else { return format!("{dead}{text}") };
        if normalize(base, None, NormalizationD).is_some_and(|decomposed| decomposed.contains(mark)) {
            return text;
        }
        let combination = if let Some((_, known)) = self.combinations.iter().find(|(pair, _)| *pair == (dead, base)) {
            *known
        } else {
            let candidate = normalize(base, Some(mark), NormalizationC).and_then(|combined| {
                let mut chars = combined.chars();
                let first = chars.next()?;
                chars.next().is_none().then_some(first)
            });
            if self.combinations.len() == DEAD_COMBINATIONS { self.combinations.pop_front(); }
            self.combinations.push_back(((dead, base), candidate));
            candidate
        };
        combination.map_or_else(|| format!("{dead}{text}"), |combined| format!("{combined}{}", &text[base.len_utf8()..]))
    }
}

fn packet_unit(input: &HeldInput) -> Option<u16> {
    let correlated = &input.normalized.correlated;
    if correlated.correlation != Correlation::Matched {
        return None;
    }
    correlated.slots().find_map(|slot| match slot.packet.sample {
        InputSample::HookKey(key) if injected_hook(&slot.packet.sample) && key.virtual_key == u32::from(VK_PACKET) =>
            u16::try_from(key.scan_code).ok(),
        _ => None,
    })
}

fn normalize(base: char, mark: Option<char>, form: i32) -> Option<String> {
    let mut source = [0_u16; 4];
    let mut length = base.encode_utf16(&mut source).len();
    if let Some(mark) = mark {
        length += mark.encode_utf16(&mut source[length..]).len();
    }
    let mut output = [0_u16; 8];
    // SAFETY: `source` holds `length` valid UTF-16 units and `output` is a
    // writable buffer of the passed length; both outlive the call.
    let count = unsafe { NormalizeString(form, source.as_ptr(), length as i32, output.as_mut_ptr(), output.len() as i32) };
    String::from_utf16(output.get(..usize::try_from(count).ok()?)?).ok()
}

fn combining(dead: char) -> Option<char> {
    Some(match dead {
        '`' => '\u{0300}', '\'' | '´' => '\u{0301}', '^' => '\u{0302}', '~' => '\u{0303}', '¯' => '\u{0304}',
        '˘' => '\u{0306}', '˙' => '\u{0307}', '"' | '¨' => '\u{0308}', '˚' | '°' => '\u{030a}', '˝' => '\u{030b}',
        'ˇ' => '\u{030c}', '¸' => '\u{0327}', '˛' => '\u{0328}',
        '\u{0300}'..='\u{036f}' => dead,
        _ => return None,
    })
}
