use std::collections::{BTreeSet, VecDeque};

use crate::{
    protocol::{MessageTickMs, SessionTimeUs, ipc::InputSample},
    session::hotkey::HotkeyActivation,
};

use super::{Action, HeldInput, HeldSynchronization, Modifier, held::{HeldIdentity, Holding, compatible, modifier_bit}};

const ASSOCIATION_US: i64 = 70_000;
const RAW_ONLY_ASSOCIATION_US: u64 = 20_000;
const MAX_CONTROL_WAIT_US: i64 = 200_000;
const PREFIX_WAIT_US: i64 = 120_000;
const COARSE_DISTANCE_MS: u32 = 16;
const MAX_PREFIX: usize = 1_024;
const MAX_NOTICES: usize = 32;
const MAX_CONSUMED: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlDisposition {
    Application,
    Recorder,
    Uncertain,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlBoundary {
    pub at: SessionTimeUs,
    pub modifiers: u8,
}

pub(crate) struct ClassifiedControlInput {
    pub input: HeldInput,
    pub disposition: ControlDisposition,
    pub boundary: Option<ControlBoundary>,
}

struct Staged {
    input: HeldInput,
    disposition: Option<ControlDisposition>,
    boundary: Option<ControlBoundary>,
}

struct Confirmation {
    activation: HotkeyActivation,
    at: SessionTimeUs,
}

pub(crate) struct ControlChords {
    keys: [u16; 4],
    pending: VecDeque<Staged>,
    confirmations: VecDeque<Confirmation>,
    consumed: BTreeSet<HeldIdentity>,
    bound: Option<SessionTimeUs>,
    lost_through: Option<SessionTimeUs>,
    start_chord: bool,
    startup_changes: u8,
}

impl ControlChords {
    pub fn new(keys: [u16; 4]) -> Self {
        Self {
            keys, pending: VecDeque::new(), confirmations: VecDeque::new(), consumed: BTreeSet::new(),
            bound: None, lost_through: None, start_chord: false, startup_changes: 0,
        }
    }

    pub fn note_startup_change(&mut self, modifier: Modifier) {
        self.startup_changes |= modifier_bit(modifier);
    }

    pub fn reopen(&mut self, hotkey: bool) {
        self.startup_changes = 0;
        self.start_chord = hotkey;
    }

    pub fn start_chord(&mut self) {
        self.start_chord = true;
    }

    pub fn omitted_from_sync(&mut self, state: HeldSynchronization) -> u8 {
        let mut omit = 0;
        for consumed in &self.consumed {
            omit |= modifier_of(*consumed).map_or(0, modifier_bit);
        }
        if std::mem::take(&mut self.start_chord) {
            let bootstrap = state.modifiers & chord_modifiers() & !self.startup_changes;
            for modifier in control_modifiers() {
                if bootstrap & modifier_bit(modifier) != 0 {
                    self.consume_modifier(modifier);
                }
            }
            omit |= bootstrap;
        }
        omit
    }

    pub fn confirm(&mut self, activation: HotkeyActivation, at: SessionTimeUs) {
        if self.confirmations.len() == MAX_NOTICES {
            self.note_loss(at);
            return;
        }
        self.confirmations.push_back(Confirmation { activation, at });
    }

    pub fn note_loss(&mut self, through: SessionTimeUs) {
        self.lost_through = Some(self.lost_through.map_or(through, |old| old.max(through)));
    }

    pub fn progress(&mut self, bound: SessionTimeUs) {
        self.bound = Some(self.bound.map_or(bound, |old| old.max(bound)));
    }

    pub fn local_before(&self) -> Option<SessionTimeUs> {
        let native = self.bound?;
        [Some(native), self.pending.front().map(|slot| time(&slot.input)),
            self.confirmations.front().map(|notice| notice.at)].into_iter().flatten().min()
    }

    pub fn stage(&mut self, input: HeldInput) -> Result<(), HeldInput> {
        if self.pending.len() == MAX_PREFIX {
            self.note_loss(time(&input));
            return Err(input);
        }
        self.pending.push_back(Staged { input, disposition: None, boundary: None });
        Ok(())
    }

    pub fn discard_from(&mut self, at: SessionTimeUs) -> impl Iterator<Item = HeldInput> + '_ {
        let first = self.pending.iter().position(|slot| time(&slot.input) >= at).unwrap_or(self.pending.len());
        self.pending.drain(first..).map(|slot| slot.input)
    }

    pub fn close_interval(&mut self, through: SessionTimeUs) {
        self.note_loss(through);
        self.consumed.clear();
        self.confirmations.retain(|confirmation| confirmation.at > through);
    }

    pub fn release(
        &mut self, input_before: Option<SessionTimeUs>, now: SessionTimeUs, closing: bool,
    ) -> Vec<ClassifiedControlInput> {
        self.associate(input_before, closing);
        let mut ready = Vec::new();
        while let Some(front) = self.pending.front() {
            let Some(disposition) = self.decision(front, input_before, now, closing) else { break };
            let Some(slot) = self.pending.pop_front() else { break };
            if disposition == ControlDisposition::Recorder {
                if let Some(boundary) = slot.boundary {
                    for modifier in control_modifiers() {
                        if boundary.modifiers & modifier_bit(modifier) != 0 {
                            self.consume_modifier(modifier);
                        }
                    }
                }
                self.consume_edge(&slot.input);
            }
            ready.push(ClassifiedControlInput { input: slot.input, disposition, boundary: slot.boundary });
        }
        ready
    }

    fn decision(
        &self, front: &Staged, input_before: Option<SessionTimeUs>, now: SessionTimeUs, closing: bool,
    ) -> Option<ControlDisposition> {
        if let Some(disposition) = front.disposition {
            return Some(disposition);
        }
        if front.input.edge.is_some_and(|edge| self.consumed.iter().any(|held| compatible(*held, edge.identity))) {
            return Some(ControlDisposition::Recorder);
        }
        let Action::Key(key) = front.input.normalized.correlated.first.action else {
            return Some(ControlDisposition::Application);
        };
        let modifier_prefix = key.modifier.is_some_and(is_chord_modifier);
        if !modifier_prefix && !self.keys.contains(&key.virtual_key) {
            return Some(ControlDisposition::Application);
        }
        let at = time(&front.input);
        let wait = if modifier_prefix { PREFIX_WAIT_US } else { ASSOCIATION_US };
        let until = SessionTimeUs(at.0.saturating_add(wait));
        let timed_out = now.0.saturating_sub(at.0) >= MAX_CONTROL_WAIT_US;
        let decidable = input_before.is_some_and(|bound| bound > until) && self.bound.is_some_and(|bound| bound > until);
        if self.lost_through.is_some_and(|loss| at.0.saturating_sub(ASSOCIATION_US) <= loss.0)
            && (decidable || timed_out || closing)
        {
            return Some(ControlDisposition::Uncertain);
        }
        let awaiting_confirmation = self.confirmations.iter().any(|notice| {
            eligible(&front.input, notice) || modifier_prefix
                && self.pending.iter().skip(1).take_while(|slot| !ordinary_use(&slot.input, &self.keys))
                    .any(|slot| eligible(&slot.input, notice))
        });
        if awaiting_confirmation {
            return (timed_out || closing).then_some(ControlDisposition::Uncertain);
        }
        if modifier_prefix {
            for slot in self.pending.iter().skip(1) {
                if recorder_key_down(&slot.input, &self.keys) {
                    break;
                }
                if ordinary_use(&slot.input, &self.keys) {
                    return Some(ControlDisposition::Application);
                }
            }
        }
        if decidable {
            return Some(ControlDisposition::Application);
        }
        if timed_out || closing {
            return Some(ControlDisposition::Uncertain);
        }
        None
    }

    fn associate(&mut self, input_before: Option<SessionTimeUs>, closing: bool) {
        while let Some(confirmation) = self.confirmations.front() {
            if closing && input_before.is_some_and(|cutoff| confirmation.at > cutoff) {
                break;
            }
            let end = SessionTimeUs(confirmation.at.0.saturating_add(ASSOCIATION_US));
            if !closing && input_before.is_none_or(|bound| bound <= end) {
                break;
            }
            let Some(confirmation) = self.confirmations.pop_front() else { break };
            let candidates: Vec<_> = self.pending.iter().enumerate().filter_map(|(index, slot)| {
                (slot.disposition.is_none() && eligible(&slot.input, &confirmation)).then_some(index)
            }).collect();
            let lost = self.lost_through.is_some_and(|loss| confirmation.at.0.saturating_sub(ASSOCIATION_US) <= loss.0);
            if lost || candidates.len() != 1 {
                for index in candidates {
                    self.pending[index].disposition = Some(ControlDisposition::Uncertain);
                }
                continue;
            }
            let key_index = candidates[0];
            let modifiers = definite_modifiers(self.pending[key_index].input.held.modifiers);
            self.pending[key_index].disposition = Some(ControlDisposition::Recorder);
            self.pending[key_index].boundary = Some(ControlBoundary { at: confirmation.at, modifiers });
            // Only the current modifier holdings before the triggering down
            // can be exclusively control-owned. Ordinary use stops the search.
            let mut open = modifiers;
            for slot in self.pending.range_mut(..key_index).rev() {
                if ordinary_use(&slot.input, &self.keys) {
                    break;
                }
                if let Some(edge) = slot.input.edge && let Some(modifier) = modifier_of(edge.identity) {
                    let bit = modifier_bit(modifier);
                    if open & bit != 0 {
                        if edge.down && slot.disposition.is_none() {
                            slot.disposition = Some(ControlDisposition::Recorder);
                        }
                        open &= !bit;
                    }
                }
            }
        }
    }

    fn consume_modifier(&mut self, modifier: Modifier) {
        if self.consumed.len() >= MAX_CONSUMED
            || self.consumed.iter().any(|held| modifier_of(*held) == Some(modifier))
        {
            return;
        }
        self.consumed.insert(HeldIdentity { device: None, holding: Holding::Key {
            vk: super::modifier_vk(modifier), scan: None, modifier: Some(modifier),
        } });
    }

    fn consume_edge(&mut self, input: &HeldInput) {
        let Some(edge) = input.edge else { return };
        if !edge.down {
            let mut eligible = self.consumed.iter().filter(|held| compatible(**held, edge.identity)).copied();
            if let Some(held) = eligible.next() && eligible.next().is_none() {
                self.consumed.remove(&held);
            }
        } else if self.consumed.len() < MAX_CONSUMED
            && !self.consumed.iter().any(|held| compatible(*held, edge.identity))
        {
            self.consumed.insert(edge.identity);
        }
    }
}

