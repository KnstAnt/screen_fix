use std::{collections::{BTreeMap, BTreeSet}, sync::Arc};

use crate::{
    capture::{DropReason, observation::{ProvisionalInputKind, ProvisionalInputObservation}},
    protocol::{
        AdmissionEpoch, DeviceId, Evidence, GeometryId, MessageTickAnchor, ObservationId, PhysicalPoint, QpcTick, RecordTime, RequestId,
        SessionTimeUs, SourceSequence, UnavailableReason,
        ipc::{CollectedInput, HookKey, InputSample, Producer, RawKey, Receipt},
    },
};

use super::{Action, Button, HeldSynchronization, Motion, PendingInput, ScanCode, normalize};

pub(crate) const MATCH_WINDOW_US: i64 = 20_000;
const MAX_PENDING: usize = 1_024;
const MAX_MOTION_GROUP: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Correlation {
    Matched,
    MissingScan,
    Ambiguous,
    Unmatched,
    Discontinuous,
}

pub(crate) struct CorrelatedInput {
    pub first: PendingInput,
    pub partners: Vec<PendingInput>,
    pub correlation: Correlation,
    pub promotion: Option<PenPromotion>,
}

impl CorrelatedInput {
    /// A logical key has one source sample and at most one opposite-provider match.
    pub fn keyboard_witnesses(&self) -> (Option<RawKey>, Option<HookKey>) {
        let mut raw_input = None;
        let mut keyboard_hook = None;
        for slot in std::iter::once(&self.first).chain(&self.partners) {
            match slot.packet.sample {
                InputSample::RawKey(key) => raw_input = Some(key),
                InputSample::HookKey(key) => keyboard_hook = Some(key),
                _ => {}
            }
        }
        (raw_input, keyboard_hook)
    }
}

#[derive(Clone)]
pub(crate) struct PenPromotion {
    pub pen: Arc<CollectedInput>,
    pub pointer: Arc<CollectedInput>,
    pub gap_us: i64,
}


#[derive(Debug, Clone, Copy)]
pub(crate) struct InputGap {
    pub producer: Producer,
    pub first: SessionTimeUs,
    pub last: SessionTimeUs,
    pub native: Option<NativeLoss>,
    pub logical_slots: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeLoss {
    pub first_sequence: SourceSequence,
    pub last_sequence: SourceSequence,
    pub admitted_count: Option<u64>,
}

impl InputGap {
    pub fn merge(self, other: Self) -> Self {
        let native = match (self.native, other.native) {
            (Some(left), Some(right)) => Some(NativeLoss {
                first_sequence: left.first_sequence.min(right.first_sequence),
                last_sequence: left.last_sequence.max(right.last_sequence),
                admitted_count: left.admitted_count.zip(right.admitted_count).map(|(a, b)| a.saturating_add(b)),
            }),
            (left, right) => left.or(right),
        };
        Self {
            first: self.first.min(other.first), last: self.last.max(other.last),
            native, logical_slots: self.logical_slots.saturating_add(other.logical_slots),
            ..self
        }
    }
}

pub(crate) enum InputStep {
    Record(CorrelatedInput),
    Gap(InputGap),
    DeviceRemoved { id: DeviceId, at: SessionTimeUs },
    Synchronize { state: HeldSynchronization, at: SessionTimeUs },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Order {
    time: SessionTimeUs,
    qpc: i64,
    producer: usize,
    source: SourceSequence,
    part: u8,
}

pub(crate) fn producer_index(producer: Producer) -> usize {
    match producer {
        Producer::RawInput => 0,
        Producer::DeviceNotifications => 1,
        Producer::KeyboardHook => 2,
        Producer::PointerHook => 3,
    }
}

/// Owns the bounded local native input prefix. All records strictly before
/// the minimum certified producer receipt bound are known or represented by gaps.
pub(crate) struct InputMatcher {
    slots: BTreeMap<Order, PendingInput>,
    ambiguous: BTreeSet<Order>,
    removals: BTreeMap<Order, DeviceId>,
    synchronization: Option<(Order, HeldSynchronization)>,
    gaps: [Option<InputGap>; 4],
    gap_barrier: Option<SessionTimeUs>,
    bounds: [Option<SessionTimeUs>; 4],
    next_id: u64,
    pen_tips: BTreeMap<(DeviceId, u16), bool>,
    pen_links: BTreeMap<Order, PenPromotion>,
    pen_ambiguous: BTreeSet<Order>,
    pen_uncertain: BTreeMap<DeviceId, SessionTimeUs>,
}

impl InputMatcher {
    pub fn new() -> Self {
        Self {
            slots: BTreeMap::new(), ambiguous: BTreeSet::new(), removals: BTreeMap::new(), synchronization: None, gaps: [None; 4], gap_barrier: None,
            bounds: [None; 4], next_id: 0, pen_tips: BTreeMap::new(), pen_links: BTreeMap::new(), pen_ambiguous: BTreeSet::new(), pen_uncertain: BTreeMap::new(),
        }
    }

