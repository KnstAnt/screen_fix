//! Ordered input publication. Windows approval and focus are evaluated
//! against the private receipt proof before any per-input payload is built.

use std::{collections::{BTreeSet, VecDeque}, sync::mpsc};

use serde_json::json;

use crate::{
    capture::{
        DropReason,
        context::TextTarget,
        observation::ProvisionalInputKind,
        input::{
            Action, ClassifiedControlInput, ControlDisposition, Correlation, HeldBoundary, HeldIdentity, SyntheticHeldEdge,
            HeldInput, InputOrigin, InputResolution, Key, Modifier, PublishedHeld, has_promotion, modifier_bit, modifier_of,
        },
    },
    package::journal::{JournalIngress, Submit},
    protocol::{AdmissionEpoch, DeviceId, EventRef, Evidence, FocusGeneration, GeometryId, ObservationId, SessionTimeUs, UnavailableReason, ipc::{DeviceDescriptor, DeviceKind, ForegroundAtReceipt, InputSample, MAX_INPUT_DEVICES, Producer, elevated::VisitBinding}},
    session::windows::Receipt,
};

use super::super::{ProbeError, clock, context::{ConfirmedText, Context, ImeState, ReceiptFocus, Visit}, package_journal_error};
use super::{ReceiptPrefix, hid::HidPublication, receiver::ProvisionalRequests, text::{KeyboardText, TextCommit, TextOrigin, TextStatus, Typing}};

const MAX_SLOTS: usize = 1024;
const MAX_CONFIRMATIONS: usize = 256;
const FOCUS_CONFIRM_US: i64 = 100_000;
const MAX_WAIT_US: i64 = 500_000;

pub(super) struct Gate {
    slots: VecDeque<Slot>,
    in_flight: VecDeque<Confirmation>,
    held: PublishedHeld,
    modifier_knowledge: ModifierKnowledge,
    hid: HidPublication,
    pen_observed: bool,
    pressure_observed: bool,
    reference_key: Option<[u8; 32]>,
    touchpads: BTreeSet<DeviceId>,
    touchpad_inventory_uncertain: bool,
    redacted: Option<RedactedVisit>,
    completed_redactions: VecDeque<RedactedVisit>,
    motion: Option<MotionCounts>,
    overflow_at: Option<SessionTimeUs>,
    overflow: bool,
    active_key_visit: Option<VisitKey>,
    keyboard_text: KeyboardText,
    last_text_key: Option<(VisitKey, SessionTimeUs)>,
    last_pointer_move: Option<(VisitKey, SessionTimeUs)>,
    confirmed_text: VecDeque<(SessionTimeUs, ConfirmedText)>,
}

enum Slot {
    Input(ClassifiedControlInput),
    Synchronization {
        at: SessionTimeUs, visit: Visit, held_modifiers: u8, released_modifiers: u8, edges: Vec<SyntheticHeldEdge>,
    },
    Boundary { at: SessionTimeUs, kind: HeldBoundary, selection: Selection },
}

#[derive(Clone, Copy)]
struct ModifierKnowledge {
    known: u8,
    // A second device may still hold a side after an approved device releases it.
    possibly_hidden: u8,
    awaiting_confirmation: u8,
}

impl ModifierKnowledge {
    fn from_released(released: u8) -> Self {
        Self { known: released, possibly_hidden: !released, awaiting_confirmation: 0 }
    }

    fn conceal(&mut self, bits: u8) {
        self.known &= !bits;
        self.possibly_hidden |= bits;
        self.awaiting_confirmation &= !bits;
    }

    fn after(mut self, input: &HeldInput, key: Key) -> Self {
        self.known &= input.held.modifiers_known;
        self.awaiting_confirmation &= input.held.modifiers;
        if let Some(modifier) = key.modifier {
            let bit = modifier_bit(modifier);
            self.awaiting_confirmation &= !bit;
            let held = input.held.modifiers & bit != 0;
            if input.held.modifiers_known & bit == 0
                || matches!(input.repeat, Evidence::Unavailable(UnavailableReason::Ambiguous | UnavailableReason::Blind))
            {
                self.conceal(bit);
            } else if key.down && held {
                self.known |= bit;
            } else if !key.down && !held {
                self.known |= bit;
                self.possibly_hidden &= !bit;
            } else if !key.down && self.possibly_hidden & bit != 0 {
                self.known &= !bit;
            }
        } else {
            self.conceal(modifier_bits(key));
        }
        self
    }
}

fn modifier_bits(key: Key) -> u8 {
    match (key.modifier, key.virtual_key) {
        (Some(modifier), _) => modifier_bit(modifier),
        (None, 0x10) => modifier_bit(Modifier::LeftShift) | modifier_bit(Modifier::RightShift),
        (None, 0x11) => modifier_bit(Modifier::LeftCtrl) | modifier_bit(Modifier::RightCtrl),
        (None, 0x12) => modifier_bit(Modifier::LeftAlt) | modifier_bit(Modifier::RightAlt),
        _ => 0,
    }
}

#[derive(Clone, Copy)]
pub(super) enum Selection {
    All,
    Removed(crate::protocol::DeviceId),
    PenDevice(crate::protocol::DeviceId),
    Modifiers(u8),
}

impl Selection {
    fn contains(self, held: HeldIdentity) -> bool {
        match self {
            Self::All => true,
            Self::Removed(id) => held.device.is_none_or(|device| device == id),
            Self::PenDevice(id) => held.device == Some(id)
                && matches!(held.holding, crate::capture::input::Holding::PenTip { .. }),
            Self::Modifiers(bits) => modifier_of(held).is_some_and(|modifier| bits & modifier_bit(modifier) != 0),
        }
    }
}

