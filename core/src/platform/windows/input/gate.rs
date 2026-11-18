//! Ordered input publication. Windows approval and focus are evaluated
//! against the private receipt proof before any per-input payload is built.

use std::{collections::{BTreeMap, BTreeSet, VecDeque}, ops::ControlFlow, sync::mpsc};

use serde_json::{Value, json};

use crate::{
    devlog::devlog,
    capture::{
        context::{ControllerHint, TextTarget},
        observation::ProvisionalInputKind,
        input::{
            Action, Button, ClassifiedControlInput, ControlDisposition, CorrelatedInput, Correlation, HeldBoundary, HeldIdentity,
            SyntheticHeldEdge, HeldInput, Holding, InputOrigin, Key, PendingInput, PenPromotion, PublishedHeld, ScanCode,
            VK_PACKET, buttons_in, has_promotion, injected_hook, modifier_bit, modifier_of, modifiers_in,
        },
    },
    package::{
        diagnostics::Failure,
        journal::{HealthRecord, Submit},
        outcome::{HealthKind, LossCount, LossEvidence, LossProducer, LossReason, LossScope, SuppressionClass},
        row::RowEnvelope,
    },
    protocol::{
        AdmissionEpoch, DeviceId, EventRef, Evidence, FocusGeneration, GeometryId, ObservationId, QpcTick, RecordTime,
        SessionTimeUs, UnavailableReason,
        ipc::{
            DeviceDescriptor, ForegroundAtReceipt, HidReport, HidReportResult, HidUsage, HidValue, InputSample,
            MAX_INPUT_DEVICES, OptionalCollection, Producer, elevated::VisitBinding,
        },
    },
    platform::windows::controller::Receipt,
    util::keep_earlier,
};

use super::super::{ProbeError, clock, unavailable, context::{ConfirmedText, Context, HookReceipt, ImeState, ReceiptFocus, Visit}};
use crate::capture::input::{
    blind::Blinds, hid::{self, HidPublication}, loss::{is_key, record_loss},
    publication::{ModifierKnowledge, MotionCounts, Outcomes, modifier_bits, per_second},
};
use super::{
    provisional::ProvisionalRequests, text::{KeyboardText, TextCommit, TextOrigin, TextStatus, Typing, text_key},
    writer::InputWriter,
};

const MAX_SLOTS: usize = 1024;
const MAX_CONFIRMATIONS: usize = 256;
/// Confirmed text commits awaiting their place in the input order.
const MAX_CONFIRMED_TEXT: usize = 16;
const FOCUS_CONFIRM_US: i64 = 100_000;
const MAX_WAIT_US: i64 = 500_000;
/// Completed redacted visits kept apart; later ones merge into one unknown visit.
const MAX_COMPLETED_REDACTIONS: usize = 64;
/// A text key this long after the previous one of its visit starts a new burst.
const TEXT_BURST_US: i64 = 500_000;
/// A pointer move this long after the previous one of its visit can open an observation.
const POINTER_MOVE_OBSERVATION_US: i64 = 300_000;

pub(super) struct Gate {
    slots: VecDeque<Slot>,
    in_flight: VecDeque<Confirmation>,
    held: PublishedHeld,
    modifier_knowledge: ModifierKnowledge,
    hid: HidPublication,
    pen: PenEvidence,
    reference_key: Option<[u8; 32]>,
    touchpads: TouchpadInventory,
    redactions: Redactions,
    motion: Option<MotionCounts>,
    overflow_at: Option<SessionTimeUs>,
    overflow: bool,
    active_key_visit: Option<VisitKey>,
    keyboard_text: KeyboardText,
    last_text_key: Option<(VisitKey, SessionTimeUs)>,
    last_pointer_move: Option<(VisitKey, SessionTimeUs)>,
    confirmed_text: VecDeque<(SessionTimeUs, ConfirmedText)>,
    outcomes: Outcomes,
    /// Controller processes running at start (CTX-12), the injector hints of
    /// every injected row (INP-7).
    injector_hints: Vec<ControllerHint>,
    suppression_log: SuppressionLog,
}

/// Pending runtime health conditions; each kind is rare.
const MAX_PENDING_HEALTH: usize = 32;

enum Slot {
    Input(ClassifiedControlInput),
    Synchronization {
        at: SessionTimeUs, visit: Visit, held_modifiers: u8, released_modifiers: u8, edges: Vec<SyntheticHeldEdge>,
    },
    Boundary { at: SessionTimeUs, kind: HeldBoundary, selection: Selection },
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
                && matches!(held.holding, Holding::PenTip { .. }),
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

/// The receipt approval of an input row, or the class that suppresses it.
type Approval = Result<(ForegroundAtReceipt, ReceiptFocus), SuppressionClass>;

/// What the front slot of the queue can do now.
enum Front {
    /// The slot waits for more approval, focus or IME evidence.
    Wait,
    /// The slot is decided now; an input slot with this approval. `cause`
    /// explains an `ApprovalUnknown` in the development log only.
    Publish { approval: Approval, ime: ImeState, into_redaction: bool, cause: Option<UnknownCause> },
}

struct RedactedVisit {
    start: Option<SessionTimeUs>,
    end: Option<SessionTimeUs>,
    count: u64,
}

/// Key counts of password and possibly sensitive field visits: the open
/// visit, and the completed ones awaiting their `redacted` rows.
struct Redactions {
    open: Option<RedactedVisit>,
    completed: VecDeque<RedactedVisit>,
}

impl Redactions {
    fn new() -> Self {
        Self { open: None, completed: VecDeque::with_capacity(MAX_COMPLETED_REDACTIONS) }
    }

    fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// A key down of a redacted visit that started at `start`.
    fn count_key(&mut self, start: Option<SessionTimeUs>) {
        let visit = self.open.get_or_insert(RedactedVisit { start, end: None, count: 0 });
        visit.count = visit.count.saturating_add(1);
    }

    /// A suppressed key joins the open visit; false without one.
    fn join_open(&mut self) -> bool {
        let Some(visit) = &mut self.open else { return false };
        visit.count = visit.count.saturating_add(1);
        true
    }

    /// End the open visit at `end`. True when the completed queue was full,
    /// so the visit merged into the last one as a visit of unknown bounds.
    fn finish(&mut self, end: Option<SessionTimeUs>) -> bool {
        let Some(mut visit) = self.open.take() else { return false };
        visit.end = end.filter(|end| visit.start.is_none_or(|start| *end >= start));
        if self.completed.len() == MAX_COMPLETED_REDACTIONS {
            // Preserve only a combined unknown visit when the private
            // aggregate queue cannot retain individual visit boundaries.
            if let Some(last) = self.completed.back_mut() {
                last.start = None;
                last.end = None;
                last.count = last.count.saturating_add(visit.count);
            }
            true
        } else {
            self.completed.push_back(visit);
            false
        }
    }
}

/// The active touchpads, which witness the contact of a null-device pointer row.
#[derive(Default)]
struct TouchpadInventory {
    devices: BTreeSet<DeviceId>,
    uncertain: bool,
}

impl TouchpadInventory {
    fn note(&mut self, descriptor: &DeviceDescriptor) {
        if descriptor.kind.hid_usage() == Some(OptionalCollection::Touchpad.usage()) {
            if self.devices.len() == MAX_INPUT_DEVICES && !self.devices.contains(&descriptor.id) {
                self.uncertain = true;
            } else {
                self.devices.insert(descriptor.id);
            }
        }
    }

    fn forget(&mut self, device: DeviceId) {
        self.devices.remove(&device);
    }

    fn mark_uncertain(&mut self) {
        self.uncertain = true;
    }

    fn reset(&mut self) {
        self.devices.clear();
        self.uncertain = false;
    }

