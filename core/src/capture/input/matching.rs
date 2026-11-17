use std::{collections::{BTreeMap, BTreeSet}, sync::Arc};

use crate::{
    capture::observation::{ProvisionalInputKind, ProvisionalInputObservation},
    package::outcome::{LossCount, LossEvidence, LossRange, LossReason, LossScope},
    protocol::{
        AdmissionEpoch, DeviceId, Evidence, GeometryId, MessageTickAnchor, ObservationId, PhysicalPoint, QpcTick, RecordTime, RequestId,
        SessionTimeUs, SourceSequence, UnavailableReason,
        ipc::{CollectedInput, HookKey, InputSample, Producer, RawKey, Receipt},
    },
};

use super::{Action, Button, HeldSynchronization, Motion, PendingInput, ScanCode, WheelAxis, normalize};

pub(crate) const MATCH_WINDOW_US: i64 = 20_000;
/// A raw motion packet belongs to the earliest hook sample within the window,
/// which takes every packet within its own window: a decision sees one more
/// window past that hook (stage I2).
const MOTION_HORIZON_US: i64 = 2 * MATCH_WINDOW_US;
const MAX_PENDING: usize = 1_024;
/// A wheel run is decided after this many records, as if it ended there, so a
/// continuously spinning wheel cannot hold the bounded prefix.
const MAX_WHEEL_RUN: usize = 32;

/// Correlation quality is evidence metadata on a row. It never decides
/// whether a transition is published (remediation plan §6.3, stage I1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Correlation {
    Matched,
    MissingScan,
    Ambiguous,
    Unmatched,
    /// A candidate window crosses a recorded gap, barrier or device removal.
    Discontinuous,
}

/// The connected component of an ambiguous key or button row: one row per raw
/// record, with every hook of the component consumed by it. An ambiguous wheel
/// run instead has one row per hook record and consumes its raw records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cluster {
    pub raw: u32,
    pub hooks: u32,
    /// Every hook carries an injection flag and every raw record has a NULL
    /// device handle (INP-7 precedence for the whole component).
    pub injected: bool,
    /// Hooks beyond the raw record count. Only the component's first row
    /// carries them, so they are counted once.
    pub excess_hooks: u32,
}

pub(crate) struct CorrelatedInput {
    pub first: PendingInput,
    pub partners: Vec<PendingInput>,
    pub correlation: Correlation,
    pub promotion: Option<PenPromotion>,
    pub cluster: Option<Cluster>,
    /// Records consumed by this row without a row of their own: the hooks of
    /// an ambiguous key or button cluster, the raw records of an ambiguous
    /// wheel run, or raw motion packets that a hook sample could not take
    /// (raw-motion evidence). They never enter the hook-only path.
    pub absorbed: Vec<PendingInput>,
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

    /// Every record this row accounts for: its own and the absorbed hooks.
    pub fn records(&self) -> impl Iterator<Item = &PendingInput> {
        std::iter::once(&self.first).chain(&self.partners).chain(&self.absorbed)
    }

    pub fn into_records(self) -> impl Iterator<Item = PendingInput> {
        std::iter::once(self.first).chain(self.partners).chain(self.absorbed)
    }
}

#[derive(Clone)]
pub(crate) struct PenPromotion {
    pub pen: Arc<CollectedInput>,
    pub pointer: Arc<CollectedInput>,
    pub gap_us: i64,
}

/// A typed, ranged loss of one producer's records. It both delimits the
/// matcher's continuity intervals and becomes one input `gap` ledger entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InputGap {
    pub producer: Producer,
    pub reason: LossReason,
    pub first: SessionTimeUs,
    pub last: SessionTimeUs,
    pub count: LossCount,
    pub source_seq: Option<[u64; 2]>,
    pub qpc: Option<[QpcTick; 2]>,
}