struct Confirmation {
    observation: Option<ObservationId>,
    geometry: Evidence<GeometryId>,
    kind: ProvisionalInputKind,
    admission: AdmissionEpoch,
    proof: ForegroundAtReceipt,
    device: Option<DeviceId>,
    at: SessionTimeUs,
    event: mpsc::Receiver<EventRef>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct VisitKey {
    proof: ForegroundAtReceipt,
    generation: Option<FocusGeneration>,
}

struct RedactedVisit {
    start: Option<SessionTimeUs>,
    end: Option<SessionTimeUs>,
    count: u64,
}

#[derive(Default)]
struct MotionCounts {
    second: i64,
    coalesced: u64,
    thinned: u64,
}

impl Gate {
    pub fn new() -> Self {
        Self {
            slots: VecDeque::with_capacity(MAX_SLOTS), in_flight: VecDeque::with_capacity(MAX_CONFIRMATIONS),
            hid: HidPublication::default(), pen_observed: false, pressure_observed: false,
            reference_key: reference_key(),
            touchpads: BTreeSet::new(), touchpad_inventory_uncertain: false,
            held: PublishedHeld::default(), modifier_knowledge: ModifierKnowledge::from_released(0),
            redacted: None, completed_redactions: VecDeque::with_capacity(64),
            motion: None, overflow_at: None, overflow: false, active_key_visit: None,
            keyboard_text: KeyboardText::default(), last_text_key: None, last_pointer_move: None, confirmed_text: VecDeque::with_capacity(16),
        }
    }

    pub fn push(&mut self, input: ClassifiedControlInput, requests: &mut ProvisionalRequests) {
        if self.slots.len() >= MAX_SLOTS || self.overflow_at.is_some() {
            self.overflow = true;
            self.overflow_at.get_or_insert(input.input.normalized.correlated.first.time.recv_t_us);
            requests.retire_input(input.input, InputResolution::Dropped(DropReason::Capacity));
        } else {
            self.slots.push_back(Slot::Input(input));
        }
    }

    pub fn boundary(&mut self, at: SessionTimeUs, kind: HeldBoundary, selection: Selection) {
        if !is_closing(kind) && (self.slots.len() >= MAX_SLOTS || self.overflow_at.is_some()) {
            self.overflow = true;
            self.overflow_at = Some(self.overflow_at.map_or(at, |first| first.min(at)));
        } else {
            self.slots.push_back(Slot::Boundary { at, kind, selection });
        }
    }

    pub fn retain_prefix(
        &mut self, before: SessionTimeUs, hook_before: Option<crate::protocol::QpcTick>, requests: &mut ProvisionalRequests,
    ) {
        for _ in 0..self.slots.len() {
            let Some(slot) = self.slots.pop_front() else { break; };
            match slot {
                Slot::Input(input) => {
                    let correlated = &input.input.normalized.correlated;
                    let promotion_covered = hook_before.is_none_or(|before| correlated.promotion.as_ref()
                        .is_none_or(|promotion| promotion.pen.receipt.qpc < before && promotion.pointer.receipt.qpc < before));
                    if promotion_covered && std::iter::once(&correlated.first).chain(&correlated.partners)
                        .all(|receipt| receipt.time.recv_t_us < before)
                    {
                        self.slots.push_back(Slot::Input(input));
                    } else {
                        requests.retire_input(input.input, InputResolution::Dropped(DropReason::AdmissionClosed));
                    }
                }
                Slot::Synchronization { at, edges, .. } if at >= before => requests.suppress_synthetic(edges),
                Slot::Boundary { at, .. } if at >= before => {}
                slot => self.slots.push_back(slot),
            }
        }
        self.confirmed_text.retain(|(at, _)| *at < before);
    }

    pub fn synchronize(
        &mut self, at: SessionTimeUs, visit: Visit, held_modifiers: u8, released_modifiers: u8,
        edges: Vec<SyntheticHeldEdge>,
    ) {
        if self.slots.len() >= MAX_SLOTS || self.overflow_at.is_some() {
            self.overflow = true;
            self.overflow_at.get_or_insert(at);
            return;
        }
        self.slots.push_back(Slot::Synchronization { at, visit, held_modifiers, released_modifiers, edges });
    }

    pub fn note_input_device(&mut self, descriptor: &DeviceDescriptor) {
        if matches!(descriptor.kind, DeviceKind::Hid { usage_page: 0x0D, usage: 5 }) {
            if self.touchpads.len() == MAX_INPUT_DEVICES && !self.touchpads.contains(&descriptor.id) {
                self.touchpad_inventory_uncertain = true;
            } else {
                self.touchpads.insert(descriptor.id);
            }
        }
    }

    pub fn forget_input_device(&mut self, device: DeviceId) {
        self.touchpads.remove(&device);
        self.hid.reset(Some(device));
    }

    pub fn note_device_inventory_loss(&mut self) {
        self.touchpad_inventory_uncertain = true;
    }

    pub fn reset_device_inventory(&mut self) {
        self.touchpads.clear();
        self.touchpad_inventory_uncertain = false;
        self.hid.reset(None);
    }

    pub fn pen_observation(&self) -> (bool, bool) {
        (self.pen_observed, self.pressure_observed)
    }

    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    fn first_slot(&self) -> Option<SessionTimeUs> {
        self.slots.front().map(|slot| match slot {
            Slot::Input(input) => input.input.normalized.correlated.first.time.recv_t_us,
            Slot::Boundary { at, .. } | Slot::Synchronization { at, .. } => *at,
        })
    }

    pub fn unresolved_before(&self) -> Option<SessionTimeUs> {
        [self.first_slot(), self.overflow_at, self.confirmed_text.front().map(|(at, _)| *at)].into_iter().flatten().min()
    }

    pub fn stage_confirmed(&mut self, text: ConfirmedText, receipt: Receipt) {
        if text.callback_at < receipt.boundary.qpc { return; }
        if self.confirmed_text.len() >= 16 {
            self.overflow = true;
            return;
        }
        if let Ok(at) = receipt.clock.time(text.observed_at) {
            self.confirmed_text.push_back((at, text));
        }
    }