    /// The only touchpad, when the inventory is certain.
    fn sole(&self) -> Option<DeviceId> {
        (!self.uncertain && self.devices.len() == 1).then(|| self.devices.first().copied()).flatten()
    }
}

/// Pen evidence of the published reports, for the manifest's pen capture.
#[derive(Default)]
struct PenEvidence {
    observed: bool,
    pressure: bool,
}

impl PenEvidence {
    /// A published HID report: a pen report, and its normalized pressure.
    fn observe(&mut self, sample: &HidReport) {
        if sample.is_pen() && let HidReportResult::Report(report) = &sample.result {
            self.observed = true;
            self.pressure |= report.fields.as_slice().iter().any(|field| {
                field.usage == HidUsage::Pressure && matches!(field.value,
                    HidValue::Number { raw: Some(raw), spec } if hid::normalize(raw, spec.logical).is_some())
            });
        }
    }
}

/// An input row before its envelope, with what publication applies once
/// the row is submitted.
struct RowDraft {
    row: Value,
    /// The row may confirm a provisional still observation.
    permit_observation: bool,
    /// A key down that is not a known press never enters the published held state.
    publish_edge: bool,
    source_time: Option<SessionTimeUs>,
    /// The modifier knowledge after a key row, adopted once it is queued.
    modifier_knowledge: Option<ModifierKnowledge>,
    /// Text inferred from a key row, published after it.
    commits: Vec<TextCommit>,
    /// A touchpad report with an active contact.
    active_pad_contact: bool,
}

impl RowDraft {
    fn new(row: Value, first: &PendingInput) -> Self {
        Self {
            row, permit_observation: true, publish_edge: true, source_time: first.time.source_t_us,
            modifier_knowledge: None, commits: Vec::new(), active_pad_contact: false,
        }
    }
}

impl Gate {
    pub fn new(injector_hints: Vec<ControllerHint>) -> Self {
        Self {
            slots: VecDeque::with_capacity(MAX_SLOTS), in_flight: VecDeque::with_capacity(MAX_CONFIRMATIONS),
            hid: HidPublication::default(), pen: PenEvidence::default(),
            reference_key: reference_key(), touchpads: TouchpadInventory::default(),
            held: PublishedHeld::default(), modifier_knowledge: ModifierKnowledge::from_released(0),
            redactions: Redactions::new(),
            motion: None, overflow_at: None, overflow: false, active_key_visit: None,
            keyboard_text: KeyboardText::default(), last_text_key: None, last_pointer_move: None, confirmed_text: VecDeque::with_capacity(MAX_CONFIRMED_TEXT),
            outcomes: Outcomes::default(), injector_hints, suppression_log: SuppressionLog::default(),
        }
    }

    /// Record typed loss evidence for the input stream.
    pub fn note_loss(&mut self, evidence: LossEvidence) {
        self.outcomes.loss(evidence);
    }

    /// Synthetic held edges lost at `at`.
    fn note_edges_lost(&mut self, reason: LossReason, edges: usize, at: SessionTimeUs) {
        if let Some(count) = std::num::NonZeroU64::new(edges as u64) {
            self.note_loss(LossEvidence::ordinary(reason, None, count, at, at));
        }
    }

    /// Count a policy suppression of input items in the stream totals only.
    pub fn note_suppressed(&mut self, class: SuppressionClass, count: u64) {
        if count == 0 { return; }
        let total = self.outcomes.suppressed.entry(class).or_default();
        *total = total.saturating_add(count);
    }

    /// A suppression decided in publication order at `at`. Eligible classes
    /// are also counted in the `input_health` row of that second.
    fn suppress_at(&mut self, class: SuppressionClass, count: u64, at: SessionTimeUs) {
        if count == 0 { return; }
        self.note_suppressed(class, count);
        if per_second(class) {
            let total = MotionCounts::at(&mut self.motion, at).suppressed.entry(class).or_default();
            *total = total.saturating_add(count);
        }
    }

    /// An ongoing runtime input health condition. It is admitted in receipt
    /// order once every earlier record has been published.
    pub fn note_health(&mut self, kind: HealthKind, failure: Failure, detected: SessionTimeUs, first_affected: SessionTimeUs) {
        let health = HealthRecord {
            kind, failure, detected, first_affected: Some(first_affected), ongoing: true, monitor: None, count: None,
        };
        if self.outcomes.health.len() < MAX_PENDING_HEALTH {
            self.outcomes.health.push(health);
        } else if let Some(same) = self.outcomes.health.iter_mut().rev().find(|pending| pending.kind == kind) {
            // Keep the bound: the latest pending condition of the same kind absorbs the count.
            same.count = Some(same.count.unwrap_or(1).saturating_add(1));
        } else {
            // A condition of a kind with no pending record cannot be kept.
            self.overflow = true;
        }
    }

    /// Account each lost record once, by its producer and privacy class
    /// ([`record_loss`]). An ordinary range starts at `since` when the loss
    /// began earlier, such as at the last producer progress (§6.3).
    pub fn note_records_lost<'a>(
        &mut self, reason: LossReason, records: impl IntoIterator<Item = &'a PendingInput>,
        since: Option<SessionTimeUs>,
    ) {
        for record in records {
            let at = record.time.recv_t_us;
            let first = since.map_or(at, |since| since.min(at));
            let sample = &record.packet.sample;
            self.outcomes.loss(record_loss(reason, sample.producer(), is_key(sample), first, at));
        }
    }

    /// Admit pending outcomes to the journal. Ordinary losses wait until every
    /// record at or before their last receipt is published (`ready`). At a
    /// closing boundary (`closing`) every remaining loss is admitted: a range
    /// past the cutoff is clipped with an unknown count, and a loss wholly
    /// after the cutoff belongs to no admitted interval and is discarded.
    fn admit_outcomes(
        &mut self, ready: Option<SessionTimeUs>, closing: Option<SessionTimeUs>, receipt: Receipt,
        output: &mut InputWriter, blinds: &mut Blinds,
    ) -> Result<(), ProbeError> {
        for (class, count) in std::mem::take(&mut self.outcomes.suppressed) {
            output.record_suppressed(class, count);
        }
        // Blind intervals join the input stream (INP-13) at their end, raised to the published prefix.
        for blind in blinds.take_ready(ready, closing) {
            let at = output.raised(blind.end);
            let row = blind.row().map_err(|_| unavailable("Encode an input blind row"))?;
            output.submit_boundary(at, vec![row])?;
        }
        let health: Vec<_> = self.outcomes.health
            .extract_if(.., |record| closing.is_some() || ready.is_some_and(|ready| record.detected <= ready)).collect();
        for mut record in health {
            if let Some(cutoff) = closing { record.detected = record.detected.min(cutoff); }
            output.submit_health(record)?;
        }
        let admitted = output.published_or(receipt.boundary.time);
        for evidence in std::mem::take(&mut self.outcomes.sensitive) {
            output.record_loss(evidence, admitted)?;
        }
        // `ordinary` holds ordinary-scope losses only.
        let mut due: Vec<_> = self.outcomes.ordinary.extract_if(.., |evidence| matches!(evidence.scope,
            LossScope::Ordinary(range) if closing.is_some() || ready.is_some_and(|ready| range.last_t_us <= ready)))
            .filter_map(|evidence| match evidence.scope {
                LossScope::Ordinary(range) => Some((range, evidence)),
                LossScope::Sensitive => None,
            })
            .collect();
        due.sort_by_key(|(range, _)| (range.first_t_us, range.last_t_us));
        for (mut range, mut evidence) in due {
            if let Some(cutoff) = closing {
                if range.first_t_us > cutoff { continue; }
                if range.last_t_us > cutoff {
                    range.last_t_us = cutoff;
                    evidence.count = LossCount::Unknown;
                    evidence.scope = LossScope::Ordinary(range);
                }
            }
            output.admit_loss(evidence, range.last_t_us)?;
        }
        Ok(())
    }

    pub fn push(&mut self, input: ClassifiedControlInput, requests: &mut ProvisionalRequests) {
        if self.slots.len() >= MAX_SLOTS || self.overflow_at.is_some() {
            self.overflow = true;
            self.overflow_at.get_or_insert(input.input.receipt());
            // A row is one item; its partner and absorbed evidence belongs to it.
            self.note_records_lost(LossReason::StagingPressure, [input.input.first()], None);
            requests.retire_input(input.input);
        } else {
            self.slots.push_back(Slot::Input(input));
        }
    }

    pub fn boundary(&mut self, at: SessionTimeUs, kind: HeldBoundary, selection: Selection) {
        if !is_closing(kind) && (self.slots.len() >= MAX_SLOTS || self.overflow_at.is_some()) {
            self.overflow = true;
            keep_earlier(&mut self.overflow_at, at);
        } else {
            self.slots.push_back(Slot::Boundary { at, kind, selection });
        }
    }