fn time(input: &HeldInput) -> SessionTimeUs {
    input.normalized.correlated.first.time.recv_t_us
}

fn eligible(input: &HeldInput, confirmation: &Confirmation) -> bool {
    let correlated = &input.normalized.correlated;
    let Action::Key(key) = correlated.first.action else { return false };
    if !key.down || key.virtual_key != confirmation.activation.virtual_key {
        return false;
    }
    let distance = time(input).0.abs_diff(confirmation.at.0);
    if distance > ASSOCIATION_US as u64 {
        return false;
    }
    let hook_tick = std::iter::once(&correlated.first).chain(&correlated.partners).find_map(|slot| {
        matches!(slot.packet.sample, InputSample::HookKey(_)).then_some(slot.packet.receipt.message_tick_ms).flatten()
    });
    let tick = hook_tick.or(correlated.first.packet.receipt.message_tick_ms);
    tick.map_or(distance <= RAW_ONLY_ASSOCIATION_US, |tick| {
        tick_distance(tick, confirmation.activation.message_tick) <= COARSE_DISTANCE_MS
    })
}

fn tick_distance(left: MessageTickMs, right: MessageTickMs) -> u32 {
    (left.0.wrapping_sub(right.0) as i32).unsigned_abs()
}

fn recorder_key_down(input: &HeldInput, keys: &[u16; 4]) -> bool {
    matches!(input.normalized.correlated.first.action, Action::Key(key) if key.down && keys.contains(&key.virtual_key))
}