    pub fn drain(
        &mut self, context: &mut Context<'_>, output: &mut JournalIngress, prefix: &mut ReceiptPrefix,
        requests: &mut ProvisionalRequests, receipt: Receipt, through: SessionTimeUs,
        certified_before: Option<SessionTimeUs>, final_boundary: bool,
    ) -> Result<(), ProbeError> {
        self.resolve_confirmed(requests);
        let observed = if context.visit().is_some_and(Visit::is_elevated) {
            receipt.clock.time(receipt.qpc).map_err(super::clock_error)?.max(through)
        } else {
            through
        };
        loop {
            if let Some((at, _)) = self.confirmed_text.front()
                && self.first_slot().is_none_or(|first| *at < first)
                && self.overflow_at.is_none_or(|overflow| *at < overflow)
            {
                if self.confirmed_text.front().is_some_and(|(_, text)| text.source_read.is_some())
                    && let Some(last) = prefix.last_time.filter(|last| *last > *at)
                {
                    let (at, _) = self.confirmed_text.pop_front().expect("confirmed text was present");
                    output.record_loss_at(1, at, at, last).map_err(package_journal_error)?;
                    continue;
                }
                let elapsed = observed.0.saturating_sub(at.0);
                if !final_boundary && elapsed < MAX_WAIT_US
                    && (elapsed < FOCUS_CONFIRM_US || certified_before.is_none_or(|prefix| prefix <= *at))
                {
                    break;
                }
                let (at, text) = self.confirmed_text.pop_front().expect("confirmed text was present");
                self.flush_motion(at, false, receipt, output, prefix)?;
                let callback_fresh = text.source_read.is_none() || receipt.clock.time(text.callback_at)
                    .is_ok_and(|callback| observed.0.saturating_sub(callback.0) <= MAX_WAIT_US);
                if at <= through && elapsed <= MAX_WAIT_US && callback_fresh
                    && certified_before.is_some_and(|prefix| prefix > at) && context.confirm_text(&text)
                {
                    let source = receipt.clock.time(text.callback_at).ok();
                    let generation = Some(text.visit.generation);
                    let commit = TextCommit {
                        text: text.text, provenance: TextOrigin::TextEditCompositionFinalized, status: TextStatus::Confirmed,
                    };
                    self.flush_redactions(at, receipt, output, prefix)?;
                    Self::publish_text(at, receipt, generation, source, commit, output, prefix)?;
                }
                continue;
            }
            let Some(at) = self.first_slot().filter(|at| *at <= through) else { break };
            self.flush_motion(at, false, receipt, output, prefix)?;
            let mut approval = None;
            let mut ime = ImeState::Unknown;
            if let Some(Slot::Input(classified)) = self.slots.front()
                && classified.disposition == ControlDisposition::Application
            {
                let correlated = &classified.input.normalized.correlated;
                approval = approved_receipt(&classified.input, context);
                if let Some((proof, focus)) = approval && let Action::Key(key) = correlated.first.action {
                    let at = correlated.first.time.recv_t_us;
                    let elapsed = observed.0.saturating_sub(at.0);
                    if elapsed < FOCUS_CONFIRM_US || matches!(focus, ReceiptFocus::Pending(_)) && elapsed < MAX_WAIT_US {
                        if !final_boundary {
                            break;
                        }
                        approval = Some((proof, focus.visit().map_or(ReceiptFocus::Unknown, ReceiptFocus::Historical)));
                    }
                    let deadline = SessionTimeUs(at.0.saturating_add(MAX_WAIT_US));
                    if key.down && key.virtual_key != 0xe7 && focus.target(receipt.clock, deadline) == TextTarget::Editable
                        && let Some(visit) = focus.visit()
                    {
                        if !final_boundary && elapsed < MAX_WAIT_US {
                            let Some(state) = context.ime_for_key(visit, correlated.first.id,
                                correlated.first.packet.receipt.qpc, std::time::Duration::from_micros(MAX_WAIT_US as u64))
                            else { break };
                            ime = state;
                        }
                    }
                }
            }
            match self.slots.pop_front().expect("gate has a front slot") {
                Slot::Boundary { at, kind, selection } => {
                    if is_closing(kind) {
                        self.finish_redaction(Some(at));
                        self.flush_redactions(at, receipt, output, prefix)?;
                    }
                    self.close(at, kind, selection, receipt, output, prefix)?;
                }
                Slot::Synchronization { at, visit, held_modifiers, released_modifiers, edges } => {
                    let mut witnessed = self.modifier_knowledge.awaiting_confirmation;
                    self.modifier_knowledge = ModifierKnowledge::from_released(0);
                    if context.confirm_focus(visit) {
                        let deadline = SessionTimeUs(at.0.saturating_add(MAX_WAIT_US));
                        let safe_target = context.approve_receipt(visit.receipt_proof(), visit.started_at, visit.started_at)
                            .is_some_and(|focus| matches!(focus.target(receipt.clock, deadline), TextTarget::Editable | TextTarget::NonText));
                        if safe_target {
                            self.modifier_knowledge = ModifierKnowledge::from_released(released_modifiers);
                        }
                        let mut rows = Vec::with_capacity(edges.len() + 1);
                        rows.push(json!({ "kind": "held_state_sync", "synthetic": true,
                            "focus_generation": visit.generation, "admission_epoch": receipt.boundary.epoch }));
                        for edge in edges {
                            if !safe_target
                                && matches!(edge.edge.identity.holding, crate::capture::input::Holding::Key { .. })
                            {
                                continue;
                            }
                            if edge.edge.down && let Some(modifier) = modifier_of(edge.edge.identity) {
                                witnessed |= modifier_bit(modifier);
                            }
                            rows.push(held_row(edge, receipt));
                            self.held.published(edge.edge);
                        }
                        if safe_target {
                            self.modifier_knowledge.known |= witnessed & held_modifiers & self.held.modifiers_and_buttons().0;
                        }
                        let count = rows.len();
                        output.submit_boundary(at, rows).map_err(package_journal_error)?;
                        for _ in 0..count { prefix.advance_time(at, Submit::Queued)?; }
                    }
                }
                Slot::Input(classified) => {
                    if let Some((proof, focus)) = approval {
                        self.publish(classified.input, proof, focus, ime, context, output, prefix, requests, receipt)?;
                    } else {
                        if matches!(classified.input.normalized.correlated.first.action, Action::Key(_)) {
                            // Without an approved classification, even the affected side is private.
                            self.modifier_knowledge.conceal(u8::MAX);
                        }
                        if let InputSample::Hid(sample) = &classified.input.normalized.correlated.first.packet.sample {
                            self.hid.reset(Some(sample.device));
                        }
                        requests.retire_input(classified.input, InputResolution::Suppressed);
                    }
                }
            }
        }
        if self.slots.is_empty() && self.overflow_at.is_some_and(|at| at <= through)
            && let Some(at) = self.overflow_at.take() {
            self.close(at, HeldBoundary::InputGap, Selection::All, receipt, output, prefix)?;
        }
        if final_boundary || self.active_key_visit.is_some_and(|key| key.generation.is_some() && !context.is_current(key.proof, key.generation)) {
            let completion = if final_boundary { Some(through) } else {
                certified_before.map(|bound| bound.min(through)).filter(|at| self.slots.is_empty()
                    && context.focus_boundary().and_then(|qpc| receipt.clock.time(qpc).ok())
                        .is_some_and(|boundary| boundary < *at))
            };
            if let Some(through) = completion {
                let end = self.active_key_visit.and_then(|visit| visit.generation)
                    .and_then(|generation| context.focus_end(generation, receipt.clock));
                if final_boundary || end.is_some_and(|at| at <= through) {
                    self.finish_redaction(end.or(final_boundary.then_some(through)));
                    let publish_at = end.map_or(through, |at| at.max(prefix.last_time.unwrap_or(receipt.boundary.time)));
                    self.flush_redactions(publish_at, receipt, output, prefix)?;
                    self.close(publish_at, HeldBoundary::FocusChanged, Selection::All, receipt, output, prefix)?;
                }
            }
        }
        if let Some(bound) = certified_before.or(final_boundary.then_some(through)) {
            let available = self.unresolved_before().map_or(bound, |pending| bound.min(pending)).min(through);
            self.flush_motion(available, final_boundary, receipt, output, prefix)?;
        }
        self.resolve_confirmed(requests);
        context.note_held_buttons(self.held.buttons_down());
        Ok(())
    }