    /// Keep only the certified prefix before `before`. Rows outside it that
    /// start before the closing boundary `close_at` were admitted and are
    /// counted as lost; rows at or after `close_at` were never admitted.
    pub fn retain_prefix(
        &mut self, before: SessionTimeUs, close_at: SessionTimeUs, hook_before: Option<QpcTick>,
        requests: &mut ProvisionalRequests,
    ) {
        let reason = if hook_before.is_some() { LossReason::HookLost } else { LossReason::ProducerStall };
        for _ in 0..self.slots.len() {
            let Some(slot) = self.slots.pop_front() else { break; };
            match slot {
                Slot::Input(input) => {
                    let correlated = &input.input.normalized.correlated;
                    let promotion_covered = hook_before.is_none_or(|before| correlated.promotion.as_ref()
                        .is_none_or(|promotion| promotion.pen.receipt.qpc < before && promotion.pointer.receipt.qpc < before));
                    if promotion_covered && correlated.records().all(|receipt| receipt.time.recv_t_us < before) {
                        self.slots.push_back(Slot::Input(input));
                    } else {
                        if correlated.first.time.recv_t_us < close_at {
                            self.note_records_lost(reason, [&correlated.first], None);
                        }
                        requests.retire_input(input.input);
                    }
                }
                Slot::Synchronization { at, edges, .. } if at >= before => {
                    if at < close_at {
                        self.note_edges_lost(reason, edges.len(), at);
                    }
                }
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
            self.note_edges_lost(LossReason::StagingPressure, edges.len(), at);
            return;
        }
        self.slots.push_back(Slot::Synchronization { at, visit, held_modifiers, released_modifiers, edges });
    }

    pub fn note_input_device(&mut self, descriptor: &DeviceDescriptor) {
        self.touchpads.note(descriptor);
    }

    pub fn forget_input_device(&mut self, device: DeviceId) {
        self.touchpads.forget(device);
        self.hid.reset(Some(device));
    }

    /// An optional HID loss resets only touchpad and multi-axis publication state.
    pub fn reset_optional_hid(&mut self) {
        self.hid.reset_optional();
    }

    pub fn note_device_inventory_loss(&mut self) {
        self.touchpads.mark_uncertain();
    }

    pub fn reset_device_inventory(&mut self) {
        self.touchpads.reset();
        self.hid.reset(None);
    }

    /// Whether a pen report, and one with pressure, was published.
    pub fn pen_observation(&self) -> (bool, bool) {
        (self.pen.observed, self.pen.pressure)
    }

    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    fn first_slot(&self) -> Option<SessionTimeUs> {
        self.slots.front().map(|slot| match slot {
            Slot::Input(input) => input.input.receipt(),
            Slot::Boundary { at, .. } | Slot::Synchronization { at, .. } => *at,
        })
    }

    /// The certified prefix that no unresolved slot holds back, up to `through`.
    fn ready(&self, certified_before: Option<SessionTimeUs>, through: SessionTimeUs) -> Option<SessionTimeUs> {
        certified_before.map(|certified| self.unresolved_before().map_or(certified, |pending| pending.min(certified)).min(through))
    }

    pub fn unresolved_before(&self) -> Option<SessionTimeUs> {
        [self.first_slot(), self.overflow_at, self.confirmed_text.front().map(|(at, _)| *at)].into_iter().flatten().min()
    }

    pub fn stage_confirmed(&mut self, text: ConfirmedText, receipt: Receipt) {
        if text.callback_at < receipt.boundary.qpc { return; }
        let Ok(at) = receipt.clock.time(text.observed_at) else { return };
        if self.confirmed_text.len() >= MAX_CONFIRMED_TEXT {
            self.overflow = true;
            self.note_loss(text_loss(LossReason::CommitCapacity, at));
            return;
        }
        self.confirmed_text.push_back((at, text));
    }

    pub fn drain(
        &mut self, context: &mut Context<'_>, output: &mut InputWriter,
        requests: &mut ProvisionalRequests, blinds: &mut Blinds, receipt: Receipt, through: SessionTimeUs,
        certified_before: Option<SessionTimeUs>, final_boundary: bool,
    ) -> Result<(), ProbeError> {
        self.resolve_confirmed(requests);
        let observed = if context.visit().is_some_and(Visit::is_elevated) {
            receipt.clock.time(receipt.qpc)?.max(through)
        } else {
            through
        };
        loop {
            let ready = self.ready(certified_before, through);
            self.admit_outcomes(ready, None, receipt, output, blinds)?;
            match self.drain_confirmed_text(context, output, receipt, through, observed, certified_before, final_boundary)? {
                Some(ControlFlow::Continue(())) => continue,
                Some(ControlFlow::Break(())) => break,
                None => {}
            }
            let Some(at) = self.first_slot().filter(|at| *at <= through) else { break };
            self.flush_motion(at, false, receipt, output)?;
            let Front::Publish { approval, ime, into_redaction, cause } = self.decide_front(context, receipt, observed, at, final_boundary)
            else { break };
            match self.slots.pop_front().expect("gate has a front slot") {
                Slot::Boundary { at, kind, selection } => {
                    if is_closing(kind) {
                        self.suppression_log.end(at);
                        self.finish_redaction(Some(at));
                        self.flush_redactions(at, receipt, output)?;
                    }
                    self.close(at, kind, selection, receipt, output)?;
                }
                Slot::Synchronization { at, visit, held_modifiers, released_modifiers, edges } => {
                    self.publish_synchronization(at, visit, held_modifiers, released_modifiers, edges, context, output, receipt)?;
                }
                Slot::Input(classified) => match approval {
                    Ok((proof, focus)) => {
                        self.suppression_log.end(at);
                        self.publish(classified.input, proof, focus, ime, context, output, requests, receipt)?;
                    }
                    Err(class) => {
                        self.suppression_log.suppress(class, cause, at);
                        self.suppress_input(classified.input, class, into_redaction, at, context, requests);
                    }
                }
            }
        }
        if self.slots.is_empty() && self.overflow_at.is_some_and(|at| at <= through)
            && let Some(at) = self.overflow_at.take() {
            self.close(at, HeldBoundary::InputGap, Selection::All, receipt, output)?;
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
                    let publish_at = end.map_or(through, |at| at.max(output.published_or(receipt.boundary.time)));
                    self.flush_redactions(publish_at, receipt, output)?;
                    self.close(publish_at, HeldBoundary::FocusChanged, Selection::All, receipt, output)?;
                }
            }
        }
        if let Some(bound) = certified_before.or(final_boundary.then_some(through)) {
            let available = self.unresolved_before().map_or(bound, |pending| bound.min(pending)).min(through);
            self.flush_motion(available, final_boundary, receipt, output)?;
        }
        let ready = self.ready(certified_before, through);
        self.admit_outcomes(ready, final_boundary.then_some(through), receipt, output, blinds)?;
        // C5 (D-3 A): one row per ended suppression interval, with counts only.
        for interval in context.take_suppressions(ready, final_boundary.then_some(through)) {
            let Some(end) = interval.end else { continue };
            let at = output.raised(end);
            let row = json!({ "kind": "suppressed_interval", "reason": interval.reason,
                "start_t_us": interval.start, "end_t_us": end, "input_suppressed": interval.input,
                "admission_epoch": receipt.boundary.epoch });
            output.submit(at, row)?;
        }
        self.resolve_confirmed(requests);
        context.note_held_buttons(self.held.buttons_down());
        if let Some(published) = output.published() {
            context.prune_visits(published, receipt.clock, self.active_key_visit.and_then(|visit| visit.generation));
        }
        Ok(())
    }

    /// Decide the front confirmed text commit when it precedes every slot:
    /// none without one, `Break` while it waits for focus confirmation, and
    /// `Continue` once it was published or lost.
    fn drain_confirmed_text(
        &mut self, context: &mut Context<'_>, output: &mut InputWriter, receipt: Receipt, through: SessionTimeUs,
        observed: SessionTimeUs, certified_before: Option<SessionTimeUs>, final_boundary: bool,
    ) -> Result<Option<ControlFlow<()>>, ProbeError> {
        let Some(&(at, ref text)) = self.confirmed_text.front() else { return Ok(None) };
        if self.first_slot().is_some_and(|first| at >= first) || self.overflow_at.is_some_and(|overflow| at >= overflow) {
            return Ok(None);
        }
        if text.source_read.is_some() && let Some(last) = output.published().filter(|last| *last > at) {
            let (at, _) = self.confirmed_text.pop_front().expect("confirmed text was present");
            output.record_loss(text_loss(LossReason::LateCommit, at), last)?;
            return Ok(Some(ControlFlow::Continue(())));
        }
        let elapsed = observed.0.saturating_sub(at.0);
        if !final_boundary && elapsed < MAX_WAIT_US
            && (elapsed < FOCUS_CONFIRM_US || certified_before.is_none_or(|prefix| prefix <= at))
        {
            return Ok(Some(ControlFlow::Break(())));
        }
        let (at, text) = self.confirmed_text.pop_front().expect("confirmed text was present");
        self.flush_motion(at, false, receipt, output)?;
        let callback_fresh = text.source_read.is_none() || receipt.clock.time(text.callback_at)
            .is_ok_and(|callback| observed.0.saturating_sub(callback.0) <= MAX_WAIT_US);
        if at <= through && elapsed <= MAX_WAIT_US && callback_fresh
            && certified_before.is_some_and(|prefix| prefix > at) && context.confirm_text(&text)
        {
            let source = receipt.clock.time(text.callback_at).ok();
            let generation = Some(text.visit.generation);
            let commit = TextCommit {
                text: Some(text.text), provenance: TextOrigin::TextEditCompositionFinalized,
                status: if text.valid { TextStatus::Confirmed } else { TextStatus::Invalid },
            };
            self.flush_redactions(at, receipt, output)?;
            publish_text(output, at, receipt, generation, source, commit)?;
        } else {
            // A commit that cannot be confirmed in time is a typed loss (I7).
            self.note_loss(text_loss(LossReason::LateCommit, at));
        }
        Ok(Some(ControlFlow::Continue(())))
    }

    /// Whether the front slot, first received at `at`, is decided now. Only
    /// an input slot reads the approval; other slots never consult it.
    fn decide_front(
        &self, context: &mut Context<'_>, receipt: Receipt, observed: SessionTimeUs, at: SessionTimeUs, final_boundary: bool,
    ) -> Front {
        let mut ime = ImeState::Unknown;
        // A hook-only key with an uncertain focus joins an adjacent open redacted visit.
        let mut into_redaction = false;
        let Some(Slot::Input(classified)) = self.slots.front() else {
            return Front::Publish { approval: Err(SuppressionClass::ApprovalUnknown), ime, into_redaction, cause: None };
        };
        let correlated = &classified.input.normalized.correlated;
        // The development log's cause of a hook-only `ApprovalUnknown`.
        let mut cause = None;
        let mut approval = match classified.disposition {
            ControlDisposition::Recorder => Err(SuppressionClass::RecorderControl),
            ControlDisposition::Uncertain => Err(SuppressionClass::ControlUncertain),
            ControlDisposition::Application if hook_only(correlated) => {
                // Stage I3: approval from the visit timeline (§6.4).
                let qpc = correlated.first.packet.receipt.qpc;
                let key = matches!(correlated.first.action, Action::Key(_));
                match context.hook_receipt(qpc, receipt.clock) {
                    HookReceipt::Pending if !final_boundary && observed.0.saturating_sub(at.0) < MAX_WAIT_US => return Front::Wait,
                    HookReceipt::Covered(visit) => {
                        cause = Some(UnknownCause::HookApprovalFailed);
                        hook_approval(visit, qpc, context)
                    }
                    HookReceipt::FocusUncertain { before, after } if key => {
                        into_redaction = self.redactions.is_open() && self.active_key_visit
                            .and_then(|visit| visit.generation).is_some_and(|open| open == before || open == after.generation);
                        Err(SuppressionClass::FocusUncertain)
                    }
                    // A pointer record needs only a stable approved foreground.
                    HookReceipt::FocusUncertain { after, .. } => {
                        cause = Some(UnknownCause::HookApprovalFailed);
                        hook_approval(after, qpc, context).map(|(proof, _)| (proof, ReceiptFocus::Unknown))
                    }
                    HookReceipt::Pending => {
                        cause = Some(UnknownCause::HookPending);
                        Err(SuppressionClass::ApprovalUnknown)
                    }
                    HookReceipt::Unknown => {
                        cause = Some(UnknownCause::HookUnknown);
                        Err(SuppressionClass::ApprovalUnknown)
                    }
                }
            }
            ControlDisposition::Application => approved_receipt(&classified.input, context),
        };
        if let Ok((proof, focus)) = approval && let Action::Key(key) = correlated.first.action {
            let at = correlated.first.time.recv_t_us;
            let elapsed = observed.0.saturating_sub(at.0);
            if elapsed < FOCUS_CONFIRM_US || matches!(focus, ReceiptFocus::Pending(_)) && elapsed < MAX_WAIT_US {
                if !final_boundary {
                    return Front::Wait;
                }
                approval = Ok((proof, focus.visit().map_or(ReceiptFocus::Unknown, ReceiptFocus::Historical)));
            }
            let deadline = SessionTimeUs(at.0.saturating_add(MAX_WAIT_US));
            if key.down && key.virtual_key != VK_PACKET && focus.target(receipt.clock, deadline) == TextTarget::Editable
                && let Some(visit) = focus.visit()
            {
                if !final_boundary && elapsed < MAX_WAIT_US {
                    let Some(state) = context.ime_for_key(visit, correlated.first.id,
                        correlated.first.packet.receipt.qpc, std::time::Duration::from_micros(MAX_WAIT_US as u64))
                    else { return Front::Wait };
                    ime = state;
                }
            }
        }
        let cause = match approval {
            Err(SuppressionClass::ApprovalUnknown) if crate::devlog::ENABLED => cause.or_else(|| Some(raw_unknown_cause(&classified.input))),
            _ => None,
        };
        Front::Publish { approval, ime, into_redaction, cause }
    }

    /// A held-state synchronization slot: its synthetic edges are published
    /// when its visit is confirmed, keys only on a safe text target.
    fn publish_synchronization(
        &mut self, at: SessionTimeUs, visit: Visit, held_modifiers: u8, released_modifiers: u8,
        edges: Vec<SyntheticHeldEdge>, context: &mut Context<'_>, output: &mut InputWriter, receipt: Receipt,
    ) -> Result<(), ProbeError> {
        let mut witnessed = self.modifier_knowledge.awaiting_confirmation;
        self.modifier_knowledge = ModifierKnowledge::from_released(0);
        if !context.confirm_focus(visit) {
            self.suppress_at(SuppressionClass::UnconfirmedSynchronization, edges.len() as u64, at);
            return Ok(());
        }
        let deadline = SessionTimeUs(at.0.saturating_add(MAX_WAIT_US));
        let safe_target = context.approve_receipt(visit.receipt_proof(), visit.started_at, visit.started_at)
            .is_some_and(|focus| matches!(focus.target(receipt.clock, deadline),
                TextTarget::Editable | TextTarget::NonText | TextTarget::TypingUnknown));
        if safe_target {
            self.modifier_knowledge = ModifierKnowledge::from_released(released_modifiers);
        }
        let mut rows = Vec::with_capacity(edges.len() + 1);
        rows.push(json!({ "kind": "held_state_sync", "synthetic": true,
            "focus_generation": visit.generation, "admission_epoch": receipt.boundary.epoch }));
        for edge in edges {
            if !safe_target
                && matches!(edge.edge.identity.holding, Holding::Key { .. })
            {
                self.suppress_at(SuppressionClass::UnconfirmedSynchronization, 1, at);
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
        output.submit_boundary(at, rows)
    }

    /// An input slot suppressed by `class`; a key joins the open redacted
    /// visit when `into_redaction`.
    fn suppress_input(
        &mut self, input: HeldInput, class: SuppressionClass, into_redaction: bool, at: SessionTimeUs,
        context: &mut Context<'_>, requests: &mut ProvisionalRequests,
    ) {
        if !(into_redaction && self.redactions.join_open()) {
            self.suppress_at(class, 1, at);
        }
        if matches!(class, SuppressionClass::NotApproved | SuppressionClass::ApprovalUnknown) {
            context.count_suppressed(at);
        }
        if matches!(input.first().action, Action::Key(_)) {
            // Without an approved classification, even the affected side is private.
            self.modifier_knowledge.conceal(u8::MAX);
        }
        if let InputSample::Hid(sample) = &input.first().packet.sample {
            self.hid.reset(Some(sample.device));
        }
        requests.retire_input(input);
    }

    fn publish(
        &mut self, mut input: HeldInput, proof: ForegroundAtReceipt, focus: ReceiptFocus, ime: ImeState, context: &mut Context<'_>,
        output: &mut InputWriter, requests: &mut ProvisionalRequests, receipt: Receipt,
    ) -> Result<(), ProbeError> {
        let at = input.first().time.recv_t_us;
        let visit = VisitKey { proof, generation: focus.generation() };
        let report_scope = focus.visit().and_then(Visit::elevated_binding);
        let touchpad = self.touchpads.sole().filter(|_| clean_null_pointer(&input.normalized)).and_then(|device| {
            let contact = self.hid.overlapping_contact(device, at, proof)?;
            Some((device, json!({
                "report_reference": contact.reference,
                "receipt_gap_us": at.0.checked_sub(contact.at.0)?,
            })))
        });
        if let Some((device, _)) = &touchpad && let Some(details) = &mut input.normalized.pointer {
            details.points.origin = InputOrigin::Touchpad;
            details.points.device = Evidence::Observed(*device);
        }
        let first = input.first();
        let report_reference = matches!(&first.packet.sample, InputSample::Hid(_))
            .then(|| self.hid_reference(&first.packet, report_scope)).flatten();
        let approved_promotion = input.normalized.correlated.promotion.as_ref().filter(|link| {
            link.pen.foreground_at_receipt == Some(proof) && promotion_focus(link, proof, context)
        });
        let draft = match first.action {
            Action::Key(key) => self.key_row(&input, key, proof, focus, visit, ime, context, output, receipt)?,
            Action::Button { logical, down, .. } =>
                Some(self.button_row(&input, logical, down, proof, focus, approved_promotion.is_some(), context, receipt)?),
            Action::Motion(_) => self.motion_row(&input, proof, visit, requests),
            Action::Wheel { axis, value } => {
                let pointer = input.normalized.pointer;
                Some(RowDraft::new(json!({
                    "kind": "scroll", "axis": axis, "wheel": value,
                    "raw_wheel": pointer.as_ref().map(|pointer| pointer.raw_wheel),
                    "hook_wheel": pointer.as_ref().map(|pointer| pointer.hook_wheel),
                }), first))
            }
            Action::Hid => self.hid_row(&input, report_reference.as_deref(), proof),
            Action::AltGrCompanion => {
                // A key of a possibly sensitive visit: counted in the stream totals only.
                self.note_suppressed(SuppressionClass::NotPublishable, 1);
                None
            }
        };
        let Some(mut draft) = draft else {
            requests.retire_input(input);
            return Ok(());
        };
        self.flush_redactions(at, receipt, output)?;
        let mut row = std::mem::take(&mut draft.row);
        self.decorate(&mut row, &input, draft.source_time, proof, touchpad, approved_promotion, report_scope, receipt)?;
        let pointer = input.normalized.pointer;
        let geometry = pointer.map_or(first.geometry, |pointer| pointer.points.geometry);
        let kind = match first.action {
            Action::Motion(_) => Some(ProvisionalInputKind::PreMotion),
            Action::Button { down, .. } => Some(if down { ProvisionalInputKind::PreAction } else { ProvisionalInputKind::Next }),
            Action::Key(key) => Some(if key.down { ProvisionalInputKind::PreAction } else { ProvisionalInputKind::Next }),
            Action::Wheel { axis, .. } => Some(ProvisionalInputKind::Scroll { axis }),
            Action::Hid => first.pen_transition.map(|down| if down { ProvisionalInputKind::PreAction } else { ProvisionalInputKind::Next }),
            Action::AltGrCompanion => None,
        };
        let permit_observation = draft.permit_observation;
        let lacks_confirmation_capacity = permit_observation && kind.is_some() && self.in_flight.len() == MAX_CONFIRMATIONS;
        let (disposition, confirmation) = if permit_observation && let Some(kind) = kind
            && !lacks_confirmation_capacity
        {
            let observation = requests.choose(&input, geometry, proof, kind);
            let device = pointer.map_or(input.normalized.origin.device, |pointer| pointer.points.device).observed();
            let (disposition, event) = output.submit_confirmed(at, row)?;
            (disposition, Some(Confirmation { observation, geometry, kind, admission: first.admission, proof, device, at, event }))
        } else {
            (output.submit(at, row)?, None)
        };
        let capacity_gap = lacks_confirmation_capacity.then_some((geometry, kind));
        self.after_submit(disposition, draft, &input, visit, proof, report_reference, capacity_gap, context, output, requests, receipt)?;
        match (disposition, confirmation) {
            (Submit::Queued, Some(confirmation)) => {
                requests.retire_except(&input, confirmation.observation);
                self.in_flight.push_back(confirmation);
            }
            _ => requests.retire_input(input),
        }
        Ok(())
    }

    /// The row of a key, or none when its visit is redacted. A change of key
    /// visit first closes the held state of the previous one.
    fn key_row(
        &mut self, input: &HeldInput, key: Key, proof: ForegroundAtReceipt, focus: ReceiptFocus, visit: VisitKey,
        ime: ImeState, context: &mut Context<'_>, output: &mut InputWriter, receipt: Receipt,
    ) -> Result<Option<RowDraft>, ProbeError> {
        let normal = &input.normalized;
        let first = &normal.correlated.first;
        let at = first.time.recv_t_us;
        let deadline = SessionTimeUs(at.0.saturating_add(MAX_WAIT_US));
        let target = focus.target(receipt.clock, deadline);
        let target = if matches!(target, TextTarget::Editable | TextTarget::NonText | TextTarget::TypingUnknown)
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
                .max(output.published_or(receipt.boundary.time));
            self.close(public_at, HeldBoundary::FocusChanged, Selection::All, receipt, output)?;
        }
        self.active_key_visit = Some(visit);
        if matches!(target, TextTarget::Password | TextTarget::PossiblySensitive) {
            self.modifier_knowledge.conceal(u8::MAX);
            self.keyboard_text.reset();
            if key.down {
                let start = focus.visit().and_then(|visit| receipt.clock.time(visit.started_at).ok());
                self.redactions.count_key(start);
            }
            return Ok(None);
        }
        let published_press = input.edge.is_some_and(|edge| self.held.contains(edge));
        // Stage I4: a down is a press only when the key was privately
        // known up since the last boundary. Any other first down after
        // a boundary is not a press, a pre-action trigger or text input.
        let press = key.down && !published_press && matches!(input.repeat, Evidence::Observed(false));
        let unknown_press = key.down && !published_press && !press;
        let held_across = unknown_press && matches!(input.repeat, Evidence::Observed(true) | Evidence::Uncertain { .. });
        self.finish_redaction(None);
        let mut commits = Vec::new();
        // D-4c: a mixed field publishes keys with unknown typing and infers no text.
        let typing = if target == TextTarget::TypingUnknown || target == TextTarget::Editable && unknown_press {
            Typing::Unknown
        } else if target == TextTarget::Editable {
            if let Some(layout) = proof.focus.map(|focus| focus.keyboard_layout) {
                let text = self.keyboard_text.key(input, key, layout, ime);
                commits = text.commits;
                text.typing
            } else {
                Typing::Unknown
            }
        } else {
            Typing::NonText
        };
        // A text key whose text could not be inferred is written as unknown text (INP-3).
        let text_target = matches!(target, TextTarget::Editable | TextTarget::TypingUnknown);
        if text_target && key.down && matches!(typing, Typing::Unknown) && text_key(key.virtual_key) {
            commits.push(TextCommit::unknown());
        }
        let permit_observation = if text_target && key.down {
            let first_in_burst = self.last_text_key.replace((visit, at)).is_none_or(|(old, last)| {
                old != visit || at.0.saturating_sub(last.0) >= TEXT_BURST_US
            });
            press && first_in_burst
        } else {
            if key.down { target == TextTarget::NonText && press } else { published_press }
        };
        let (raw_input, keyboard_hook) = normal.correlated.keyboard_witnesses();
        let mut raw_input = json!(raw_input);
        let mut keyboard_hook = json!(keyboard_hook);
        if key.virtual_key == VK_PACKET {
            // VK_PACKET carries a UTF-16 unit in the scan field. Text
            // evidence belongs to the editable-only text_commit path.
            if raw_input.is_object() {
                raw_input["make_code"] = Value::Null;
            }
            if keyboard_hook.is_object() {
                keyboard_hook["scan_code"] = Value::Null;
            }
        }
        let knowledge = self.modifier_knowledge.after(input, key);
        let row = json!({
            "kind": if key.down { "key_down" } else { "key_up" },
            "vk": key.virtual_key, "scan": key.scan.map(ScanCode::folded),
            "scan_prefix": key.scan.map(|scan| scan.prefix), "key": normal.key_name,
            "raw_input": raw_input, "keyboard_hook": keyboard_hook,
            "mods": modifiers_in(input.held.modifiers & knowledge.known), "mods_known": modifiers_in(knowledge.known),
            "repeat": key.down.then_some(published_press).filter(|repeat| *repeat || press),
            "held_across_boundary": held_across.then_some(true),
            "press_unavailable": (!key.down && !published_press).then_some(true), "typing": typing,
            "ime": (target == TextTarget::Editable && key.down && key.virtual_key != VK_PACKET).then_some(ime),
            "layout": proof.focus.map(|focus| focus.keyboard_layout), "focus_generation": visit.generation,
        });
        Ok(Some(RowDraft {
            permit_observation, publish_edge: !unknown_press, modifier_knowledge: Some(knowledge), commits,
            ..RowDraft::new(row, first)
        }))
    }

    /// The row of a pointer button, with the element under a current down.
    fn button_row(
        &self, input: &HeldInput, logical: Button, down: bool, proof: ForegroundAtReceipt, focus: ReceiptFocus,
        promoted_link: bool, context: &mut Context<'_>, receipt: Receipt,
    ) -> Result<RowDraft, ProbeError> {
        let normal = &input.normalized;
        let first = &normal.correlated.first;
        let pointer = normal.pointer;
        let mut element = None;
        let points = pointer.as_ref().map(|pointer| pointer.points);
        if down && context.is_current(proof, focus.generation()) && let Some(points) = points
            && let (Evidence::Observed(point), Evidence::Observed(geometry)) = (points.event_position, points.geometry)
        {
            element = context.pointer_query(point, geometry, receipt.clock, receipt.qpc, Some(first.packet.receipt.qpc))?;
        }
        let promoted = normal.origin.origin == InputOrigin::PenPromoted;
        let published = input.edge.is_some_and(|edge| self.held.contains(edge));
        let row = json!({ "kind": if down { "pointer_down" } else { "pointer_up" }, "button": logical,
            "button_physical": pointer.as_ref().map(|p| p.physical_button),
            "pen_promotion": promoted.then_some(true),
            "press_unavailable": (!down && !published).then_some(true),
            "physical_press": promoted_link.then_some(false),
            "element": element });
        Ok(RowDraft {
            permit_observation: !promoted_link && input.edge.is_some() && down != published,
            ..RowDraft::new(row, first)
        })
    }

    /// The row of a cursor motion, or none when the motion is only counted
    /// in its second's `input_health` row.
    fn motion_row(
        &mut self, input: &HeldInput, proof: ForegroundAtReceipt, visit: VisitKey, requests: &mut ProvisionalRequests,
    ) -> Option<RowDraft> {
        let normal = &input.normalized;
        let first = &normal.correlated.first;
        let at = first.time.recv_t_us;
        let pointer = normal.pointer;
        let counts = MotionCounts::at(&mut self.motion, at);
        // Raw packets without a cursor sample are device-delta evidence only (stage I2).
        let evidence = if normal.cursor_motion { &normal.correlated.absorbed[..] } else { std::slice::from_ref(first) };
        for record in evidence {
            counts.raw_motion(record);
        }
        if !normal.cursor_motion {
            return None;
        }
        counts.coalesced = counts.coalesced.saturating_add(u64::from(
            pointer.map_or(0, |pointer| pointer.raw_packets.saturating_sub(1))));
        counts.thinned = counts.thinned.saturating_add(u64::from(input.motion_thinned));
        requests.pointer_moved(at, first.admission, proof, visit.generation, pointer.map(|pointer| pointer.points));
        let permit_observation = self.last_pointer_move.replace((visit, at)).is_none_or(|(previous, last)| {
            previous != visit || at.0.saturating_sub(last.0) >= POINTER_MOVE_OBSERVATION_US
        });
        if input.motion_thinned {
            return None;
        }
        Some(RowDraft { permit_observation, ..RowDraft::new(json!({ "kind": "pointer_move" }), first) })
    }

    /// The row of a HID report, or none when it is not publishable or thinned.
    fn hid_row(&mut self, input: &HeldInput, report_reference: Option<&str>, proof: ForegroundAtReceipt) -> Option<RowDraft> {
        let first = input.first();
        let at = first.time.recv_t_us;
        let InputSample::Hid(sample) = &first.packet.sample else {
            self.suppress_at(SuppressionClass::NotPublishable, 1, at);
            return None;
        };
        let source_time = self.hid.scan_time(sample, at);
        if input.motion_thinned {
            let counts = MotionCounts::at(&mut self.motion, at);
            counts.thinned = counts.thinned.saturating_add(1);
            return None;
        }
        let (mut row, active) = self.hid.row(sample, at, report_reference, proof, source_time);
        let mut permit_observation = true;
        if sample.is_pen() {
            let published_tip = input.edge.is_some_and(|edge| self.held.contains(edge));
            row["contact_transition"] = json!(first.pen_transition.filter(|down| *down || published_tip));
            row["position_at_receipt"] = json!(first.packet.position_at_receipt);
            row["position_at_receipt_geometry"] = json!(first.geometry);
            permit_observation = match first.pen_transition { Some(down) => down != published_tip, None => false };
        }
        Some(RowDraft { permit_observation, source_time, active_pad_contact: active, ..RowDraft::new(row, first) })
    }

    /// The envelope and the origin, device, correlation and pointer fields
    /// every input row carries.
    fn decorate(
        &self, row: &mut Value, input: &HeldInput, source_time: Option<SessionTimeUs>, proof: ForegroundAtReceipt,
        touchpad: Option<(DeviceId, Value)>, approved_promotion: Option<&PenPromotion>, report_scope: Option<VisitBinding>,
        receipt: Receipt,
    ) -> Result<(), ProbeError> {
        let normal = &input.normalized;
        let first = &normal.correlated.first;
        let at = first.time.recv_t_us;
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
        if normal.origin.origin == InputOrigin::Injected {
            let mut hints = self.injector_hints.clone();
            hints.extend(normal.origin.injector_hint.filter(|hint| !hints.contains(hint)));
            row["injector_hints"] = json!(hints);
        }
        row["window"] = json!(proof.window);
        if let Some((device, witness)) = touchpad {
            row["origin"] = json!(InputOrigin::Touchpad);
            row["device"] = json!(Evidence::Observed(device));
            // The held state and its synthetic edges keep the reporting device.
            row["source_device"] = json!(normal.origin.device);
            row["touchpad_contact_witness"] = witness;
        } else {
            row["origin"] = json!(normal.origin.origin);
            row["device"] = json!(normal.origin.device);
        }
        if matches!(normal.origin.origin, InputOrigin::PenPromoted | InputOrigin::TouchPromoted) {
            row["cursor_id"] = json!(normal.origin.cursor_id);
        }
        row["correlation"] = json!(normal.correlated.correlation);
        if let Some(cluster) = normal.correlated.cluster {
            row["cluster_raw"] = json!(cluster.raw);
            row["cluster_hooks"] = json!(cluster.hooks);
        }
        // A correlated group has at most one hook. Prefer its tick without
        // replacing the earlier logical receipt that orders the group.
        let tick_witness = if matches!(first.packet.sample, InputSample::RawKey(_) | InputSample::RawPointer(_)) {
            normal.correlated.partners.iter().find(|slot| matches!(slot.packet.sample,
                InputSample::HookKey(_) | InputSample::HookPointer(_)) && slot.time.coarse.is_some()).unwrap_or(first)
        } else {
            first
        };
        let coarse = tick_witness.time.coarse;
        let late = coarse.filter(|_| !injected_hook(&tick_witness.packet.sample))
            .and_then(|tick| clock::message_tick_estimate(receipt.clock, tick_witness.tick_anchor, tick))
            .map(|estimate| estimate.is_late(at));
        RowEnvelope::new(RecordTime { source_t_us: source_time, coarse, ..first.time }, late, first.admission)
            .write(row).map_err(|_| unavailable("Encode the input row envelope"))?;
        if let Some(pointer) = normal.pointer {
            // `held` and `mods` are the published state after this row (INP-5).
            let (modifiers, buttons) = match input.edge {
                Some(edge) => {
                    let mut after = self.held.clone();
                    after.published(edge);
                    after.modifiers_and_buttons()
                }
                None => self.held.modifiers_and_buttons(),
            };
            let points = pointer.points;
            row["pos"] = json!(points.event_position);
            row["pos_recv"] = json!(points.receipt_position);
            row["monitor"] = json!(points.monitor);
            row["held"] = json!(buttons_in(buttons));
            row["mods"] = json!(modifiers_in(modifiers));
            row["delta"] = json!(pointer.raw_relative);
            row["coalesced"] = json!(pointer.raw_packets.saturating_sub(1));
            row["warp"] = json!(pointer.warp);
            row["geometry"] = json!(points.geometry);
            row["pos_receipt_t_us"] = json!(points.event_position_receipt_t_us);
            row["pos_recv_receipt_t_us"] = json!(points.receipt_position_t_us);
            row["raw_absolute"] = json!(pointer.raw_absolute);
            row["raw_absolute_xy"] = json!(pointer.raw_absolute_packet.map(|(xy, _)| xy));
            row["raw_move_flags"] = json!(pointer.raw_absolute_packet.map(|(_, flags)| flags));
            row["raw_packets"] = json!(pointer.raw_packets);
        }
        Ok(())
    }

    /// Apply a submitted row: the state it publishes once queued, or the
    /// state it leaves unknown once dropped. `capacity_gap` is the geometry
    /// and kind of an approved observation the gate had no capacity to confirm.
    fn after_submit(
        &mut self, disposition: Submit, draft: RowDraft, input: &HeldInput, visit: VisitKey, proof: ForegroundAtReceipt,
        report_reference: Option<String>, capacity_gap: Option<(Evidence<GeometryId>, Option<ProvisionalInputKind>)>,
        context: &mut Context<'_>, output: &mut InputWriter, requests: &mut ProvisionalRequests, receipt: Receipt,
    ) -> Result<(), ProbeError> {
        let first = input.first();
        let at = first.time.recv_t_us;
        match disposition {
            Submit::Queued => {
                if let Some(knowledge) = draft.modifier_knowledge {
                    self.modifier_knowledge = knowledge;
                }
                requests.activity(at);
                if let Some((geometry, kind)) = capacity_gap {
                    let observation = kind.and_then(|kind| requests.choose(input, geometry, proof, kind));
                    requests.approved_observation_gap(at, geometry, kind, observation);
                }
                if draft.publish_edge && let Some(edge) = input.edge {
                    self.held.published(edge);
                }
                if let Some(generation) = visit.generation {
                    context.note_approved_input(generation, first.packet.receipt.qpc);
                }
                if let InputSample::Hid(sample) = &first.packet.sample {
                    if sample.is_touchpad() {
                        self.hid.observe_touchpad(sample.device, at, proof, report_reference.as_deref(), draft.active_pad_contact);
                    }
                    self.hid.observe_published_ndof(sample, at, report_reference, proof);
                    self.pen.observe(sample);
                }
                for commit in draft.commits {
                    publish_text(output, at, receipt, visit.generation, None, commit)?;
                }
            }
            Submit::Dropped => {
                if draft.modifier_knowledge.is_some() {
                    self.conceal_modifier(input);
                }
                self.keyboard_text.reset();
                if let InputSample::Hid(sample) = &first.packet.sample {
                    self.hid.reset(Some(sample.device));
                }
            }
        }
        Ok(())
    }

    fn conceal_modifier(&mut self, input: &HeldInput) {
        if let Action::Key(key) = input.first().action {
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
                requests.resolve(id);
            }
        }
    }

    fn finish_redaction(&mut self, end: Option<SessionTimeUs>) {
        if self.redactions.finish(end) {
            self.overflow = true;
        }
    }

    fn flush_redactions(&mut self, at: SessionTimeUs, receipt: Receipt, output: &mut InputWriter) -> Result<(), ProbeError> {
        for visit in self.redactions.completed.drain(..) {
            let row = json!({ "kind": "redacted", "field_start_t_us": visit.start, "field_end_t_us": visit.end,
                "key_count": visit.count, "admission_epoch": receipt.boundary.epoch });
            output.submit(at, row)?;
        }
        Ok(())
    }

    fn flush_motion(
        &mut self, through: SessionTimeUs, final_boundary: bool, receipt: Receipt, output: &mut InputWriter,
    ) -> Result<(), ProbeError> {
        let Some(counts) = &self.motion else { return Ok(()) };
        let end = SessionTimeUs(counts.second.saturating_add(1).saturating_mul(1_000_000));
        if !final_boundary && end > through {
            return Ok(());
        }
        let counts = self.motion.take().expect("motion bucket was present");
        if counts.coalesced == 0 && counts.thinned == 0 && counts.suppressed.is_empty() && counts.raw_motion.is_empty() {
            return Ok(());
        }
        let at = end.min(through).max(output.published_or(receipt.boundary.time));
        let row = json!({ "kind": "input_health", "session_second": counts.second,
            "raw_packets_coalesced": counts.coalesced, "motion_rows_thinned": counts.thinned,
            "suppressed": counts.suppressed,
            "raw_motion": counts.raw_motion.iter().map(|(device, (packets, x, y))| {
                json!({ "device": device, "packets": packets, "delta": [x, y] })
            }).collect::<Vec<_>>(),
            "admission_epoch": receipt.boundary.epoch });
        output.submit(at, row)?;
        Ok(())
    }

    fn close(
        &mut self, at: SessionTimeUs, kind: HeldBoundary, selection: Selection, receipt: Receipt, output: &mut InputWriter,
    ) -> Result<(), ProbeError> {
        let published = output.published_or(receipt.boundary.time);
        let at = if matches!(kind, HeldBoundary::InputGap | HeldBoundary::HookUnavailable
            | HeldBoundary::DeviceRemoved | HeldBoundary::RecorderControl | HeldBoundary::HidReportUnavailable) {
            published
        } else {
            at.max(published)
        };
        // Losses are typed `gap` rows; hook loss and device removal are blind
        // intervals written by the input owner. A boundary adds no marker row.
        if kind == HeldBoundary::FocusChanged || is_closing(kind) {
            if let Some(commit) = self.keyboard_text.finish_packet() {
                publish_text(output, at, receipt, self.active_key_visit.and_then(|visit| visit.generation), None, commit)?;
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
            output.submit_boundary(at, edges.into_iter().map(|edge| held_row(edge, receipt)).collect())?;
        } else {
            for edge in edges {
                output.submit(at, held_row(edge, receipt))?;
            }
        }
        Ok(())
    }
}