impl InputGap {
    /// One logical record lost locally at `at`.
    pub fn logical(producer: Producer, reason: LossReason, at: SessionTimeUs) -> Self {
        Self {
            producer, reason, first: at, last: at, count: LossCount::Known(std::num::NonZeroU64::MIN),
            source_seq: None, qpc: None,
        }
    }

    /// Merge a loss with the same producer and reason.
    pub fn merge(self, other: Self) -> Self {
        Self {
            first: self.first.min(other.first), last: self.last.max(other.last),
            count: self.count.merge(other.count),
            source_seq: merge_bounds(self.source_seq, other.source_seq),
            qpc: merge_bounds(self.qpc, other.qpc),
            ..self
        }
    }

    pub fn evidence(&self) -> LossEvidence {
        LossEvidence {
            reason: self.reason, producer: Some(self.producer.into()), count: self.count,
            scope: LossScope::Ordinary(LossRange {
                first_t_us: self.first, last_t_us: self.last, source_seq: self.source_seq, qpc: self.qpc,
            }),
        }
    }
}

pub(crate) struct Discarded {
    pub pending: Vec<PendingInput>,
    pub decided: Vec<CorrelatedInput>,
    pub gaps: Vec<InputGap>,
}

pub(crate) enum InputStep {
    Record(CorrelatedInput),
    Gap(InputGap),
    DeviceRemoved { id: DeviceId, at: SessionTimeUs },
    Synchronize { state: HeldSynchronization, at: SessionTimeUs },
    /// Admitted records at a closing boundary whose candidate windows no
    /// producer progress covers. They are resolved as `producer_stall` (§6.3)
    /// over a range that starts at the last producer progress, `since`.
    Undecidable { records: Vec<PendingInput>, since: SessionTimeUs },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Order {
    time: SessionTimeUs,
    qpc: i64,
    producer: usize,
    source: SourceSequence,
    part: u8,
}

const PRODUCERS: usize = Producer::ALL.len();

/// Raw Input records carry the receipt-time foreground proof; edges join
/// them only with hook records.
fn raw_side(order: Order) -> bool {
    Producer::ALL.get(order.producer).is_some_and(|producer| producer.raw_input())
}

/// When a record may be decided.
#[derive(Clone, Copy)]
enum Decision {
    /// Every producer receipt strictly below this bound is known.
    Ready(SessionTimeUs),
    /// A closing boundary admits records at or below `limit`. A record is
    /// decided when every producer has closed or progressed past
    /// `min(t + window, limit)`; otherwise it is undecidable.
    Closing(SessionTimeUs),
}

/// Owns the bounded local native input prefix. All records strictly before
/// the minimum certified producer receipt bound are known or represented by gaps.
pub(crate) struct InputMatcher {
    slots: BTreeMap<Order, PendingInput>,
    /// Rows of decided components that wait for earlier records.
    decided: BTreeMap<Order, CorrelatedInput>,
    removals: BTreeMap<Order, DeviceId>,
    synchronization: Option<(Order, HeldSynchronization)>,
    gaps: BTreeMap<(usize, LossReason), InputGap>,
    gap_barrier: Option<SessionTimeUs>,
    bounds: [Option<SessionTimeUs>; PRODUCERS],
    /// Producers whose final progress was received: no later record can arrive.
    closed: bool,
    next_id: u64,
    pen_tips: BTreeMap<(DeviceId, u16), bool>,
    pen_links: BTreeMap<Order, PenPromotion>,
    pen_ambiguous: BTreeSet<Order>,
    pen_uncertain: BTreeMap<DeviceId, SessionTimeUs>,
}

impl InputMatcher {
    pub fn new() -> Self {
        Self {
            slots: BTreeMap::new(), decided: BTreeMap::new(), removals: BTreeMap::new(),
            synchronization: None, gaps: BTreeMap::new(), gap_barrier: None, bounds: [None; PRODUCERS], closed: false,
            next_id: 0, pen_tips: BTreeMap::new(), pen_links: BTreeMap::new(), pen_ambiguous: BTreeSet::new(),
            pen_uncertain: BTreeMap::new(),
        }
    }

