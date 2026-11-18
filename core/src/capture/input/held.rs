use std::collections::BTreeSet;

use crate::protocol::{DeviceId, Evidence, SessionTimeUs, UnavailableReason, ipc::InputSample};

use super::{
    Action, Button, Correlation, InputOrigin, Key, Modifier, NormalizedInput, PendingInput, ScanCode,
    normalize::{LLKHF_UP, VK_CAPITAL},
};

/// The longest separation between the system's left-Ctrl companion and the
/// right Alt of one AltGr press. Both are hook records; this is not the
/// raw-to-hook matching window, although it has the same initial value.
const ALT_GR_COMPANION_US: i64 = 20_000;

const MAX_HELD: usize = 512;
const MODIFIER_MASK: u8 = 0xff;
const BUTTON_MASK: u8 = 0x1f;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Holding {
    Key { vk: u16, scan: Option<ScanCode>, modifier: Option<Modifier> },
    Button { logical: Button, physical: Option<Button> },
    PenTip { link: u16 },
    PromotedPenPointer { logical: Button, cursor_id: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct HeldIdentity {
    pub device: Option<DeviceId>,
    pub holding: Holding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct HeldEdge {
    pub identity: HeldIdentity,
    pub down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeldSnapshot {
    pub modifiers: u8,
    pub modifiers_known: u8,
    pub buttons: u8,
    pub buttons_known: u8,
    pub caps_lock: Option<bool>,
    pub alt_gr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyntheticHeldEdge {
    pub t_us: SessionTimeUs,
    pub edge: HeldEdge,
    pub reason: HeldBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeldSynchronization {
    pub modifiers: u8,
    pub physical_buttons: u8,
    pub swapped: bool,
    pub caps_lock: bool,
    /// Every virtual key physically down at the resync, as a 256-bit set.
    /// Private state only; ordinary keys get no synthetic edges (stage I4).
    pub keys: [u64; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeldBoundary {
    InputGap,
    DeviceRemoved,
    AdmissionClosed,
    Pause,
    Stop,
    FocusChanged,
    RecorderControl,
    HookUnavailable,
    HidReportUnavailable,
    Synchronization,
}

pub(crate) struct HeldInput {
    pub normalized: NormalizedInput,
    pub held: HeldSnapshot,
    pub edge: Option<HeldEdge>,
    pub repeat: Evidence<bool>,
    pub held_motion: bool,
    pub motion_thinned: bool,
}

impl HeldInput {
    /// The row's first record.
    pub fn first(&self) -> &PendingInput {
        &self.normalized.correlated.first
    }

    /// The receipt time of the row's first record.
    pub fn receipt(&self) -> SessionTimeUs {
        self.first().time.recv_t_us
    }
}

#[derive(Default)]
pub(crate) struct PhysicalHeld {
    held: BTreeSet<HeldIdentity>,
    modifiers_known: u8,
    buttons_known: u8,
    known_released: BTreeSet<HeldIdentity>,
    /// Ordinary keys known up since the last resync (stage I4). A key outside
    /// it, and not released since, is down or unknown across the boundary.
    keys_up: [u64; 4],
    caps_lock: Option<bool>,
    companion_at: Option<SessionTimeUs>,
    right_alt_at: Option<SessionTimeUs>,
    alt_gr: bool,
}

impl PhysicalHeld {
    pub fn observe(&mut self, normalized: NormalizedInput) -> HeldInput {
        let action = normalized.correlated.first.action;
        let device = normalized.origin.device.observed();
        let at = normalized.correlated.first.time.recv_t_us;
        if let Action::AltGrCompanion = action
            && let crate::protocol::ipc::InputSample::HookKey(key) = normalized.correlated.first.packet.sample
        {
            if key.flags & LLKHF_UP == 0 {
                self.companion_at = Some(at);
                self.alt_gr |= self.right_alt_at.is_some_and(|right| at.0.saturating_sub(right.0) <= ALT_GR_COMPANION_US);
            } else {
                self.companion_at = None;
                self.alt_gr = false;
            }
        }
        if let Action::Key(Key { modifier: Some(Modifier::RightAlt), down, .. }) = action {
            if down && self.right_alt_at.is_none() {
                self.right_alt_at = Some(at);
                self.alt_gr = self.companion_at.is_some_and(|ctrl| at.0.saturating_sub(ctrl.0) <= ALT_GR_COMPANION_US);
            } else if !down {
                self.right_alt_at = None;
                self.alt_gr = false;
            }
        }
        let holding = match action {
            Action::Key(Key { virtual_key: vk, scan, modifier, down }) => {
                Some((Holding::Key { vk, scan, modifier }, down))
            }
            Action::Button { logical, down, .. } if normalized.origin.origin == InputOrigin::PenPromoted => {
                match normalized.origin.cursor_id {
                    Evidence::Observed(cursor_id) if !down || normalized.correlated.promotion.is_none() =>
                        Some((Holding::PromotedPenPointer { logical, cursor_id }, down)),
                    _ => None,
                }
            }
            Action::Hid => match &normalized.correlated.first.packet.sample {
                InputSample::Hid(report) => super::pen::tip(report).map(|(link, down)| (Holding::PenTip { link }, down)),
                _ => None,
            },
            Action::Button { logical, physical, down } => {
                let physical = normalized.pointer.as_ref()
                    .and_then(|pointer| pointer.physical_button.observed().or(physical));
                Some((Holding::Button { logical, physical }, down))
            }
            _ => None,
        };
        let edge = holding.map(|(holding, down)| HeldEdge { identity: HeldIdentity { device, holding }, down });
        let uncertain_edge = normalized.correlated.correlation == Correlation::Ambiguous && device.is_none();
        let repeat = match edge {
            Some(edge) if !uncertain_edge => self.apply(edge),
            Some(_) => Evidence::Unavailable(UnavailableReason::Ambiguous),
            None => Evidence::Unavailable(UnavailableReason::Unsupported),
        };
        if let Action::Key(Key { virtual_key: VK_CAPITAL, down: true, .. }) = action {
            if uncertain_edge {
                self.caps_lock = None;
            } else if !matches!(repeat, Evidence::Observed(true)) {
                self.caps_lock = self.caps_lock.map(|on| !on);
            }
        }
        let held = self.snapshot();
        HeldInput {
            normalized, held, edge, repeat,
            held_motion: held.buttons != 0 || self.held.iter().any(|id| matches!(id.holding, Holding::PenTip { .. })),
            motion_thinned: false,
        }
    }

    pub fn snapshot(&self) -> HeldSnapshot {
        let (modifiers, buttons) = masks(&self.held);
        HeldSnapshot {
            modifiers, modifiers_known: self.modifiers_known | modifiers,
            buttons, buttons_known: self.buttons_known | buttons,
            caps_lock: self.caps_lock, alt_gr: self.alt_gr,
        }
    }

    pub fn invalidate(&mut self) {
        self.held.clear();
        self.modifiers_known = 0;
        self.buttons_known = 0;
        self.known_released.clear();
        self.keys_up = [0; 4];
        self.caps_lock = None;
        self.companion_at = None;
        self.right_alt_at = None;
        self.alt_gr = false;
    }

    pub fn invalidate_pen(&mut self, device: DeviceId) {
        let known_non_pen = |hold: &HeldIdentity| {
            hold.device != Some(device) || !matches!(hold.holding, Holding::PenTip { .. })
        };
        self.held.retain(known_non_pen);
        self.known_released.retain(known_non_pen);
    }

    pub fn remove(&mut self, id: DeviceId) {
        self.held.retain(|hold| hold.device.is_some_and(|device| device != id));
        self.known_released.retain(|hold| hold.device != Some(id));
        self.modifiers_known = 0;
        self.buttons_known = 0;
        self.keys_up = [0; 4];
    }

    /// Called only with a desktop/integrity-verified modifier/button snapshot.
    /// Ordinary keys retain any independently observed evidence.
    pub fn synchronize(
        &mut self, synchronization: HeldSynchronization, t_us: SessionTimeUs,
    ) -> Vec<SyntheticHeldEdge> {
        let HeldSynchronization { modifiers, physical_buttons, swapped, caps_lock, keys } = synchronization;
        let prior = self.snapshot();
        self.keys_up = keys.map(|down| !down);
        self.caps_lock = Some(caps_lock);
        self.companion_at = None;
        self.right_alt_at = None;
        self.alt_gr = false;
        self.held.retain(|entry| !matches!(entry.holding,
            Holding::Key { modifier: Some(_), .. } | Holding::Button { .. }));
        self.modifiers_known = MODIFIER_MASK;
        self.buttons_known = BUTTON_MASK;
        let mut edges = Vec::new();
        for modifier in Modifier::ALL {
            let bit = modifier_bit(modifier);
            let identity = HeldIdentity { device: None, holding: Holding::Key {
                vk: modifier.vk(), scan: None, modifier: Some(modifier),
            } };
            self.synchronize_one(identity, modifiers & bit != 0, prior.modifiers & bit != 0, t_us, &mut edges);
        }
        for physical in Button::ALL {
            let logical = physical.mapped(swapped);
            let identity = HeldIdentity { device: None, holding: Holding::Button { logical, physical: Some(physical) } };
            self.synchronize_one(
                identity, physical_buttons & button_bit(physical) != 0, prior.buttons & button_bit(logical) != 0,
                t_us, &mut edges,
            );
        }
        edges
    }

    fn synchronize_one(
        &mut self, identity: HeldIdentity, down: bool, previously_down: bool, t_us: SessionTimeUs,
        edges: &mut Vec<SyntheticHeldEdge>,
    ) {
        if down {
            self.held.insert(identity);
        }
        if down != previously_down {
            edges.push(SyntheticHeldEdge { edge: HeldEdge { identity, down }, t_us, reason: HeldBoundary::Synchronization });
        }
    }

    /// Whether an ordinary key was known up since the resync. It is down from now on.
    fn take_key_up(&mut self, vk: u16) -> bool {
        let Some(word) = self.keys_up.get_mut(usize::from(vk >> 6)) else { return false };
        let bit = 1_u64 << (vk & 63);
        let up = *word & bit != 0;
        *word &= !bit;
        up
    }

    fn apply(&mut self, edge: HeldEdge) -> Evidence<bool> {
        let own = edge.identity;
        let mut matching = self.held.iter().filter(|old| compatible(**old, own)).copied();
        let existing = matching.next();
        let ambiguous = matching.next().is_some();
        if ambiguous {
            return Evidence::Unavailable(UnavailableReason::Ambiguous);
        }
        if edge.down {
            if let Some(old) = existing {
                if old.device.is_none() && own.device.is_some() {
                    self.held.remove(&old);
                    self.held.insert(own);
                }
                return Evidence::Observed(true);
            }
            if self.held.len() >= MAX_HELD {
                self.invalidate();
                return Evidence::Unavailable(UnavailableReason::Blind);
            }
            self.held.insert(own);
            let released = self.known_released.iter().find(|old| compatible(**old, own)).copied();
            let known_up = match own.holding {
                Holding::Key { modifier: Some(modifier), .. } => self.modifiers_known & modifier_bit(modifier) != 0,
                Holding::Key { vk, modifier: None, .. } => self.take_key_up(vk),
                Holding::Button { logical, .. } => self.buttons_known & button_bit(logical) != 0,
                _ => false,
            };
            if let Some(released) = released {
                self.known_released.remove(&released);
            }
            if known_up || released.is_some() {
                Evidence::Observed(false)
            } else {
                Evidence::Uncertain { value: false, reason: UnavailableReason::Blind }
            }
        } else {
            if let Some(old) = existing {
                self.held.remove(&old);
            }
            if self.known_released.len() < MAX_HELD {
                self.known_released.insert(own);
            }
            match own.holding {
                Holding::Key { modifier: Some(modifier), .. } => self.modifiers_known |= modifier_bit(modifier),
                Holding::Button { logical, .. } => self.buttons_known |= button_bit(logical),
                _ => {}
            }
            Evidence::Unavailable(UnavailableReason::Unsupported)
        }
    }
}

#[derive(Default, Clone)]
pub(crate) struct PublishedHeld {
    held: BTreeSet<HeldIdentity>,
}

impl PublishedHeld {
    /// The modifier and button masks of the published holdings.
    pub fn modifiers_and_buttons(&self) -> (u8, u8) {
        masks(&self.held)
    }

    pub fn buttons_down(&self) -> bool {
        self.modifiers_and_buttons().1 != 0
            || self.held.iter().any(|id| matches!(id.holding, Holding::PenTip { .. }))
    }

    pub fn contains(&self, edge: HeldEdge) -> bool {
        self.held.iter().filter(|old| compatible(**old, edge.identity)).take(2).count() == 1
    }

    /// Call after a successful, approved edge publication. Repeats share
    /// the existing published holding.
    pub fn published(&mut self, edge: HeldEdge) {
        if edge.down {
            if self.held.len() < MAX_HELD && !self.held.iter().any(|old| compatible(*old, edge.identity)) {
                self.held.insert(edge.identity);
            }
        } else {
            let mut candidates = self.held.iter().filter(|old| compatible(**old, edge.identity)).copied();
            if let Some(old) = candidates.next() && candidates.next().is_none() {
                self.held.remove(&old);
            }
        }
    }

    pub fn close(
        &mut self, t_us: SessionTimeUs, reason: HeldBoundary, selection: impl Fn(HeldIdentity) -> bool,
    ) -> Vec<SyntheticHeldEdge> {
        let mut closed = Vec::new();
        self.held.retain(|identity| {
            if !selection(*identity) {
                return true;
            }
            closed.push(SyntheticHeldEdge { t_us, edge: HeldEdge { identity: *identity, down: false }, reason });
            false
        });
        closed
    }
}

/// The modifier and logical button masks of `held`.
fn masks<'a>(held: impl IntoIterator<Item = &'a HeldIdentity>) -> (u8, u8) {
    let mut modifiers = 0;
    let mut buttons = 0;
    for identity in held {
        match identity.holding {
            Holding::Key { modifier: Some(modifier), .. } => modifiers |= modifier_bit(modifier),
            Holding::Button { logical, .. } | Holding::PromotedPenPointer { logical, .. } => buttons |= button_bit(logical),
            _ => {}
        }
    }
    (modifiers, buttons)
}

pub(super) fn compatible(left: HeldIdentity, right: HeldIdentity) -> bool {
    if let (Some(left), Some(right)) = (left.device, right.device) && left != right {
        return false;
    }
    match (left.holding, right.holding) {
        (Holding::Key { vk: a, scan: x, .. }, Holding::Key { vk: b, scan: y, .. }) => {
            a == b && match (x, y) { (Some(x), Some(y)) => x == y, _ => true }
        }
        (Holding::PenTip { link: a }, Holding::PenTip { link: b }) => a == b,
        (Holding::PromotedPenPointer { logical: a, cursor_id: ca },
            Holding::PromotedPenPointer { logical: b, cursor_id: cb }) => a == b && ca == cb,
        (Holding::Button { logical: a, physical: x }, Holding::Button { logical: b, physical: y }) => {
            match (x, y) { (Some(x), Some(y)) => x == y, _ => a == b }
        }
        _ => false,
    }
}

pub(crate) fn modifier_bit(modifier: Modifier) -> u8 { 1 << (modifier as u8) }
pub(crate) fn button_bit(button: Button) -> u8 { 1 << (button as u8) }

/// The sides set in a modifier mask: a journal `mods` list (INP-2).
pub(crate) fn modifiers_in(bits: u8) -> Vec<Modifier> {
    Modifier::ALL.into_iter().filter(|modifier| bits & modifier_bit(*modifier) != 0).collect()
}

/// The buttons set in a button mask: a journal `held` list (INP-5).
pub(crate) fn buttons_in(bits: u8) -> Vec<Button> {
    Button::ALL.into_iter().filter(|button| bits & button_bit(*button) != 0).collect()
}