/// Publish a text commit at `at` in the input order.
fn publish_text(
    output: &mut InputWriter, at: SessionTimeUs, receipt: Receipt, generation: Option<FocusGeneration>,
    provider_receipt: Option<SessionTimeUs>, commit: TextCommit,
) -> Result<(), ProbeError> {
    let row = json!({
        "kind": "text_commit", "text": commit.text, "status": commit.status, "provenance": commit.provenance,
        "focus_generation": generation, "admission_epoch": receipt.boundary.epoch,
        "text_edit_receipt_t_us": provider_receipt,
    });
    output.submit(at, row)?;
    Ok(())
}

fn clean_null_pointer(input: &crate::capture::input::NormalizedInput) -> bool {
    if input.origin.origin != InputOrigin::Unknown || input.correlated.correlation != Correlation::Matched
        || input.pointer.is_none()
    {
        return false;
    }
    let mut raw = false;
    let mut hook = false;
    for slot in input.correlated.slots() {
        match &slot.packet.sample {
            InputSample::RawPointer(pointer) if pointer.device.is_none()
                && !has_promotion(u64::from(pointer.extra_information)) => raw = true,
            InputSample::HookPointer(pointer) if !injected_hook(&slot.packet.sample)
                && !has_promotion(pointer.extra_information) => hook = true,
            _ => return false,
        }
    }
    raw && hook
}