    pub fn reopen(&mut self) {
        *self = Self { next_id: self.next_id, ..Self::new() };
    }

    /// Producer progress, or coverage of its receipts by a recorded blind
    /// interval. Bounds never move backward.
    pub fn progress(&mut self, producer: Producer, before: SessionTimeUs) {
        let bound = &mut self.bounds[producer.index()];
        *bound = Some(bound.map_or(before, |previous| previous.max(before)));
    }

    /// Every producer has delivered its final records: the helper drained and
    /// exited normally. Decisions after this are exact (§6.3).
    pub fn close_producers(&mut self) {
        self.closed = true;
    }

    pub fn gap(&mut self, gap: InputGap) {
        self.pen_tips.clear();
        self.pen_links.clear();
        self.gaps.entry((gap.producer.index(), gap.reason))
            .and_modify(|pending| *pending = pending.merge(gap))
            .or_insert(gap);
    }

    pub fn remove_device(&mut self, id: DeviceId, receipt: Receipt, time: SessionTimeUs) {
        self.pen_tips.retain(|(device, _), _| *device != id);
        self.pen_uncertain.remove(&id);
        self.pen_links.clear();
        let producer = Producer::DeviceNotifications;
        if self.removals.len() == crate::protocol::ipc::MAX_INPUT_DEVICES {
            self.gap(InputGap::logical(producer, LossReason::MatcherCapacity, time));
            return;
        }
        self.removals.insert(Order {
            time, qpc: receipt.qpc.0, producer: producer.index(), source: receipt.sequence, part: 0,
        }, id);
    }

    pub fn stage_synchronization(&mut self, qpc: QpcTick, at: SessionTimeUs, state: HeldSynchronization) {
        let order = Order { time: at, qpc: qpc.0, producer: PRODUCERS, source: SourceSequence(0), part: 0 };
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
                    stage(ProvisionalInputChange::Dropped(observation.id));
                }
                self.gap(InputGap::logical(producer, LossReason::MatcherCapacity, time.recv_t_us));
                continue;
            }
            let order = Order {
                time: time.recv_t_us,
                qpc: packet.receipt.qpc.0,
                producer: producer.index(),
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
        self.take(Decision::Ready(bound))
    }

    /// Decide records at or below `limit` at a closing boundary (§6.3). Missing
    /// partners are treated as at expiry; records whose windows no producer
    /// progress covers are returned once as `Undecidable`.
    pub fn finish(&mut self, limit: SessionTimeUs) -> Option<InputStep> {
        self.take(Decision::Closing(limit))
    }

    pub fn certified_before(&self) -> Option<SessionTimeUs> {
        self.producer_floor()
    }

    pub fn invalidate_through(&mut self, through: SessionTimeUs) {
        self.pen_tips.clear();
        self.pen_links.clear();
        self.gap_barrier = Some(self.gap_barrier.map_or(through, |previous| previous.max(through)));
    }

    /// Every undecided record, the rows decided but not yet taken, and the
    /// gaps not yet taken. The caller accounts the admitted ones.
    pub fn discard(&mut self) -> Discarded {
        self.pen_links.clear();
        self.pen_ambiguous.clear();
        self.removals.clear();
        self.synchronization = None;
        Discarded {
            pending: std::mem::take(&mut self.slots).into_values().collect(),
            decided: std::mem::take(&mut self.decided).into_values().collect(),
            gaps: std::mem::take(&mut self.gaps).into_values().collect(),
        }
    }

    fn producer_floor(&self) -> Option<SessionTimeUs> {
        if self.closed {
            return Some(SessionTimeUs(i64::MAX));
        }
        self.bounds.into_iter().try_fold(SessionTimeUs(i64::MAX), |floor, bound| Some(floor.min(bound?)))
    }

    pub fn unresolved_before(&self) -> Option<SessionTimeUs> {
        let producer = self.producer_floor()?;
        let first = self.first_item().map(|order| order.time);
        [Some(producer), first, self.first_gap().map(|gap| gap.first)].into_iter().flatten().min()
    }