fn ordinary_use(input: &HeldInput, keys: &[u16; 4]) -> bool {
    match input.normalized.correlated.first.action {
        Action::Key(key) => key.down && key.modifier.is_none() && !keys.contains(&key.virtual_key),
        Action::Button { down, .. } => down,
        Action::Wheel { .. } => true,
        _ => false,
    }
}

pub(crate) fn modifier_of(identity: HeldIdentity) -> Option<Modifier> {
    match identity.holding {
        Holding::Key { modifier, .. } => modifier,
        _ => None,
    }
}

fn is_chord_modifier(modifier: Modifier) -> bool { control_modifiers().contains(&modifier) }

fn control_modifiers() -> [Modifier; 4] {
    [Modifier::LeftCtrl, Modifier::RightCtrl, Modifier::LeftAlt, Modifier::RightAlt]
}

fn chord_modifiers() -> u8 {
    control_modifiers().into_iter().fold(0, |mask, modifier| mask | modifier_bit(modifier))
}

fn definite_modifiers(held: u8) -> u8 {
    let ctrl = held & (modifier_bit(Modifier::LeftCtrl) | modifier_bit(Modifier::RightCtrl));
    let alt = held & (modifier_bit(Modifier::LeftAlt) | modifier_bit(Modifier::RightAlt));
    let ctrl = if ctrl.count_ones() == 1 { ctrl } else { 0 };
    let alt = if alt.count_ones() == 1 { alt } else { 0 };
    ctrl | alt
}
