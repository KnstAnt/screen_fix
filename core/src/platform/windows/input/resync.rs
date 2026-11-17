use screen_fix_native_observation::held;

use crate::capture::input::{Button, HeldSynchronization, Modifier, button_bit, modifier_bit};

pub(crate) use held::on_input_desktop;

pub(super) fn read() -> Option<HeldSynchronization> {
    held::read().map(from_snapshot)
}

pub(super) fn from_snapshot(snapshot: held::HeldSnapshot) -> HeldSynchronization {
    let mut modifiers = 0;
    let mut physical_buttons = 0;
    for (modifier, pressed) in [Modifier::LeftShift, Modifier::RightShift, Modifier::LeftCtrl,
        Modifier::RightCtrl, Modifier::LeftAlt, Modifier::RightAlt, Modifier::LeftWin,
        Modifier::RightWin].into_iter().zip(snapshot.modifiers)
    {
        if pressed {
            modifiers |= modifier_bit(modifier);
        }
    }
    for (button, pressed) in [Button::Left, Button::Right, Button::Middle, Button::X1, Button::X2]
        .into_iter().zip(snapshot.physical_buttons)
    {
        if pressed {
            physical_buttons |= button_bit(button);
        }
    }
    HeldSynchronization {
        modifiers, physical_buttons, swapped: snapshot.swapped, caps_lock: snapshot.caps_lock, keys: snapshot.keys,
    }
}