    fn first_gap(&self) -> Option<InputGap> {
        self.gaps.values().copied().min_by_key(|gap| gap.first)
    }

    fn first_item(&self) -> Option<Order> {
        [self.slots.first_key_value().map(|(order, _)| *order),
            self.decided.first_key_value().map(|(order, _)| *order),
            self.removals.first_key_value().map(|(order, _)| *order),
            self.synchronization.map(|(order, _)| order)].into_iter().flatten().min()
    }

    fn take(&mut self, decision: Decision) -> Option<InputStep> {
        let due = |time: SessionTimeUs| match decision {
            Decision::Ready(bound) => time < bound,
            Decision::Closing(limit) => time <= limit,
        };
        loop {
            let first = self.first_item();
            if let Some(gap) = self.first_gap()
                && first.is_none_or(|order| gap.first <= order.time)
                && due(gap.first)
            {
                self.gaps.remove(&(gap.producer.index(), gap.reason));
                // An optional HID loss does not interrupt keyboard and pointer continuity.
                if gap.producer != Producer::OptionalHid {
                    self.invalidate_through(gap.last);
                }
                return Some(InputStep::Gap(gap));
            }
            let order = first?;
            if self.synchronization.is_some_and(|(pending, _)| pending == order) {
                if !due(order.time) { return None; }
                let (_, state) = self.synchronization.take()?;
                return Some(InputStep::Synchronize { state, at: order.time });
            }
            if self.removals.contains_key(&order) {
                if !due(order.time) { return None; }
                let id = self.removals.remove(&order)?;
                self.gap_barrier = Some(self.gap_barrier.map_or(order.time, |previous| previous.max(order.time)));
                return Some(InputStep::DeviceRemoved { id, at: order.time });
            }
            // Rows decided while open may lie past a later closing limit; they
            // stay for the caller's discard like any record outside admission.
            let limit = match decision {
                Decision::Closing(limit) if order.time > limit => return None,
                Decision::Closing(limit) => Some(limit),
                Decision::Ready(_) => None,
            };
            if let Some(row) = self.decided.remove(&order) {
                return Some(InputStep::Record(row));
            }
            match self.slots.get(&order)?.action {
                Action::Motion(_) => {
                    let through = SessionTimeUs(order.time.0.saturating_add(MOTION_HORIZON_US));
                    if !self.covers(through, decision) {
                        return self.undecidable(decision);
                    }
                    self.decide_motion(order, limit);
                    continue;
                }
                Action::Wheel { axis, .. } => {
                    let run = self.wheel_run(order, axis, limit);
                    let last = run.last().copied().unwrap_or(order);
                    if !self.covers(SessionTimeUs(last.time.0.saturating_add(MATCH_WINDOW_US)), decision) {
                        return self.undecidable(decision);
                    }
                    self.decide_wheel(run);
                    continue;
                }
                _ => {}
            }
            let members = self.component(order, limit);
            let latest = members.last().copied().unwrap_or(order);
            if !self.covers(SessionTimeUs(latest.time.0.saturating_add(MATCH_WINDOW_US)), decision) {
                return self.undecidable(decision);
            }
            self.decide(members);
        }
    }

    /// Every receipt at or below `through` (capped at a closing limit) is known.
    fn covers(&self, through: SessionTimeUs, decision: Decision) -> bool {
        match decision {
            Decision::Ready(bound) => through < bound,
            Decision::Closing(limit) => self.producer_floor().is_some_and(|floor| through.min(limit) < floor),
        }
    }