/// A row without raw or promotion evidence: hook records only. Its approval
/// comes from the visit timeline (stage I3, §6.4).
fn hook_only(input: &CorrelatedInput) -> bool {
    input.partners.is_empty() && input.promotion.is_none()
        && matches!(input.first.packet.sample, InputSample::HookKey(_) | InputSample::HookPointer(_))
}

/// The receipt approval of a hook-only record covered by `visit`.
fn hook_approval(visit: Visit, qpc: QpcTick, context: &mut Context<'_>) -> Approval {
    let proof = visit.receipt_proof();
    context.approve_receipt(proof, qpc, qpc).map(|focus| (proof, focus)).ok_or(SuppressionClass::ApprovalUnknown)
}

/// Publication depends on approval and privacy only; correlation quality is
/// row metadata. A record with raw evidence uses its raw foreground proof.
fn approved_receipt(input: &HeldInput, context: &mut Context<'_>) -> Approval {
    let unknown = SuppressionClass::ApprovalUnknown;
    let input = &input.normalized.correlated;
    let slots = || input.slots();
    let producer = Producer::RawInput;
    let first_raw = slots().find(|slot| slot.packet.sample.producer() == producer);
    let promotion_pen = input.promotion.as_ref().map(|link| link.pen.as_ref());
    let proof = first_raw.map(|slot| slot.packet.as_ref()).or(promotion_pen)
        .and_then(|packet| packet.foreground_at_receipt).ok_or(unknown)?;
    if !slots().filter(|slot| slot.packet.sample.producer() == producer)
        .all(|slot| slot.packet.foreground_at_receipt == Some(proof))
    {
        return Err(unknown);
    }
    let borrowed_pen_qpc = first_raw.is_none().then(|| promotion_pen.map(|pen| pen.receipt.qpc)).flatten();
    let times = || slots().map(|slot| slot.packet.receipt.qpc).chain(borrowed_pen_qpc);
    let first = times().min().ok_or(unknown)?;
    let last = times().max().ok_or(unknown)?;
    let focus = context.approve_receipt(proof, last, first).ok_or(SuppressionClass::NotApproved)?;
    // A pointer or other non-key record needs only the approved process in
    // the foreground at its receipt, as in the hook-only branch. A key keeps
    // requiring a known focus when a hook record is part of it. A historical
    // focus is an ended visit of the receipt's own proof that covered the
    // whole record; a pointer row writes only `proof.window`, and its element
    // query requires the current visit.
    let uses_hook = slots().any(|slot| slot.packet.sample.producer() != producer);
    let key = matches!(input.first.action, Action::Key(_) | Action::AltGrCompanion)
        || slots().any(|slot| is_key(&slot.packet.sample));
    if key && uses_hook && matches!(focus, ReceiptFocus::Unknown | ReceiptFocus::Historical(_)) {
        return Err(unknown);
    }
    Ok((proof, focus))
}