    pub fn reopen(&mut self) {
        *self = Self { next_id: self.next_id, ..Self::new() };
    }

    pub fn progress(&mut self, producer: Producer, before: SessionTimeUs) {
        self.bounds[producer_index(producer)] = Some(before);
    }

    pub fn producer_bound(&self, producer: Producer) -> Option<SessionTimeUs> {
        self.bounds[producer_index(producer)]
    }

    pub fn gap(&mut self, gap: InputGap) {
        self.pen_tips.clear();
        self.pen_links.clear();
        let pending = &mut self.gaps[producer_index(gap.producer)];
        *pending = Some(pending.map_or(gap, |old| old.merge(gap)));
    }

    pub fn remove_device(&mut self, id: DeviceId, receipt: Receipt, time: SessionTimeUs) {
        self.pen_tips.retain(|(device, _), _| *device != id);
        self.pen_uncertain.remove(&id);
        self.pen_links.clear();
        let producer = Producer::DeviceNotifications;
        if self.removals.len() == crate::protocol::ipc::MAX_INPUT_DEVICES {
            self.gap(InputGap { producer, first: time, last: time, native: None, logical_slots: 1 });
            return;
        }
        self.removals.insert(Order {
            time, qpc: receipt.qpc.0, producer: producer_index(producer), source: receipt.sequence, part: 0,
        }, id);
    }

    pub fn stage_synchronization(&mut self, qpc: QpcTick, at: SessionTimeUs, state: HeldSynchronization) {
        let order = Order { time: at, qpc: qpc.0, producer: 4, source: SourceSequence(0), part: 0 };
        self.synchronization = Some((order, state));
    }

    pub fn discard_stale_synchronization(&mut self, through: SessionTimeUs) {
        if self.synchronization.is_some_and(|(order, _)| order.time <= through) {
            self.synchronization = None;
        }
    }