    /// While open, an undecided record waits. At a closing boundary the first
    /// undecidable record makes every later admitted record undecidable too,
    /// because the required coverage only grows with receipt time.
    fn undecidable(&mut self, decision: Decision) -> Option<InputStep> {
        let Decision::Closing(limit) = decision else { return None };
        let stalled: Vec<_> = self.slots.range(..lower_order(SessionTimeUs(limit.0.saturating_add(1))))
            .map(|(order, _)| *order).collect();
        // The missing range starts at the last progress that every producer delivered.
        let first = stalled.first().map(|order| order.time).unwrap_or(limit);
        let since = self.producer_floor().map_or(first, |floor| floor.min(first));
        let records = stalled.into_iter().filter_map(|order| self.take_slot(order)).collect();
        Some(InputStep::Undecidable { records, since })
    }

    fn take_slot(&mut self, order: Order) -> Option<PendingInput> {
        self.pen_ambiguous.remove(&order);
        self.slots.remove(&order)
    }

    /// The connected component of the raw/hook compatibility graph that
    /// contains `start`. A closing boundary excludes records past its limit.
    fn component(&self, start: Order, limit: Option<SessionTimeUs>) -> BTreeSet<Order> {
        let mut members = BTreeSet::from([start]);
        let mut queue = vec![start];
        while let Some(node) = queue.pop() {
            for other in self.candidates(node) {
                if limit.is_none_or(|limit| other.time <= limit) && members.insert(other) {
                    queue.push(other);
                }
            }
        }
        members
    }

    /// Form the rows of one decided key or button component. Edges only
    /// join raw and hook records, so a component without one side is a single record.
    fn decide(&mut self, members: BTreeSet<Order>) {
        let (raws, hooks): (Vec<Order>, Vec<Order>) = members.iter().partition(|order| raw_side(**order));
        match (raws.as_slice(), hooks.as_slice()) {
            (&[raw], &[hook]) if !self.crosses_pair(raw, hook) => {
                let discontinuous = self.crosses_gap(raw.time) || self.crosses_gap(hook.time);
                let correlation = if discontinuous {
                    Correlation::Discontinuous
                } else {
                    match (self.slots[&raw].action, self.slots[&hook].action) {
                        (Action::Key(a), Action::Key(b)) if a.virtual_key != 0xe7 && (a.scan.is_none() || b.scan.is_none()) =>
                            Correlation::MissingScan,
                        _ => Correlation::Matched,
                    }
                };
                let (first, other) = if raw < hook { (raw, hook) } else { (hook, raw) };
                let promotion = self.promotion(&[first, other], correlation);
                let (Some(first_input), Some(partner)) = (self.take_slot(first), self.take_slot(other)) else { return };
                self.decided.insert(first, CorrelatedInput {
                    first: first_input, partners: vec![partner], correlation, promotion, cluster: None, absorbed: Vec::new(),
                });
            }
            ([], _) | (_, []) => {
                for order in members {
                    self.unpaired(order);
                }
            }
            _ => {
                let injected = hooks.iter().all(|hook| injected_hook(&self.slots[hook].packet.sample))
                    && raws.iter().all(|raw| null_handle(&self.slots[raw].packet.sample));
                let raw_count = u32::try_from(raws.len()).unwrap_or(u32::MAX);
                let hook_count = u32::try_from(hooks.len()).unwrap_or(u32::MAX);
                let mut absorbed: Vec<_> = hooks.iter().filter_map(|hook| {
                    self.pen_links.remove(hook);
                    self.take_slot(*hook)
                }).collect();
                for (index, raw) in raws.into_iter().enumerate() {
                    self.pen_links.remove(&raw);
                    let Some(first) = self.take_slot(raw) else { continue };
                    let lead = index == 0;
                    self.decided.insert(raw, CorrelatedInput {
                        first, partners: Vec::new(), correlation: Correlation::Ambiguous, promotion: None,
                        cluster: Some(Cluster {
                            raw: raw_count, hooks: hook_count, injected,
                            excess_hooks: if lead { hook_count.saturating_sub(raw_count) } else { 0 },
                        }),
                        absorbed: if lead { std::mem::take(&mut absorbed) } else { Vec::new() },
                    });
                }
            }
        }
    }