/// Why `approved_receipt` returned `ApprovalUnknown`, for the development
/// log. It repeats that function's checks without consulting the context;
/// past them only the hook focus check of a key returns that class.
fn raw_unknown_cause(input: &HeldInput) -> UnknownCause {
    let input = &input.normalized.correlated;
    let producer = Producer::RawInput;
    let first_raw = input.slots().find(|slot| slot.packet.sample.producer() == producer);
    let promotion_pen = input.promotion.as_ref().map(|link| link.pen.as_ref());
    let Some(proof) = first_raw.map(|slot| slot.packet.as_ref()).or(promotion_pen).and_then(|packet| packet.foreground_at_receipt)
    else { return UnknownCause::NoForegroundProof };
    if !input.slots().filter(|slot| slot.packet.sample.producer() == producer)
        .all(|slot| slot.packet.foreground_at_receipt == Some(proof))
    {
        return UnknownCause::MixedForeground;
    }
    UnknownCause::HookWithUncertainFocus
}

fn promotion_focus(link: &PenPromotion, proof: ForegroundAtReceipt, context: &mut Context<'_>) -> bool {
    let pen = link.pen.receipt.qpc;
    let pointer = link.pointer.receipt.qpc;
    context.approve_receipt(proof, pen.max(pointer), pen.min(pointer))
        .is_some_and(|focus| !matches!(focus, ReceiptFocus::Unknown | ReceiptFocus::Historical(_)))
}