    /// The provisional creator runs on the actual receipt path before a
    /// matching-confidence, thinning, hotkey, or approval decision.
    pub fn push(
        &mut self,
        packet: CollectedInput,
        admission: AdmissionEpoch,
        time: RecordTime,
        tick_anchor: Option<MessageTickAnchor>,
        fallback_scan: impl FnOnce(u16) -> Option<ScanCode>,
        pointer_geometry: impl Fn(PhysicalPoint) -> Evidence<GeometryId>,
        window_geometry: Evidence<GeometryId>,
        mut stage: impl FnMut(ProvisionalInputChange),
    ) -> Result<(), InputIdExhausted> {
        let producer = packet.sample.producer();
        let parts = normalize::parts(&packet, fallback_scan);
        if let InputSample::Hid(report) = &packet.sample && report.pen_continuity_uncertain() {
            self.pen_tips.retain(|(device, _), _| *device != report.device);
            self.pen_uncertain.retain(|_, last| time.recv_t_us.0.saturating_sub(last.0) <= 2 * MATCH_WINDOW_US);
            if self.pen_uncertain.len() < MAX_PENDING || self.pen_uncertain.contains_key(&report.device) {
                self.pen_uncertain.insert(report.device, time.recv_t_us);
            } else {
                self.invalidate_through(time.recv_t_us);
            }
        }
        let pen_transition = if let InputSample::Hid(report) = &packet.sample
            && let Some((link, down)) = report.pen_tip()
        {
            let key = (report.device, link);
            if self.pen_tips.len() >= MAX_PENDING && !self.pen_tips.contains_key(&key) {
                self.pen_tips.clear();
            }
            self.pen_tips.insert(key, down).filter(|prior| *prior != down).map(|_| down)
        } else {
            None
        };
        let device = match &packet.sample {
            InputSample::RawKey(key) => key.device,
            InputSample::RawPointer(pointer) => pointer.device,
            InputSample::Hid(report) => Some(report.device),
            _ => None,
        };
        let packet = Arc::new(packet);
        let direct_point = match packet.sample {
            InputSample::HookPointer(hook) => Some(hook.point),
            _ => packet.position_at_receipt,
        };
        for (part, action) in parts.into_iter().enumerate().filter_map(|(part, action)| {
            action.map(|action| (part as u8, action))
        }) {
            self.next_id = self.next_id.checked_add(1).filter(|id| *id < (1 << 63)).ok_or(InputIdExhausted)?;
            let id = RequestId(self.next_id);
            let kind = match action {
                Action::Motion(_) => Some(ProvisionalInputKind::PreMotion),
                Action::Hid if pen_transition == Some(true) => Some(ProvisionalInputKind::PreAction),
                Action::Key(key) if key.down => Some(ProvisionalInputKind::PreAction),
                Action::Button { down: true, .. } => Some(ProvisionalInputKind::PreAction),
                Action::Wheel { axis, .. } => Some(ProvisionalInputKind::Scroll { axis }),
                Action::Button { down: false, .. } | Action::Key(_) => Some(ProvisionalInputKind::Next),
                Action::Hid if pen_transition == Some(false) => Some(ProvisionalInputKind::Next),
                _ => None,
            };
            let geometry = if matches!(action, Action::Key(_)) {
                window_geometry
            } else {
                direct_point.map_or(Evidence::Unavailable(UnavailableReason::NotObserved), &pointer_geometry)
            };
            let observation = kind.map(|kind| ProvisionalInputObservation {
                id: ObservationId(id.0),
                admission,
                time,
                kind,
                foreground_at_receipt: packet.foreground_at_receipt,
                producer,
                source_sequence: packet.receipt.sequence,
                packet_part: part,
                geometry,
                device,
                position: match (&packet.sample, direct_point) {
                    (InputSample::HookPointer(_), Some(point)) => Evidence::Observed(point),
                    (_, Some(value)) => Evidence::Uncertain { value, reason: UnavailableReason::NotObserved },
                    _ => Evidence::Unavailable(UnavailableReason::NotObserved),
                },
            });
            if let Some(observation) = observation {
                stage(ProvisionalInputChange::Opened(observation));
            }
            if self.slots.len() == MAX_PENDING {
                if let Some(observation) = observation {
                    stage(ProvisionalInputChange::Dropped(observation.id, DropReason::Capacity));
                }
                self.gap(InputGap { producer, first: time.recv_t_us, last: time.recv_t_us, native: None, logical_slots: 1 });
                continue;
            }
            let order = Order {
                time: time.recv_t_us,
                qpc: packet.receipt.qpc.0,
                producer: producer_index(producer),
                source: packet.receipt.sequence,
                part,
            };
            self.slots.insert(order, PendingInput {
                id, admission, time, tick_anchor, packet: Arc::clone(&packet), part, action, geometry,
                observation: observation.map(|item| item.id), pen_transition,
            });
        }
        Ok(())
    }

    pub fn ready(&mut self, admitted_through: SessionTimeUs) -> Option<InputStep> {
        let bound = self.producer_floor()?.min(admitted_through);
        self.take(bound, Some(bound), false)
    }

    pub fn finish(&mut self, cutoff: SessionTimeUs) -> Option<InputStep> {
        self.take(cutoff, self.producer_floor().map(|bound| bound.min(cutoff)), true)
    }