    /// A record without a partner becomes its own row. A hook record here is
    /// in the hook-only state (stage I3).
    fn unpaired(&mut self, order: Order) {
        let correlation = if self.crosses_gap(order.time) { Correlation::Discontinuous } else { Correlation::Unmatched };
        let promotion = self.promotion(&[order], correlation);
        let Some(first) = self.take_slot(order) else { return };
        self.decided.insert(order, CorrelatedInput {
            first, partners: Vec::new(), correlation, promotion, cluster: None, absorbed: Vec::new(),
        });
    }

    /// Hook-anchored motion association (stage I2, D-7 A). A raw packet
    /// belongs to the earliest hook sample within its match window, and a hook
    /// sample takes every such packet not yet attributed. The relative deltas of
    /// exactly one device are summed into the sample, and at most one absolute
    /// packet is attached. Packets of several relative devices, or several
    /// absolute packets, are never attached: the sample's raw fields are
    /// unavailable and the packets become raw-motion evidence. A raw packet
    /// without a hook sample within the window is its own unmatched record.
    fn decide_motion(&mut self, order: Order, limit: Option<SessionTimeUs>) {
        let within = |other: &Order| limit.is_none_or(|limit| other.time <= limit);
        let hook = if raw_side(order) { self.candidates(order).find(within) } else { Some(order) };
        let Some(hook) = hook else {
            self.unpaired(order);
            return;
        };
        let mut relative: BTreeMap<Option<DeviceId>, Vec<Order>> = BTreeMap::new();
        let mut absolute = Vec::new();
        for raw in self.candidates(hook).filter(within).collect::<Vec<_>>() {
            match (self.slots[&raw].action, &self.slots[&raw].packet.sample) {
                (Action::Motion(Motion::Absolute { .. }), _) => absolute.push(raw),
                (_, InputSample::RawPointer(pointer)) => relative.entry(pointer.device).or_default().push(raw),
                _ => {}
            }
        }
        let ambiguous = relative.len() > 1 || absolute.len() > 1;
        let single_device = relative.len() == 1;
        let (mut records, mut evidence) = (vec![hook], Vec::new());
        for raw in relative.into_values().flatten() {
            if single_device { records.push(raw) } else { evidence.push(raw) }
        }
        if absolute.len() == 1 { records.extend(absolute) } else { evidence.extend(absolute) }
        let correlation = if ambiguous {
            Correlation::Ambiguous
        } else if records.len() > 1 {
            Correlation::Matched
        } else if self.crosses_gap(hook.time) {
            Correlation::Discontinuous
        } else {
            Correlation::Unmatched
        };
        records.sort_unstable();
        let key = records[0];
        let promotion = self.promotion(&records, correlation);
        let mut taken = records.into_iter().filter_map(|record| self.take_slot(record)).collect::<Vec<_>>().into_iter();
        let Some(first) = taken.next() else { return };
        let absorbed = evidence.into_iter().filter_map(|record| self.take_slot(record)).collect();
        self.decided.insert(key, CorrelatedInput {
            first, partners: taken.collect(), correlation, promotion, cluster: None, absorbed,
        });
    }

    /// The maximal run of same-axis wheel records from both producers that
    /// starts at `start`: consecutive members are within the match window, in
    /// the same admission epoch and continuity interval (stage I2).
    fn wheel_run(&self, start: Order, axis: WheelAxis, limit: Option<SessionTimeUs>) -> Vec<Order> {
        let admission = self.slots[&start].admission;
        let mut run = vec![start];
        for (order, input) in self.slots.range(start..).skip(1) {
            let last = run[run.len() - 1];
            if order.time.0.saturating_sub(last.time.0) > MATCH_WINDOW_US || limit.is_some_and(|limit| order.time > limit)
                || run.len() == MAX_WHEEL_RUN
            {
                break;
            }
            if matches!(input.action, Action::Wheel { axis: other, .. } if other == axis)
                && input.admission == admission && !self.separated(last.time, order.time)
            {
                run.push(*order);
            }
        }
        if run.len() == MAX_WHEEL_RUN {
            // A capped run is cut only where its raw and hook counts are equal,
            // so no ordinal pair spans the cut; otherwise it is decided whole.
            let mut balance = 0_i64;
            let cut = run.iter().enumerate().filter_map(|(index, order)| {
                balance += if raw_side(*order) { 1 } else { -1 };
                (balance == 0).then_some(index + 1)
            }).last();
            if let Some(cut) = cut {
                run.truncate(cut);
            }
        }
        run
    }