    fn publish(
        &mut self, input: HeldInput, proof: ForegroundAtReceipt, focus: ReceiptFocus, ime: ImeState, context: &mut Context<'_>,
        output: &mut JournalIngress, prefix: &mut ReceiptPrefix, requests: &mut ProvisionalRequests,
        receipt: Receipt,
    ) -> Result<(), ProbeError> {
        let normal = &input.normalized;
        let first = &normal.correlated.first;
        let at = first.time.recv_t_us;
        let qpc = first.packet.receipt.qpc;
        let visit = VisitKey { proof, generation: focus.generation() };
        let mut permit_observation = true;
        let mut commits = Vec::new();
        let mut source_time = first.time.source_t_us;
        let mut modifier_knowledge = None;
        let mut active_pad_contact = false;
        let report_scope = focus.visit().and_then(Visit::elevated_binding);
        let report_reference = matches!(&first.packet.sample, InputSample::Hid(_))
            .then(|| self.hid_reference(&first.packet, report_scope)).flatten();
        let approved_promotion = normal.correlated.promotion.as_ref().filter(|link| {
            link.pen.foreground_at_receipt == Some(proof) && promotion_focus(link, proof, context)
        });
        let touchpad = if !self.touchpad_inventory_uncertain && self.touchpads.len() == 1
            && clean_null_pointer(normal)
        {
            self.touchpads.first().and_then(|&device| {
                let contact = self.hid.overlapping_contact(device, at, proof)?;
                Some((device, json!({
                    "report_reference": contact.reference,
                    "receipt_gap_us": at.0.checked_sub(contact.at.0)?,
                })))
            })
        } else {
            None
        };
        let mut pointer = normal.pointer;
        if let Some((device, _)) = &touchpad && let Some(details) = &mut pointer {
            details.points.origin = InputOrigin::Touchpad;
            details.points.device = Evidence::Observed(*device);
        }
        let row = match first.action {
            Action::Key(key) => {
                let deadline = SessionTimeUs(at.0.saturating_add(MAX_WAIT_US));
                let target = focus.target(receipt.clock, deadline);
                let target = if matches!(target, TextTarget::Editable | TextTarget::NonText)
                    && !focus.visit().is_some_and(|visit| context.confirm_focus(visit))
                {
                    TextTarget::PossiblySensitive
                } else {
                    target
                };
                if let Some(old) = self.active_key_visit.filter(|old| *old != visit) {
                    let end = old.generation.and_then(|generation| context.focus_end(generation, receipt.clock));
                    self.finish_redaction(end);
                    let public_at = end.filter(|end| *end <= at).unwrap_or(receipt.boundary.time)
                        .max(prefix.last_time.unwrap_or(receipt.boundary.time));
                    self.close(public_at, HeldBoundary::FocusChanged, Selection::All, receipt, output, prefix)?;
                }
                self.active_key_visit = Some(visit);
                if matches!(target, TextTarget::Password | TextTarget::PossiblySensitive) {
                    self.modifier_knowledge.conceal(u8::MAX);
                    self.keyboard_text.reset();
                    if key.down {
                        let start = focus.visit().and_then(|visit| receipt.clock.time(visit.started_at).ok());
                        let visit = self.redacted.get_or_insert(RedactedVisit { start, end: None, count: 0 });
                        visit.count = visit.count.saturating_add(1);
                    }
                    requests.retire_input(input, InputResolution::Suppressed);
                    return Ok(());
                }
                let published_press = input.edge.is_some_and(|edge| self.held.contains(edge));
                if !key.down && !published_press {
                    self.conceal_modifier(&input);
                    requests.retire_input(input, InputResolution::Suppressed);
                    return Ok(());
                }
                self.finish_redaction(None);
                let typing = if target == TextTarget::Editable {
                    if let Some(layout) = proof.focus.map(|focus| focus.keyboard_layout) {
                        let text = self.keyboard_text.key(&input, key, layout, ime);
                        commits = text.commits;
                        text.typing
                    } else {
                        Typing::Unknown
                    }
                } else {
                    Typing::NonText
                };
                permit_observation = if target == TextTarget::Editable && key.down {
                    let first_in_burst = self.last_text_key.replace((visit, at)).is_none_or(|(old, last)| {
                        old != visit || at.0.saturating_sub(last.0) >= 500_000
                    });
                    !published_press && first_in_burst
                } else {
                    if key.down { target == TextTarget::NonText && !published_press } else { published_press }
                };
                let (raw_input, keyboard_hook) = normal.correlated.keyboard_witnesses();
                let mut raw_input = json!(raw_input);
                let mut keyboard_hook = json!(keyboard_hook);
                if key.virtual_key == 0xe7 {
                    // VK_PACKET carries a UTF-16 unit in the scan field. Text
                    // evidence belongs to the editable-only text_commit path.
                    if raw_input.is_object() {
                        raw_input["make_code"] = serde_json::Value::Null;
                    }
                    if keyboard_hook.is_object() {
                        keyboard_hook["scan_code"] = serde_json::Value::Null;
                    }
                }
                let knowledge = self.modifier_knowledge.after(&input, key);
                modifier_knowledge = Some(knowledge);
                json!({
                    "kind": if key.down { "key_down" } else { "key_up" },
                    "vk": key.virtual_key, "scan": key.scan, "key": normal.key_name,
                    "raw_input": raw_input, "keyboard_hook": keyboard_hook,
                    "modifiers": input.held.modifiers & knowledge.known, "modifiers_known": knowledge.known,
                    "repeat": key.down.then_some(published_press), "typing": typing,
                    "ime": (target == TextTarget::Editable && key.down && key.virtual_key != 0xe7).then_some(ime),
                    "layout": proof.focus.map(|focus| focus.keyboard_layout), "focus_generation": visit.generation,
                })
            }
            Action::Button { logical, down, .. } => {
                let mut element = None;
                let points = pointer.as_ref().map(|pointer| pointer.points);
                if down && context.is_current(proof, focus.generation()) && let Some(points) = points
                    && let (Evidence::Observed(point), Evidence::Observed(geometry)) = (points.event_position, points.geometry)
                {
                    element = context.pointer_query(point, geometry, receipt.clock, receipt.qpc, Some(qpc))?;
                }
                let promoted = normal.origin.origin == InputOrigin::PenPromoted;
                let published = input.edge.is_some_and(|edge| self.held.contains(edge));
                permit_observation = approved_promotion.is_none() && input.edge.is_some() && down != published;
                json!({ "kind": if down { "pointer_down" } else { "pointer_up" }, "button": logical,
                    "pen_promotion": promoted.then_some(true),
                    "press_unavailable": (!down && !published).then_some(true),
                    "physical_press": approved_promotion.is_some().then_some(false),
                    "pointer_element": element, "pointer": points,
                    "physical_button": pointer.as_ref().map(|p| p.physical_button) })
            }
            Action::Motion(_) => {
                requests.pointer_moved(at, first.admission, proof, visit.generation, pointer.map(|pointer| pointer.points));
                permit_observation = self.last_pointer_move.replace((visit, at)).is_none_or(|(previous, last)| {
                    previous != visit || at.0.saturating_sub(last.0) >= 300_000
                });
                let second = at.0.div_euclid(1_000_000);
                let counts = self.motion.get_or_insert(MotionCounts { second, ..MotionCounts::default() });
                counts.coalesced = counts.coalesced.saturating_add(u64::from(
                    pointer.map_or(0, |pointer| pointer.raw_packets.saturating_sub(1))));
                counts.thinned = counts.thinned.saturating_add(u64::from(input.motion_thinned));
                if input.motion_thinned {
                    requests.retire_input(input, InputResolution::Suppressed);
                    return Ok(());
                }
                json!({ "kind": "pointer_move", "pointer": pointer.as_ref().map(|pointer| pointer.points) })
            }
            Action::Wheel { axis, value } => json!({
                "kind": "scroll", "axis": axis, "delta": value,
                "pointer": pointer.as_ref().map(|pointer| pointer.points),
                "raw_wheel": pointer.as_ref().map(|pointer| pointer.raw_wheel),
                "hook_wheel": pointer.as_ref().map(|pointer| pointer.hook_wheel),
            }),
            Action::Hid => {
                let InputSample::Hid(sample) = &first.packet.sample else {
                    requests.retire_input(input, InputResolution::Suppressed);
                    return Ok(());
                };
                source_time = self.hid.scan_time(sample, at);
                if input.motion_thinned {
                    let second = at.0.div_euclid(1_000_000);
                    let counts = self.motion.get_or_insert(MotionCounts { second, ..MotionCounts::default() });
                    counts.thinned = counts.thinned.saturating_add(1);
                    requests.retire_input(input, InputResolution::Suppressed);
                    return Ok(());
                }
                let (mut row, active) = self.hid.row(sample, at, report_reference.as_deref(), proof, source_time);
                active_pad_contact = active;
                if sample.is_pen() {
                    let published_tip = input.edge.is_some_and(|edge| self.held.contains(edge));
                    row["contact_transition"] = json!(first.pen_transition.filter(|down| *down || published_tip));
                    row["position_at_receipt"] = json!(first.packet.position_at_receipt);
                    row["position_at_receipt_geometry"] = json!(first.geometry);
                    permit_observation = match first.pen_transition { Some(down) => down != published_tip, None => false };
                }
                row
            }
            Action::AltGrCompanion => {
                requests.retire_input(input, InputResolution::Suppressed);
                return Ok(());
            }
        };
        self.flush_redactions(at, receipt, output, prefix)?;
        let mut row = row;
        if let Some(link) = approved_promotion {
            row["pen_promotion_link"] = json!({
                "pen_report": self.hid_reference(&link.pen, report_scope),
                "pointer_report": self.hid_reference(&link.pointer, report_scope),
                "receipt_gap_us": link.gap_us,
            });
        }
        if matches!(first.action, Action::Key(_)) && normal.origin.origin == InputOrigin::Injected
            && let Some(candidates) = self.hid.ndof_key_candidates(at, proof)
        {
            row["ndof_button_candidates"] = candidates;
        }
        row["window"] = json!(proof.window);
        if let Some((device, witness)) = touchpad {
            row["origin"] = json!(InputOrigin::Touchpad);
            row["device"] = json!(Evidence::Observed(device));
            row["touchpad_contact_witness"] = witness;
        } else {
            row["origin"] = json!(normal.origin.origin);
            row["device"] = json!(normal.origin.device);
        }
        if matches!(normal.origin.origin, InputOrigin::PenPromoted | InputOrigin::TouchPromoted) {
            row["cursor_id"] = json!(normal.origin.cursor_id);
        }
        row["correlation"] = json!(normal.correlated.correlation);
        row["admission_epoch"] = json!(first.admission);
        row["source_t_us"] = json!(source_time);
        row["source_uncertainty_us"] = json!(first.time.source_uncertainty_us);
        // A correlated group has at most one hook. Prefer its tick without
        // replacing the earlier logical receipt that orders the group.
        let tick_witness = if matches!(first.packet.sample, InputSample::RawKey(_) | InputSample::RawPointer(_)) {
            normal.correlated.partners.iter().find(|slot| matches!(slot.packet.sample,
                InputSample::HookKey(_) | InputSample::HookPointer(_)) && slot.time.coarse.is_some()).unwrap_or(first)
        } else {
            first
        };
        let injected_hook = match tick_witness.packet.sample {
            InputSample::HookKey(key) => key.flags & 0x12 != 0,
            InputSample::HookPointer(pointer) => pointer.flags & 3 != 0,
            _ => false,
        };
        let coarse = tick_witness.time.coarse;
        let late = coarse.filter(|_| !injected_hook)
            .and_then(|tick| clock::message_tick_estimate(receipt.clock, tick_witness.tick_anchor, tick))
            .map(|estimate| estimate.is_late(at));
        row["tick_ms"] = json!(coarse.map(|time| time.tick_ms));
        row["tick_uncertainty_us"] = json!(coarse.map(|time| time.uncertainty_us));
        row["late"] = json!(late);
        if let Some(pointer) = pointer {
            let (modifiers, buttons) = self.held.modifiers_and_buttons();
            row["published_held_before"] = json!({ "modifiers": modifiers, "buttons": buttons });
            row["raw_relative"] = json!(pointer.raw_relative);
            row["raw_absolute"] = json!(pointer.raw_absolute);
            row["raw_packets"] = json!(pointer.raw_packets);
            row["warp"] = json!(pointer.warp);
        }
        let geometry = pointer.map_or(first.geometry, |pointer| pointer.points.geometry);
        let kind = match first.action {
            Action::Motion(_) => Some(ProvisionalInputKind::PreMotion),
            Action::Button { down, .. } => Some(if down { ProvisionalInputKind::PreAction } else { ProvisionalInputKind::Next }),
            Action::Key(key) => Some(if key.down { ProvisionalInputKind::PreAction } else { ProvisionalInputKind::Next }),
            Action::Wheel { axis, .. } => Some(ProvisionalInputKind::Scroll { axis }),
            Action::Hid => first.pen_transition.map(|down| if down { ProvisionalInputKind::PreAction } else { ProvisionalInputKind::Next }),
            Action::AltGrCompanion => None,
        };
        let lacks_confirmation_capacity = permit_observation && kind.is_some() && self.in_flight.len() == MAX_CONFIRMATIONS;
        let (disposition, confirmation) = if permit_observation && let Some(kind) = kind
            && !lacks_confirmation_capacity
        {
            let observation = requests.choose(&input, geometry, proof, kind);
            let device = match pointer.map_or(normal.origin.device, |pointer| pointer.points.device) {
                Evidence::Observed(device) => Some(device), _ => None,
            };
            let (disposition, event) = output.try_submit_confirmed(at, row).map_err(package_journal_error)?;
            (disposition, Some(Confirmation { observation, geometry, kind, admission: first.admission, proof, device, at, event }))
        } else {
            (output.try_submit(at, row).map_err(package_journal_error)?, None)
        };
        prefix.advance_time(at, disposition)?;
        if let Some(knowledge) = modifier_knowledge {
            if disposition == Submit::Queued {
                self.modifier_knowledge = knowledge;
            } else {
                self.conceal_modifier(&input);
            }
        }
        if disposition == Submit::Queued {
            requests.activity(at);
            if lacks_confirmation_capacity {
                requests.approved_observation_gap(at, geometry, kind);
            }
        }
        if disposition == Submit::Queued && let Some(edge) = input.edge {
            self.held.published(edge);
        }
        if disposition == Submit::Queued && let Some(generation) = visit.generation {
            context.note_approved_input(generation, qpc);
        }
        if disposition == Submit::Queued {
            if let InputSample::Hid(sample) = &first.packet.sample {
                if (sample.usage_page, sample.usage) == (Some(0x0D), Some(5)) {
                    self.hid.observe_touchpad(sample.device, at, proof, report_reference.as_deref(), active_pad_contact);
                }
                self.hid.observe_published_ndof(sample, at, report_reference, proof);
                use crate::protocol::ipc::{HidReportResult, HidUsage, HidValue};
                if sample.is_pen() && let HidReportResult::Report(report) = &sample.result {
                    self.pen_observed = true;
                    self.pressure_observed |= report.fields.as_slice().iter().any(|field| {
                        field.usage == HidUsage::Pressure && matches!(field.value,
                            HidValue::Number { raw: Some(raw), spec } if spec.logical[1] > spec.logical[0]
                                && (spec.logical[0]..=spec.logical[1]).contains(&raw))
                    });
                }
            }
            for commit in commits {
                Self::publish_text(at, receipt, visit.generation, None, commit, output, prefix)?;
            }
        } else {
            self.keyboard_text.reset();
            if let InputSample::Hid(sample) = &first.packet.sample {
                self.hid.reset(Some(sample.device));
            }
        }
        match (disposition, confirmation) {
            (Submit::Queued, Some(confirmation)) => {
                requests.retire_except(&input, confirmation.observation);
                self.in_flight.push_back(confirmation);
            }
            (Submit::Dropped, _) => requests.retire_input(input, InputResolution::Dropped(DropReason::Capacity)),
            _ => requests.retire_input(input, InputResolution::Suppressed),
        }
        Ok(())
    }