    pub fn certified_before(&self) -> Option<SessionTimeUs> {
        self.producer_floor()
    }

    pub fn invalidate_through(&mut self, through: SessionTimeUs) {
        self.pen_tips.clear();
        self.pen_links.clear();
        self.gap_barrier = Some(self.gap_barrier.map_or(through, |previous| previous.max(through)));
    }

    pub fn discard(&mut self) -> impl Iterator<Item = PendingInput> + '_ {
        self.gaps.fill(None);
        self.pen_links.clear();
        self.pen_ambiguous.clear();
        self.ambiguous.clear();
        self.removals.clear();
        self.synchronization = None;
        std::mem::take(&mut self.slots).into_values()
    }

    fn producer_floor(&self) -> Option<SessionTimeUs> {
        let [raw, devices, keys, pointer] = self.bounds;
        Some(raw?.min(devices?).min(keys?).min(pointer?))
    }

    pub fn unresolved_before(&self) -> Option<SessionTimeUs> {
        let producer = self.producer_floor()?;
        let first = self.first_item().map(|order| order.time);
        [Some(producer), first, self.first_gap().map(|gap| gap.first)].into_iter().flatten().min()
    }

    fn first_gap(&self) -> Option<InputGap> {
        self.gaps.iter().flatten().copied().min_by_key(|gap| gap.first)
    }

    fn first_item(&self) -> Option<Order> {
        [self.slots.first_key_value().map(|(order, _)| *order),
            self.removals.first_key_value().map(|(order, _)| *order),
            self.synchronization.map(|(order, _)| order)].into_iter().flatten().min()
    }

    fn take(&mut self, bound: SessionTimeUs, evidence: Option<SessionTimeUs>, closing: bool) -> Option<InputStep> {
        let first = self.first_item();
        if let Some(gap) = self.first_gap()
            && first.is_none_or(|order| gap.first <= order.time)
            && (gap.first < bound || closing && gap.first == bound)
        {
            self.gaps[producer_index(gap.producer)] = None;
            self.invalidate_through(gap.last);
            return Some(InputStep::Gap(gap));
        }
        let order = first?;
        if self.synchronization.is_some_and(|(pending, _)| pending == order) {
            if order.time < bound || closing && order.time == bound {
                let (_, state) = self.synchronization.take()?;
                return Some(InputStep::Synchronize { state, at: order.time });
            }
            return None;
        }
        if self.removals.contains_key(&order) {
            if order.time < bound || closing && order.time == bound {
                let id = self.removals.remove(&order)?;
                self.gap_barrier = Some(self.gap_barrier.map_or(order.time, |previous| previous.max(order.time)));
                return Some(InputStep::DeviceRemoved { id, at: order.time });
            }
            return None;
        }
        let horizon = if matches!(self.slots[&order].action, Action::Motion(_)) {
            // Raw A → hook A → raw B → hook B. Seeing past the last
            // potential hook is required before assigning all raw deltas.
            3 * MATCH_WINDOW_US
        } else {
            2 * MATCH_WINDOW_US
        };
        let certified = evidence.is_some_and(|floor| order.time.0.saturating_add(horizon) < floor.0);
        if closing {
            if order.time > bound {
                return None;
            }
        } else if !certified {
            return None;
        }
        let (correlation, candidates) = self.correlate(order, certified);
        if correlation == Correlation::Ambiguous {
            let affected: BTreeSet<_> = self.candidates(order).flat_map(|candidate| {
                std::iter::once(candidate).chain(self.candidates(candidate))
            }).collect();
            self.ambiguous.extend(affected);
        }
        self.ambiguous.remove(&order);
        let promotion = if matches!(correlation, Correlation::Matched | Correlation::Unmatched) {
            std::iter::once(order).chain(candidates.iter().copied()).find_map(|slot| {
                self.pen_links.remove(&slot).or_else(|| self.link_pen(slot))
            })
        } else {
            self.pen_links.remove(&order);
            None
        };
        self.pen_ambiguous.remove(&order);
        let first = self.slots.remove(&order)?;
        let partners = candidates.into_iter().filter_map(|other| {
            self.pen_ambiguous.remove(&other);
            self.slots.remove(&other)
        }).collect();
        Some(InputStep::Record(CorrelatedInput { first, partners, correlation, promotion }))
    }