    /// One device's raw records and the hook records of a run pair by
    /// ordinal when their counts are equal. Otherwise every hook record is a
    /// row with unavailable raw values and the raw records are consumed by
    /// the run's first row. A run with one producer only stays unpaired.
    fn decide_wheel(&mut self, run: Vec<Order>) {
        let (raws, hooks): (Vec<Order>, Vec<Order>) = run.iter().partition(|order| raw_side(**order));
        if raws.is_empty() || hooks.is_empty() {
            for order in run {
                self.unpaired(order);
            }
            return;
        }
        let device = |order: &Order| match self.slots[order].packet.sample {
            InputSample::RawPointer(pointer) => pointer.device,
            _ => None,
        };
        if raws.len() == hooks.len() && raws.iter().all(|raw| device(raw) == device(&raws[0])) {
            for (raw, hook) in raws.into_iter().zip(hooks) {
                let (first, other) = if raw < hook { (raw, hook) } else { (hook, raw) };
                let (Some(first_input), Some(partner)) = (self.take_slot(first), self.take_slot(other)) else { continue };
                self.decided.insert(first, CorrelatedInput {
                    first: first_input, partners: vec![partner], correlation: Correlation::Matched, promotion: None,
                    cluster: None, absorbed: Vec::new(),
                });
            }
            return;
        }
        let cluster = Cluster {
            raw: u32::try_from(raws.len()).unwrap_or(u32::MAX), hooks: u32::try_from(hooks.len()).unwrap_or(u32::MAX),
            injected: false, excess_hooks: 0,
        };
        let mut absorbed: Vec<_> = raws.into_iter().filter_map(|raw| self.take_slot(raw)).collect();
        for hook in hooks {
            let Some(first) = self.take_slot(hook) else { continue };
            self.decided.insert(hook, CorrelatedInput {
                first, partners: Vec::new(), correlation: Correlation::Ambiguous, promotion: None, cluster: Some(cluster),
                absorbed: std::mem::take(&mut absorbed),
            });
        }
    }

    /// Another raw/hook pair of the same order class whose raw order and hook
    /// order disagree with this pair's. The crossing makes only this
    /// component ambiguous; it is not propagated to other records.
    fn crosses_pair(&self, raw: Order, hook: Order) -> bool {
        let low = lower_order(SessionTimeUs(hook.time.0.saturating_sub(MATCH_WINDOW_US)));
        let high = lower_order(SessionTimeUs(hook.time.0.saturating_add(MATCH_WINDOW_US).saturating_add(1)));
        let action = self.slots[&raw].action;
        self.slots.range(low..high).any(|(other_raw, candidate)| {
            other_raw.producer == raw.producer && *other_raw != raw && same_order_class(candidate.action, action)
                && self.candidates(*other_raw).any(|other_hook| other_hook != hook && other_raw.cmp(&raw) != other_hook.cmp(&hook))
        })
    }