    fn conceal_modifier(&mut self, input: &HeldInput) {
        if let Action::Key(key) = input.normalized.correlated.first.action {
            self.modifier_knowledge.conceal(modifier_bits(key));
        }
    }

    fn hid_reference(&self, input: &crate::protocol::ipc::CollectedInput, visit: Option<VisitBinding>) -> Option<String> {
        let key = self.reference_key.as_ref()?;
        let mut data = [0_u8; 9];
        data[0] = match input.sample.producer() {
            Producer::RawInput => 1,
            Producer::PointerHook => 2,
            _ => return None,
        };
        data[1..].copy_from_slice(&input.receipt.sequence.0.to_le_bytes());
        let mut hash = blake3::Hasher::new_keyed(key);
        if let Some(visit) = visit {
            hash.update(b"elevated report\0");
            hash.update(&visit.epoch.0.to_le_bytes());
            hash.update(&visit.source_generation.get().to_le_bytes());
            hash.update(&visit.grant.get().to_le_bytes());
            hash.update(&visit.focus_generation.0.to_le_bytes());
            hash.update(&visit.focus_revision.to_le_bytes());
            hash.update(&visit.resync_revision.get().to_le_bytes());
        }
        hash.update(&data);
        Some(hash.finalize().to_hex().as_str()[..32].to_owned())
    }