    fn link_pen(&mut self, order: Order) -> Option<PenPromotion> {
        let endpoint = &self.slots[&order];
        let (direction, is_pen) = match (endpoint.pen_transition, promoted_tip(endpoint)) {
            (Some(down), _) => (down, true),
            (None, Some(down)) => (down, false),
            _ => return None,
        };
        let peers: Vec<_> = self.pen_peers(order, direction, is_pen).collect();
        let &[peer] = peers.as_slice() else {
            if !peers.is_empty() {
                self.pen_ambiguous.extend(peers);
            }
            return None;
        };
        let reciprocal: Vec<_> = self.pen_peers(peer, direction, !is_pen).collect();
        if reciprocal.as_slice() != [order] {
            // A retired alternative must never turn a rival's later
            // lookup into a falsely unique association.
            self.pen_ambiguous.insert(peer);
            self.pen_ambiguous.extend(reciprocal);
            return None;
        }
        if self.pen_ambiguous.contains(&order) || self.pen_ambiguous.contains(&peer) {
            self.pen_ambiguous.insert(peer);
            return None;
        }
        let (pen, pointer) = if is_pen { (order, peer) } else { (peer, order) };
        // If the hook itself cannot be independently normalized, it cannot
        // make a certain cross-provider association.
        let (pointer_confidence, raw_partners) = self.correlate(pointer, true);
        if !matches!(pointer_confidence, Correlation::Matched | Correlation::Unmatched) {
            return None;
        }
        let pen_packet = &self.slots[&pen].packet;
        let InputSample::Hid(report) = &pen_packet.sample else { return None };
        let InputSample::HookPointer(hook) = self.slots[&pointer].packet.sample else { return None };
        if self.pen_uncertain.get(&report.device).is_some_and(|at| {
            at.0.abs_diff(pen.time.0) <= MATCH_WINDOW_US as u64
        }) || raw_partners.iter().any(|peer| {
            let raw_packet = &self.slots[peer].packet;
            raw_packet.foreground_at_receipt != pen_packet.foreground_at_receipt
                || matches!(raw_packet.sample, InputSample::RawPointer(raw)
                    if promoted(u64::from(raw.extra_information))
                        && raw.extra_information as u8 != hook.extra_information as u8)
        }) {
            return None;
        }
        let link = PenPromotion {
            pen: Arc::clone(&self.slots[&pen].packet),
            pointer: Arc::clone(&self.slots[&pointer].packet),
            gap_us: pointer.time.0.saturating_sub(pen.time.0),
        };
        self.pen_links.insert(peer, link.clone());
        Some(link)
    }

    fn pen_peers(&self, order: Order, down: bool, seek_pointer: bool) -> impl Iterator<Item = Order> + '_ {
        let low = lower_order(SessionTimeUs(order.time.0.saturating_sub(MATCH_WINDOW_US)));
        let high = lower_order(SessionTimeUs(order.time.0.saturating_add(MATCH_WINDOW_US).saturating_add(1)));
        self.slots.range(low..high).filter_map(move |(key, candidate)| {
            let matches = if seek_pointer {
                promoted_tip(candidate)
            } else {
                candidate.pen_transition
            };
            (matches == Some(down) && candidate.admission == self.slots[&order].admission
                && !self.crosses_gap(key.time)).then_some(*key)
        })
    }