fn reference_key() -> Option<[u8; 32]> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

    let mut key = [0; 32];
    // SAFETY: `key` is a writable buffer of the passed length that outlives
    // the call; a null algorithm handle is valid with the system RNG flag.
    let status = unsafe {
        BCryptGenRandom(std::ptr::null_mut(), key.as_mut_ptr(), key.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    (status == 0).then_some(key)
}

/// One text-edit commit lost at `at`.
fn text_loss(reason: LossReason, at: SessionTimeUs) -> LossEvidence {
    LossEvidence::ordinary(reason, Some(LossProducer::TextEdit), std::num::NonZeroU64::MIN, at, at)
}

fn is_closing(reason: HeldBoundary) -> bool {
    matches!(reason, HeldBoundary::AdmissionClosed | HeldBoundary::Pause | HeldBoundary::Stop)
}

/// A synthetic edge carries the key, button and device fields of an
/// ordinary row of its kind (INP-13).
fn held_row(edge: SyntheticHeldEdge, receipt: Receipt) -> Value {
    let down = edge.edge.down;
    let identity = edge.edge.identity;
    let device = Evidence::from_option(identity.device, UnavailableReason::NotObserved);
    let mut row = json!({
        "synthetic": true, "boundary": edge.reason, "device": device, "admission_epoch": receipt.boundary.epoch,
    });
    let pointer = if down { "pointer_down" } else { "pointer_up" };
    match identity.holding {
        Holding::Key { vk, scan, .. } => {
            row["kind"] = json!(if down { "key_down" } else { "key_up" });
            row["vk"] = json!(vk);
            row["scan"] = json!(scan.map(ScanCode::folded));
            row["scan_prefix"] = json!(scan.map(|scan| scan.prefix));
            row["raw_input"] = Value::Null;
            row["keyboard_hook"] = Value::Null;
            row["mods"] = json!([]);
            row["mods_known"] = json!([]);
        }
        Holding::Button { logical, physical } => {
            row["kind"] = json!(pointer);
            row["button"] = json!(logical);
            row["button_physical"] = json!(Evidence::from_option(physical, UnavailableReason::NotObserved));
        }
        Holding::PromotedPenPointer { logical, cursor_id } => {
            row["kind"] = json!(pointer);
            row["button"] = json!(logical);
            row["pen_promotion"] = json!(true);
            row["cursor_id"] = json!(Evidence::Observed(cursor_id));
        }
        Holding::PenTip { link } => {
            row["kind"] = json!(if down { "pen_down" } else { "pen_up" });
            row["link"] = json!(link);
        }
    }
    row
}

/// A log-only sub-reason of `SuppressionClass::ApprovalUnknown`; the
/// serialized class is unchanged.
#[derive(Clone, Copy, Debug)]
enum UnknownCause {
    NoForegroundProof,
    MixedForeground,
    HookPending,
    HookUnknown,
    HookApprovalFailed,
    HookWithUncertainFocus,
}

impl UnknownCause {
    const ALL: [Self; 6] = [
        Self::NoForegroundProof, Self::MixedForeground, Self::HookPending, Self::HookUnknown,
        Self::HookApprovalFailed, Self::HookWithUncertainFocus,
    ];
}

type CauseCounts = [u64; UnknownCause::ALL.len()];

fn format_causes(counts: &CauseCounts) -> String {
    UnknownCause::ALL.iter().zip(counts).filter(|(_, count)| **count != 0)
        .map(|(cause, count)| format!("{cause:?}={count}")).collect::<Vec<_>>().join(" ")
}

/// A run of suppressed input slots, ended by a published input or a closing boundary.
struct SuppressedRun {
    start: SessionTimeUs,
    classes: BTreeMap<SuppressionClass, u64>,
    causes: CauseCounts,
}

/// Development log of input suppression: per-cause `ApprovalUnknown`
/// counts in at most one summary line per second, and a line at the start
/// and the end of each suppressed run. It never changes a decision.
#[derive(Default)]
struct SuppressionLog {
    /// Counts since the last summary line.
    pending: CauseCounts,
    summarized: Option<SessionTimeUs>,
    run: Option<SuppressedRun>,
}

impl SuppressionLog {
    const SUMMARY_US: i64 = 1_000_000;

    fn suppress(&mut self, class: SuppressionClass, cause: Option<UnknownCause>, at: SessionTimeUs) {
        if !crate::devlog::ENABLED {
            return;
        }
        let run = self.run.get_or_insert_with(|| {
            devlog!("gate", "suppressed interval started at_us={} class={class:?} cause={cause:?}", at.0);
            SuppressedRun { start: at, classes: BTreeMap::new(), causes: CauseCounts::default() }
        });
        *run.classes.entry(class).or_default() += 1;
        let Some(cause) = cause else { return };
        run.causes[cause as usize] += 1;
        self.pending[cause as usize] += 1;
        if self.summarized.is_none_or(|last| at.0.saturating_sub(last.0) >= Self::SUMMARY_US) {
            devlog!("gate", "approval unknown at_us={} {}", at.0, format_causes(&self.pending));
            self.pending = CauseCounts::default();
            self.summarized = Some(at);
        }
    }

    fn end(&mut self, at: SessionTimeUs) {
        if let Some(run) = self.run.take() {
            devlog!("gate", "suppressed interval ended at_us={} duration_us={} classes={:?} approval_unknown=[{}]",
                at.0, at.0.saturating_sub(run.start.0), run.classes, format_causes(&run.causes));
        }
    }
}