    fn publish_text(
        at: SessionTimeUs, receipt: Receipt, generation: Option<FocusGeneration>,
        provider_receipt: Option<SessionTimeUs>, commit: TextCommit,
        output: &mut JournalIngress, prefix: &mut ReceiptPrefix,
    ) -> Result<(), ProbeError> {
        let row = json!({
            "kind": "text_commit", "text": commit.text, "status": commit.status, "provenance": commit.provenance,
            "focus_generation": generation, "admission_epoch": receipt.boundary.epoch,
            "text_edit_receipt_t_us": provider_receipt,
        });
        prefix.advance_time(at, output.try_submit(at, row).map_err(package_journal_error)?)
    }

    pub fn resolve_confirmed(&mut self, requests: &mut ProvisionalRequests) {
        while let Some(confirmation) = self.in_flight.front() {
            let event = match confirmation.event.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => None,
            };
            let confirmation = self.in_flight.pop_front().expect("confirmation was present");
            if let Some(event) = event {
                requests.published(confirmation.observation, confirmation.geometry, confirmation.proof,
                    confirmation.device, confirmation.at, confirmation.kind, event, confirmation.admission);
            } else if let Some(id) = confirmation.observation {
                requests.resolve(id, InputResolution::Suppressed);
            }
        }
    }