    fn correlate(&mut self, order: Order, certified: bool) -> (Correlation, Vec<Order>) {
        if self.ambiguous.contains(&order) {
            return (Correlation::Ambiguous, Vec::new());
        }
        if self.crosses_gap(order.time) {
            return (Correlation::Discontinuous, Vec::new());
        }
        if !certified {
            return (Correlation::Discontinuous, Vec::new());
        }
        let candidate = self.candidates(order).collect::<Vec<_>>();
        if candidate.is_empty() {
            return (Correlation::Unmatched, Vec::new());
        }
        if matches!(self.slots[&order].action, Action::Motion(_)) {
            return self.motion(order, candidate);
        }
        let other = candidate[0];
        if candidate.len() != 1 || self.candidates(other).count() != 1 || self.ambiguous.contains(&other) {
            return (Correlation::Ambiguous, Vec::new());
        }
        let crossings = self.crossings(order, other);
        if !crossings.is_empty() {
            self.ambiguous.extend(crossings);
            return (Correlation::Ambiguous, Vec::new());
        }
        let confidence = match (self.slots[&order].action, self.slots[&other].action) {
            (Action::Key(a), Action::Key(b)) if a.virtual_key != 0xe7 && (a.scan.is_none() || b.scan.is_none()) => Correlation::MissingScan,
            _ => Correlation::Matched,
        };
        (confidence, candidate)
    }