    /// A promotion link exists only for a certain association.
    fn promotion(&mut self, records: &[Order], correlation: Correlation) -> Option<PenPromotion> {
        if !matches!(correlation, Correlation::Matched | Correlation::Unmatched) {
            for record in records {
                self.pen_links.remove(record);
            }
            return None;
        }
        records.iter().find_map(|slot| self.pen_links.remove(slot).or_else(|| self.link_pen(*slot)))
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
        let raw_partners = self.certain_raw_partners(pointer)?;
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

    /// The raw partners of a hook whose own component is a clean pair or has
    /// no raw record at all. Anything else is not a certain association.
    fn certain_raw_partners(&self, hook: Order) -> Option<Vec<Order>> {
        if self.crosses_gap(hook.time) {
            return None;
        }
        let members = self.component(hook, None);
        let raws: Vec<_> = members.iter().copied().filter(|order| raw_side(*order)).collect();
        let hooks = members.len() - raws.len();
        match raws.as_slice() {
            [] => Some(raws),
            &[raw] if hooks == 1 && !self.crosses_gap(raw.time) && !self.crosses_pair(raw, hook) => Some(raws),
            _ => None,
        }
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

    /// Compatible records within the window, in the same admission epoch and
    /// continuity interval.
    fn candidates(&self, order: Order) -> impl Iterator<Item = Order> + '_ {
        let low = lower_order(SessionTimeUs(order.time.0.saturating_sub(MATCH_WINDOW_US)));
        let high = lower_order(SessionTimeUs(order.time.0.saturating_add(MATCH_WINDOW_US).saturating_add(1)));
        let input = &self.slots[&order];
        self.slots.range(low..high).filter_map(move |(key, other)| {
            (input.admission == other.admission && compatible(input, other)
                && !self.separated(order.time, key.time)).then_some(*key)
        })
    }

    /// A recorded gap, barrier or device removal lies between the two receipts,
    /// so they belong to different continuity intervals.
    fn separated(&self, left: SessionTimeUs, right: SessionTimeUs) -> bool {
        let (low, high) = (left.min(right), left.max(right));
        self.gap_barrier.is_some_and(|barrier| low <= barrier && barrier < high)
            || self.continuity_gaps().any(|gap| gap.first <= high && low <= gap.last)
            || self.removals.range(lower_order(low)..lower_order(SessionTimeUs(high.0.saturating_add(1)))).next().is_some()
    }

    /// Pending gaps that end keyboard and pointer continuity intervals.
    fn continuity_gaps(&self) -> impl Iterator<Item = &InputGap> {
        self.gaps.values().filter(|gap| gap.producer != Producer::OptionalHid)
    }

    /// The candidate window of this receipt crosses a recorded gap, barrier or removal.
    fn crosses_gap(&self, time: SessionTimeUs) -> bool {
        let lower = time.0.saturating_sub(MATCH_WINDOW_US);
        let upper = time.0.saturating_add(MATCH_WINDOW_US);
        self.gap_barrier.is_some_and(|last| lower <= last.0)
            || self.continuity_gaps().any(|gap| upper >= gap.first.0 && lower <= gap.last.0)
            || self.removals.range(lower_order(SessionTimeUs(lower))..lower_order(SessionTimeUs(upper.saturating_add(1))))
                .next().is_some()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputIdExhausted;

pub(crate) enum ProvisionalInputChange {
    Opened(ProvisionalInputObservation),
    Dropped(ObservationId),
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

fn merge_bounds<T: Ord + Copy>(left: Option<[T; 2]>, right: Option<[T; 2]>) -> Option<[T; 2]> {
    match (left, right) {
        (Some(left), Some(right)) => Some([left[0].min(right[0]), left[1].max(right[1])]),
        (range, None) | (None, range) => range,
    }
}

/// `LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED`, or `LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED`.
fn injected_hook(sample: &InputSample) -> bool {
    match sample {
        InputSample::HookKey(key) => key.flags & 0x12 != 0,
        InputSample::HookPointer(pointer) => pointer.flags & 3 != 0,
        _ => false,
    }
}

fn null_handle(sample: &InputSample) -> bool {
    match sample {
        InputSample::RawKey(key) => key.device.is_none(),
        InputSample::RawPointer(pointer) => pointer.device.is_none(),
        _ => false,
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
        // An injected hook pairs only with a NULL-handle raw record of the same
        // kind and identity (INP-7, stage I3).
        _ if (unqualified(&first.packet.sample) || unqualified(&other.packet.sample))
            && !(null_handle(&first.packet.sample) || null_handle(&other.packet.sample)) => return false,
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