    fn finish_redaction(&mut self, end: Option<SessionTimeUs>) {
        let Some(mut visit) = self.redacted.take() else { return };
        visit.end = end.filter(|end| visit.start.is_none_or(|start| *end >= start));
        if self.completed_redactions.len() == 64 {
            self.overflow = true;
            // Preserve only a combined unknown visit when the private
            // aggregate queue cannot retain individual visit boundaries.
            if let Some(last) = self.completed_redactions.back_mut() {
                last.start = None;
                last.end = None;
                last.count = last.count.saturating_add(visit.count);
            }
        } else {
            self.completed_redactions.push_back(visit);
        }
    }

    fn flush_redactions(
        &mut self, at: SessionTimeUs, receipt: Receipt, output: &mut JournalIngress, prefix: &mut ReceiptPrefix,
    ) -> Result<(), ProbeError> {
        for visit in self.completed_redactions.drain(..) {
            let row = json!({ "kind": "redacted", "field_start_t_us": visit.start, "field_end_t_us": visit.end,
                "key_count": visit.count, "admission_epoch": receipt.boundary.epoch });
            prefix.advance_time(at, output.try_submit(at, row).map_err(package_journal_error)?)?;
        }
        Ok(())
    }

    fn flush_motion(
        &mut self, through: SessionTimeUs, final_boundary: bool, receipt: Receipt,
        output: &mut JournalIngress, prefix: &mut ReceiptPrefix,
    ) -> Result<(), ProbeError> {
        let Some(counts) = &self.motion else { return Ok(()) };
        let end = SessionTimeUs(counts.second.saturating_add(1).saturating_mul(1_000_000));
        if !final_boundary && end > through {
            return Ok(());
        }
        let counts = self.motion.take().expect("motion bucket was present");
        if counts.coalesced == 0 && counts.thinned == 0 {
            return Ok(());
        }
        let at = end.min(through).max(prefix.last_time.unwrap_or(receipt.boundary.time));
        let row = json!({ "kind": "motion_sampling", "session_second": counts.second,
            "raw_packets_coalesced": counts.coalesced, "motion_rows_thinned": counts.thinned,
            "admission_epoch": receipt.boundary.epoch });
        prefix.advance_time(at, output.try_submit(at, row).map_err(package_journal_error)?)
    }