    fn candidates(&self, order: Order) -> impl Iterator<Item = Order> + '_ {
        let low = lower_order(SessionTimeUs(order.time.0.saturating_sub(MATCH_WINDOW_US)));
        let high = lower_order(SessionTimeUs(order.time.0.saturating_add(MATCH_WINDOW_US).saturating_add(1)));
        let input = &self.slots[&order];
        self.slots.range(low..high).filter_map(move |(key, other)| {
            (input.admission == other.admission && compatible(input, other)
                && !self.crosses_gap(key.time)).then_some(*key)
        })
    }

    fn crosses_gap(&self, time: SessionTimeUs) -> bool {
        let lower = time.0.saturating_sub(MATCH_WINDOW_US);
        let upper = time.0.saturating_add(MATCH_WINDOW_US);
        self.gap_barrier.is_some_and(|last| lower <= last.0)
            || self.gaps.iter().flatten().any(|gap| upper >= gap.first.0 && lower <= gap.last.0)
            || self.removals.range(lower_order(SessionTimeUs(lower))..lower_order(SessionTimeUs(upper.saturating_add(1))))
                .next().is_some()
    }

    fn crossings(&self, first: Order, partner: Order) -> Vec<Order> {
        let (raw, hook) = if first.producer == producer_index(Producer::RawInput) {
            (first, partner)
        } else {
            (partner, first)
        };
        let low = lower_order(SessionTimeUs(hook.time.0.saturating_sub(MATCH_WINDOW_US)));
        let high = lower_order(SessionTimeUs(hook.time.0.saturating_add(MATCH_WINDOW_US).saturating_add(1)));
        let mut affected = BTreeSet::new();
        for (other_raw, candidate) in self.slots.range(low..high) {
            if other_raw.producer != raw.producer || *other_raw == raw
                || !same_order_class(candidate.action, self.slots[&raw].action)
            {
                continue;
            }
            for other_hook in self.candidates(*other_raw) {
                if other_hook != hook && other_raw.cmp(&raw) != other_hook.cmp(&hook) {
                    affected.insert(*other_raw);
                    affected.insert(other_hook);
                }
            }
        }
        affected.into_iter().collect()
    }

    fn motion(&self, order: Order, candidates: Vec<Order>) -> (Correlation, Vec<Order>) {
        let (hook, mut raw) = if order.producer == producer_index(Producer::PointerHook) {
            (order, candidates)
        } else if candidates.len() == 1 {
            (candidates[0], self.candidates(candidates[0]).collect())
        } else {
            return (Correlation::Ambiguous, Vec::new());
        };
        // Never attach the same relative delta to several positions. An
        // absolute packet may be paired but cannot be summed with other data.
        if raw.len() > MAX_MOTION_GROUP
            || self.ambiguous.contains(&hook)
            || raw.iter().any(|candidate| self.ambiguous.contains(candidate) || self.candidates(*candidate).count() != 1)
            || (raw.len() > 1 && !same_relative_device(&self.slots, &raw))
        {
            return (Correlation::Ambiguous, Vec::new());
        }
        raw.push(hook);
        raw.retain(|candidate| *candidate != order);
        raw.sort_unstable();
        (Correlation::Matched, raw)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputIdExhausted;

pub(crate) enum ProvisionalInputChange {
    Opened(ProvisionalInputObservation),
    Dropped(ObservationId, DropReason),
}

fn promoted_tip(input: &PendingInput) -> Option<bool> {
    let InputSample::HookPointer(hook) = input.packet.sample else { return None };
    if !promoted(hook.extra_information) || hook.extra_information & 0x80 != 0 {
        return None;
    }
    match input.action {
        Action::Button { logical: Button::Left, down, .. } => Some(down),
        _ => None,
    }
}

fn lower_order(time: SessionTimeUs) -> Order {
    Order { time, qpc: i64::MIN, producer: 0, source: SourceSequence(0), part: 0 }
}

fn distinct_producers(first: &PendingInput, other: &PendingInput) -> bool {
    matches!(
        (first.packet.sample.producer(), other.packet.sample.producer()),
        (Producer::RawInput, Producer::KeyboardHook | Producer::PointerHook)
            | (Producer::KeyboardHook | Producer::PointerHook, Producer::RawInput)
    )
}

fn compatible(first: &PendingInput, other: &PendingInput) -> bool {
    if !distinct_producers(first, other) {
        return false;
    }
    match (&first.packet.sample, &other.packet.sample) {
        (InputSample::RawKey(raw), InputSample::HookKey(hook)) | (InputSample::HookKey(hook), InputSample::RawKey(raw))
            if raw.virtual_key == 0xe7 || hook.virtual_key == 0xe7 =>
        {
            // A packet code unit is not a physical scan code. Match only
            // positively injected evidence with an identical native packet.
            return raw.virtual_key == 0xe7 && hook.virtual_key == 0xe7
                && raw.device.is_none() && hook.flags & 0x12 != 0
                && u32::from(raw.make_code) == hook.scan_code && (raw.flags & 1 != 0) == (hook.flags & 0x80 != 0);
        }
        _ if unqualified(&first.packet.sample) || unqualified(&other.packet.sample) => return false,
        _ => {}
    }
    match (first.action, other.action) {
        (Action::Key(left), Action::Key(right)) => left.compatible(right),
        (Action::Button { logical: left, down: a, .. }, Action::Button { logical: right, down: b, .. }) => {
            left == right && a == b
        }
        (Action::Wheel { axis: a, value: left }, Action::Wheel { axis: b, value: right }) => a == b && left == right,
        (Action::Motion(_), Action::Motion(_)) => true,
        _ => false,
    }
}

fn unqualified(sample: &InputSample) -> bool {
    match sample {
        InputSample::HookKey(key) => key.flags & 0x12 != 0 || key.virtual_key == 0xe7,
        InputSample::HookPointer(pointer) => pointer.flags & 3 != 0 && !promoted(pointer.extra_information),
        _ => false,
    }
}

pub(crate) fn promoted(extra: u64) -> bool {
    extra & 0xffff_ff00 == 0xff51_5700
}

fn same_order_class(a: Action, b: Action) -> bool {
    matches!((a, b),
        (Action::Key(_), Action::Key(_))
        | (Action::Button { .. } | Action::Wheel { .. }, Action::Button { .. } | Action::Wheel { .. }))
}

fn same_relative_device(slots: &BTreeMap<Order, PendingInput>, raw: &[Order]) -> bool {
    let first = &slots[&raw[0]];
    let InputSample::RawPointer(first_pointer) = first.packet.sample else { return false };
    first_pointer.device.is_some() && first.packet.foreground_at_receipt.is_some()
        && raw.iter().all(|candidate| {
            let input = &slots[candidate];
            matches!(input.action, Action::Motion(Motion::Relative { .. }))
                && matches!(input.packet.sample, InputSample::RawPointer(raw) if raw.device == first_pointer.device)
                && input.packet.foreground_at_receipt == first.packet.foreground_at_receipt
        })
}