    fn close(
        &mut self, at: SessionTimeUs, kind: HeldBoundary, selection: Selection, receipt: Receipt,
        output: &mut JournalIngress, prefix: &mut ReceiptPrefix,
    ) -> Result<(), ProbeError> {
        let at = if matches!(kind, HeldBoundary::InputGap | HeldBoundary::HookUnavailable
            | HeldBoundary::DeviceRemoved | HeldBoundary::RecorderControl | HeldBoundary::HidReportUnavailable) {
            prefix.last_time.unwrap_or(receipt.boundary.time)
        } else {
            at.max(prefix.last_time.unwrap_or(receipt.boundary.time))
        };
        if matches!(kind, HeldBoundary::InputGap | HeldBoundary::HookUnavailable | HeldBoundary::DeviceRemoved) {
            let row = json!({ "kind": "input_continuity_gap", "reason": kind,
                "admission_epoch": receipt.boundary.epoch });
            prefix.advance_time(at, output.try_submit(at, row).map_err(package_journal_error)?)?;
        }
        if kind == HeldBoundary::FocusChanged || is_closing(kind) {
            if let Some(commit) = self.keyboard_text.finish_packet() {
                Self::publish_text(at, receipt, self.active_key_visit.and_then(|visit| visit.generation), None, commit, output, prefix)?;
            }
            self.active_key_visit = None;
        }
        self.keyboard_text.reset();
        match kind {
            HeldBoundary::DeviceRemoved => {
                if let Selection::Removed(device) = selection {
                    self.forget_input_device(device);
                }
            }
            HeldBoundary::HidReportUnavailable => {
                if let Selection::PenDevice(device) = selection {
                    self.hid.reset(Some(device));
                }
            }
            HeldBoundary::InputGap | HeldBoundary::HookUnavailable | HeldBoundary::AdmissionClosed | HeldBoundary::Pause | HeldBoundary::Stop => self.hid.reset(None),
            _ => {}
        }
        match kind {
            HeldBoundary::InputGap | HeldBoundary::HookUnavailable
                | HeldBoundary::AdmissionClosed | HeldBoundary::Pause | HeldBoundary::Stop => {
                self.modifier_knowledge = ModifierKnowledge::from_released(0);
            }
            HeldBoundary::DeviceRemoved => {
                // Keep candidates private until the next verified synchronization.
                self.modifier_knowledge.awaiting_confirmation |= self.modifier_knowledge.known & !self.modifier_knowledge.possibly_hidden;
                self.modifier_knowledge.known = 0;
            }
            HeldBoundary::FocusChanged => self.modifier_knowledge.conceal(self.held.modifiers_and_buttons().0),
            HeldBoundary::RecorderControl => {
                if let Selection::Modifiers(bits) = selection {
                    self.modifier_knowledge.conceal(bits);
                }
            }
            HeldBoundary::Synchronization | HeldBoundary::HidReportUnavailable => {}
        }
        let edges = self.held.close(at, kind, |held| selection.contains(held));
        if kind == HeldBoundary::DeviceRemoved {
            self.modifier_knowledge.awaiting_confirmation &= self.held.modifiers_and_buttons().0;
        }
        if is_closing(kind) {
            output.submit_boundary(at, edges.iter().copied().map(|edge| held_row(edge, receipt)))
                .map_err(package_journal_error)?;
            for _ in 0..edges.len() { prefix.advance_time(at, Submit::Queued)?; }
        } else {
            for edge in edges {
                let submitted = output.try_submit(at, held_row(edge, receipt)).map_err(package_journal_error)?;
                prefix.advance_time(at, submitted)?;
            }
        }
        Ok(())
    }
}

fn clean_null_pointer(input: &crate::capture::input::NormalizedInput) -> bool {
    if input.origin.origin != InputOrigin::Unknown || input.correlated.correlation != Correlation::Matched
        || input.pointer.is_none()
    {
        return false;
    }
    let mut raw = false;
    let mut hook = false;
    for slot in std::iter::once(&input.correlated.first).chain(&input.correlated.partners) {
        match &slot.packet.sample {
            InputSample::RawPointer(pointer) if pointer.device.is_none()
                && !has_promotion(u64::from(pointer.extra_information)) => raw = true,
            InputSample::HookPointer(pointer) if pointer.flags & 3 == 0
                && !has_promotion(pointer.extra_information) => hook = true,
            _ => return false,
        }
    }
    raw && hook
}

fn approved_receipt(input: &HeldInput, context: &mut Context<'_>) -> Option<(ForegroundAtReceipt, ReceiptFocus)> {
    let input = &input.normalized.correlated;
    if matches!(input.correlation, Correlation::Ambiguous | Correlation::MissingScan | Correlation::Discontinuous) {
        return None;
    }
    let slots = || std::iter::once(&input.first).chain(&input.partners);
    let producer = crate::protocol::ipc::Producer::RawInput;
    let first_raw = slots().find(|slot| slot.packet.sample.producer() == producer);
    let promotion_pen = input.promotion.as_ref().map(|link| link.pen.as_ref());
    let proof = first_raw.map(|slot| slot.packet.as_ref()).or(promotion_pen)?.foreground_at_receipt?;
    if !slots().filter(|slot| slot.packet.sample.producer() == producer)
        .all(|slot| slot.packet.foreground_at_receipt == Some(proof))
    {
        return None;
    }
    let borrowed_pen_qpc = first_raw.is_none().then(|| promotion_pen.map(|pen| pen.receipt.qpc)).flatten();
    let times = || slots().map(|slot| slot.packet.receipt.qpc).chain(borrowed_pen_qpc);
    let first = times().min()?;
    let last = times().max()?;
    let focus = context.approve_receipt(proof, last, first)?;
    let uses_hook = slots().any(|slot| slot.packet.sample.producer() != producer);
    if uses_hook && matches!(focus, ReceiptFocus::Unknown | ReceiptFocus::Historical(_)) {
        return None;
    }
    Some((proof, focus))
}

fn promotion_focus(
    link: &crate::capture::input::PenPromotion,
    proof: ForegroundAtReceipt,
    context: &mut Context<'_>,
) -> bool {
    let pen = link.pen.receipt.qpc;
    let pointer = link.pointer.receipt.qpc;
    context.approve_receipt(proof, pen.max(pointer), pen.min(pointer))
        .is_some_and(|focus| !matches!(focus, ReceiptFocus::Unknown | ReceiptFocus::Historical(_)))
}

fn reference_key() -> Option<[u8; 32]> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

    let mut key = [0; 32];
    let status = unsafe {
        BCryptGenRandom(std::ptr::null_mut(), key.as_mut_ptr(), key.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    (status == 0).then_some(key)
}

fn is_closing(reason: HeldBoundary) -> bool {
    matches!(reason, HeldBoundary::AdmissionClosed | HeldBoundary::Pause | HeldBoundary::Stop)
}

fn held_row(edge: SyntheticHeldEdge, receipt: Receipt) -> serde_json::Value {
    let down = edge.edge.down;
    let kind = match edge.edge.identity.holding {
        crate::capture::input::Holding::Key { .. } => if down { "key_down" } else { "key_up" },
        crate::capture::input::Holding::Button { .. } |
            crate::capture::input::Holding::PromotedPenPointer { .. } => if down { "pointer_down" } else { "pointer_up" },
        crate::capture::input::Holding::PenTip { .. } => if down { "pen_down" } else { "pen_up" },
    };
    let mut row = json!({
        "kind": kind, "synthetic": true, "boundary": edge.reason, "edge": edge.edge,
        "admission_epoch": receipt.boundary.epoch,
    });
    if matches!(edge.edge.identity.holding, crate::capture::input::Holding::Key { .. }) {
        row["raw_input"] = serde_json::Value::Null;
        row["keyboard_hook"] = serde_json::Value::Null;
        row["modifiers"] = json!(0);
        row["modifiers_known"] = json!(0);
    }
    row
}
