//! Native observation selection runs only on the capture owner.

use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, atomic::{AtomicI64, Ordering}, mpsc::{Receiver, SyncSender, TryRecvError, sync_channel}},
    thread::{self, Thread},
    time::{Duration, Instant},
};

use crate::{
    capture::{frame::FrameMetadata, input::WheelAxis, observation::{ProvisionalInputKind, ProvisionalInputObservation}, save::{SaveKind, SaveObservation}},
    config::{DeviceCalibration, ResolvedCaptureConfig},
    package::{metadata::MonitorCapture, outcome::LossReason},
    protocol::{AdmissionEpoch, DeviceId, EventRef, Evidence, FrameId, GeometryId, SessionTimeUs, ipc::{ForegroundAtReceipt, elevated::VisitBinding}},
};

use super::{
    ProbeError, context::{ELEVATED_FRAME_WAIT_US, FrameApproval, FramePolicy, PointerLink}, readback, still_png, still_rows,
    native::{HeldFrame, Holds, NativeFrame, NativeImages, PRIVATE_HOLDS},
    still_time::{self, Adjustment, Interval, Missing},
};

const PRIVATE_SLOTS: usize = 128;
const CHANNEL: usize = 64;
/// Confirmed saves queued for this owner; one step drains at most this many.
const SAVE_QUEUE: usize = 8;
const PRIVATE_US: i64 = 2_000_000;
const PUBLIC_SLOTS: usize = 128;
const NEXT_US: i64 = 1_000_000;
const SETTLED_US: i64 = 10_000_000;
const SCROLL_US: i64 = 300_000;
const OBSERVE_US: i64 = 2_000_000;
const MAX_SCROLLS: usize = 32;
const QUIET_US: i64 = 500_000;
const CHANGE_LIMIT: f64 = 0.002;
const READBACK_WAIT: Duration = Duration::from_secs(3);

pub(super) struct Permission {
    pub event: EventRef,
    pub geometry: GeometryId,
    pub proof: ForegroundAtReceipt,
    pub device: Option<DeviceId>,
    pub at: SessionTimeUs,
    pub rows: EarlyRows,
}

/// The still rows reserved for one action: a pre-motion or pre-action trigger
/// reserves one row whose before and seen halves are `rows` (IMG-2); a next
/// trigger reserves its next and settled rows.
pub(super) struct EarlyRows {
    lane: still_rows::Rows,
    paired: bool,
    rows: [EarlyRow; 2],
}

/// The journal rows an action of this kind reserves.
pub(super) fn trigger_rows(kind: ProvisionalInputKind) -> usize {
    if kind == ProvisionalInputKind::Next { 2 } else { 1 }
}

/// A private still-row reservation stays uncounted until its action is approved.
enum EarlyRow {
    Reserved(still_rows::Reservation),
    Unavailable(LossReason),
    HandedOff,
}

impl EarlyRow {
    fn reserve(lane: &still_rows::Rows, at: SessionTimeUs) -> Self {
        match lane.reserve(at, None) {
            Ok(reservation) => Self::Reserved(reservation),
            Err(reason) => Self::Unavailable(reason),
        }
    }
}

impl EarlyRows {
    pub fn reserve(lane: &still_rows::Rows, at: SessionTimeUs, kind: ProvisionalInputKind) -> Self {
        let paired = trigger_rows(kind) == 1;
        let rows = if paired {
            match lane.reserve_pair(at) {
                Ok([before, seen]) => [EarlyRow::Reserved(before), EarlyRow::Reserved(seen)],
                Err(reason) => [EarlyRow::Unavailable(reason), EarlyRow::Unavailable(reason)],
            }
        } else {
            [EarlyRow::reserve(lane, at), EarlyRow::reserve(lane, at)]
        };
        Self { lane: lane.clone(), paired, rows }
    }

    pub fn approve(&mut self, kind: ProvisionalInputKind, event: EventRef, geometry: Evidence<GeometryId>,
        admission: AdmissionEpoch, at: SessionTimeUs, missing: &'static str,
    ) {
        let roles = match kind {
            ProvisionalInputKind::PreMotion => [(Trigger::PreMotion, Role::Before); 2],
            ProvisionalInputKind::PreAction | ProvisionalInputKind::Scroll { .. } => [(Trigger::PreAction, Role::Before); 2],
            ProvisionalInputKind::Next => [(Trigger::Next, Role::Image), (Trigger::Settled, Role::Image)],
        };
        // Both halves of a paired row share its reservation and fallback.
        for (index, (trigger, role)) in roles.into_iter().enumerate().take(if self.paired { 1 } else { 2 }) {
            let fallback = fallback_row(trigger, role, at, geometry, admission, Some(event), missing);
            let lost = match &self.rows[index] {
                // The ordered still deadline expired the slot before approval.
                EarlyRow::Reserved(row) => (!row.approve(fallback)).then_some(LossReason::ProducerStall),
                EarlyRow::Unavailable(reason) => Some(*reason),
                EarlyRow::HandedOff => None,
            };
            if let Some(reason) = lost {
                self.rows[index] = EarlyRow::Unavailable(reason);
                if self.paired {
                    self.rows[1] = EarlyRow::Unavailable(reason);
                }
                self.lane.note_loss(at, reason);
            }
        }
    }

    fn take(&mut self, index: usize) -> Option<still_rows::Reservation> {
        match std::mem::replace(&mut self.rows[index], EarlyRow::HandedOff) {
            EarlyRow::Reserved(reservation) => Some(reservation),
            other => {
                self.rows[index] = other;
                None
            }
        }
    }

    fn suppress(self) {
        for row in self.rows {
            if let EarlyRow::Reserved(reservation) = row {
                reservation.suppress();
            }
        }
    }
}

struct Opening {
    request: ProvisionalInputObservation,
    visit: Option<VisitBinding>,
    decision: Receiver<Permission>,
}

pub(super) struct Rest {
    pub reservation: still_rows::Reservation,
    pub at: SessionTimeUs,
    pub admission: AdmissionEpoch,
    pub geometry: GeometryId,
    pub proof: ForegroundAtReceipt,
    pub visit: Option<VisitBinding>,
    pub pointer: Option<PointerLink>,
}

pub(super) struct Save {
    pub reservation: still_rows::Reservation,
    pub confirmation: SaveObservation,
    pub geometry: GeometryId,
    pub admission: AdmissionEpoch,
    pub proof: ForegroundAtReceipt,
    pub visit: Option<VisitBinding>,
}

enum Command {
    Opening(Opening),
    Rest(Rest),
}

#[derive(Clone)]
pub(super) struct Requests {
    send: SyncSender<Command>,
    saves: SyncSender<Save>,
    last_activity: Arc<AtomicI64>,
    input_before: Arc<AtomicI64>,
    last_action: Arc<AtomicI64>,
    owner: Thread,
}

impl Requests {
    pub fn open(&self, request: ProvisionalInputObservation, visit: Option<VisitBinding>) -> Option<SyncSender<Permission>> {
        let (decision, resolved) = sync_channel(1);
        self.send.try_send(Command::Opening(Opening { request, visit, decision: resolved })).ok()?;
        self.owner.unpark();
        Some(decision)
    }

    pub fn input_progress(&self, before: SessionTimeUs) {
        self.input_before.fetch_max(before.0, Ordering::Release);
    }

    pub fn action(&self, at: SessionTimeUs) {
        self.last_action.fetch_max(at.0, Ordering::Release);
        self.owner.unpark();
    }

    pub fn activity(&self, at: SessionTimeUs) {
        self.last_activity.fetch_max(at.0, Ordering::Relaxed);
    }

    pub fn confirmed_save(&self, save: Save) -> bool {
        let queued = self.saves.try_send(save).is_ok();
        if queued {
            self.owner.unpark();
        }
        queued
    }

    pub fn pointer_rest(&self, rest: Rest) {
        if self.send.try_send(Command::Rest(rest)).is_ok() {
            self.owner.unpark();
        }
    }
}

enum Selected {
    Retained(Rc<HeldFrame>),
    Missing(Missing),
    Capacity,
    Waiting,
}

impl Selected {
    fn retained(&self) -> Option<&Rc<HeldFrame>> {
        match self { Self::Retained(frame) => Some(frame), _ => None }
    }
}

enum PrivateSelection {
    Preceding { adjustment: Adjustment, before: Selected, seen: Selected },
    Next(Selected),
    /// No still can be selected for the approved action, with the reason.
    Unavailable(DropReason),
}

struct Private {
    opening: Opening,
    interval: Option<Interval>,
    selection: PrivateSelection,
    scroll_next: Selected,
    refresh: (u32, u32),
}

impl Private {
    /// The frames this pending entry holds.
    fn held(&self) -> impl Iterator<Item = &Rc<HeldFrame>> {
        let selection = match &self.selection {
            PrivateSelection::Preceding { before, seen, .. } => [Some(before), Some(seen)],
            PrivateSelection::Next(selected) => [Some(selected), None],
            PrivateSelection::Unavailable(_) => [None, None],
        };
        selection.into_iter().flatten().chain([&self.scroll_next]).filter_map(Selected::retained)
    }
}

/// A still row's `trigger` (schema §6).
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Trigger {
    Start,
    Checkpoint,
    Final,
    Settled,
    PointerRest,
    Observe,
    PreMotion,
    PreAction,
    Next,
}

impl Trigger {
    /// A pre-motion or pre-action row joins a before and a seen frame (IMG-2).
    fn paired(self) -> bool {
        matches!(self, Self::PreMotion | Self::PreAction)
    }
}

/// A still row's `role` (schema §6).
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Role {
    Before,
    Seen,
    Image,
    FinalSaveSettled,
}

/// The trigger and role of a save-linked still.
pub(super) fn save_slot(kind: SaveKind) -> (Trigger, Role) {
    match kind {
        SaveKind::Checkpoint => (Trigger::Checkpoint, Role::Image),
        SaveKind::Final => (Trigger::Final, Role::FinalSaveSettled),
    }
}

/// The outcome of one still slot. It is serialized only into its row, as
/// `outcome`, and gives the row its `kind`, `file` and `reason` (schema §6).
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SlotOutcome {
    /// `image` is relative to `raw/`, where the journal lives (D-1).
    Saved { image: Option<String>, deduplicated: bool },
    Dropped { reason: DropReason },
    Failed { operation: &'static str, native_code: Option<i32> },
    Unavailable { reason: Missing },
}

impl SlotOutcome {
    fn dropped(reason: DropReason) -> Self {
        Self::Dropped { reason }
    }
}

/// Why a still slot has no image.
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum DropReason {
    AdjustmentFailed,
    AdmissionChanged,
    AdmissionClosed,
    AdmissionClosedBeforeScrollEnd,
    AdmissionClosedBeforeSelection,
    CaptureFailed,
    CaptureIntervalChanged,
    ElevatedFrameUncertified,
    FrameNotApproved,
    GeometryChanged,
    GeometryChangedBeforeScrollEnd,
    GeometryChangedBeforeSelection,
    GeometryChangedBeforeStartFrame,
    LowerPriority,
    NativeCaptureEpochClosed,
    NoApprovedInterval,
    NoApprovedSettledInterval,
    NoEligibleFinalFrameInCaptureEpoch,
    PauseBeforeSaveSelection,
    PauseBeforeScrollEnd,
    PauseBeforeSelection,
    PauseBeforeStartFrame,
    ReadbackCapacity,
    ReadbackDeadline,
    RetentionCapacity,
    RetentionLowerPriority,
    SaveIntervalChanged,
    ScrollBurstCapacity,
    ScrollEndUncertified,
    SettledDeadline,
    SettledRetentionPressure,
    StartFrameUnavailable,
    StillJobCapacity,
    StillOwnerClosed,
    StopBeforeSaveSelection,
    StopBeforeScrollEnd,
    StopBeforeSelection,
    StopBeforeStartFrame,
    /// The PNG worker did not save the image.
    #[serde(untagged)]
    Png(still_png::Dropped),
    /// The capture owner's drain deadline, named by that owner.
    #[serde(untagged)]
    Drain(&'static str),
}

impl DropReason {
    /// The reason at a Stop or a pause boundary.
    fn at_boundary(stopping: bool, stop: Self, pause: Self) -> Self {
        if stopping { stop } else { pause }
    }
}

impl From<readback::Rejected> for DropReason {
    fn from(reason: readback::Rejected) -> Self {
        use readback::Rejected;
        match reason {
            Rejected::Pressure => Self::ReadbackCapacity,
            Rejected::EpochMismatch => Self::CaptureIntervalChanged,
            Rejected::ApprovalSuppressed => Self::ElevatedFrameUncertified,
            Rejected::Expired => Self::ReadbackDeadline,
            Rejected::Closed => Self::AdmissionClosed,
            Rejected::CaptureFailed => Self::CaptureFailed,
        }
    }
}

struct Public {
    event: Option<EventRef>,
    trigger: Trigger,
    role: Role,
    at: SessionTimeUs,
    geometry: GeometryId,
    admission: AdmissionEpoch,
    interval: Option<Interval>,
    adjustment: Option<Adjustment>,
    frame: Option<FrameEvidence>,
    state: State,
    pointer: Option<PointerLink>,
    save: Option<SaveObservation>,
    reservation: Option<still_rows::Reservation>,
    settled_confirmed_t_us: Option<SessionTimeUs>,
    scroll_device: Option<Evidence<DeviceId>>,
}

#[derive(serde::Serialize)]
struct FrameEvidence {
    id: FrameId,
    source_t_us: SessionTimeUs,
    recv_t_us: SessionTimeUs,
    approval_generation: u64,
    width: u32,
    height: u32,
    color: crate::capture::frame::CaptureColor,
    source_uncertainty_us: Option<crate::protocol::UncertaintyUs>,
    #[serde(skip)]
    coverage: Option<still_rows::StillCoverage>,
}

enum State {
    Settling,
    Selected(Selected),
    Work(Rc<RefCell<FrameWork>>),
    Done(SlotOutcome),
}

impl State {
    fn dropped(reason: DropReason) -> Self {
        Self::Done(SlotOutcome::dropped(reason))
    }
}

impl Public {
    /// A slot without an event, interval, frame or reservation yet.
    fn new(trigger: Trigger, role: Role, at: SessionTimeUs, geometry: GeometryId, admission: AdmissionEpoch, state: State) -> Self {
        Self { event: None, trigger, role, at, geometry, admission, interval: None, adjustment: None, frame: None,
            state, pointer: None, save: None, reservation: None, settled_confirmed_t_us: None, scroll_device: None }
    }

    fn priority(&self) -> still_png::Priority {
        match self.trigger {
            Trigger::PreMotion | Trigger::PreAction => still_png::Priority::BeforeSeen,
            Trigger::Next => still_png::Priority::Next,
            Trigger::Settled => still_png::Priority::Settled,
            Trigger::Checkpoint => still_png::Priority::Checkpoint,
            Trigger::Start => still_png::Priority::Start,
            Trigger::Final => still_png::Priority::Final,
            Trigger::PointerRest | Trigger::Observe => still_png::Priority::Idle,
        }
    }

    /// The row of this slot. A seen slot is the second half of its trigger's
    /// row, which `still_rows` joins to the before half (IMG-2).
    fn row(&self, outcome: &SlotOutcome) -> serde_json::Value {
        let row = self.row_value(outcome);
        crate::devlog::devlog!("stills", "slot m{} trigger={} role={} trigger_t_us={} kind={} reason={}",
            self.geometry.monitor.0, row["trigger"], row["role"], self.at.0, row["outcome"]["kind"], row["reason"]);
        row
    }

    fn row_value(&self, outcome: &SlotOutcome) -> serde_json::Value {
        let (saved, file, reason) = match outcome {
            SlotOutcome::Saved { image, .. } => (true, image.as_deref(), serde_json::Value::Null),
            SlotOutcome::Dropped { reason } => (false, None, serde_json::json!(reason)),
            SlotOutcome::Failed { operation, .. } => (false, None, serde_json::json!(operation)),
            SlotOutcome::Unavailable { reason } => (false, None, serde_json::json!(reason)),
        };
        let frame = self.frame.as_ref();
        serde_json::json!({
            "kind": if saved { "still" } else { "still_dropped" },
            "source_t_us": frame.map(|frame| frame.source_t_us),
            "monitor": self.geometry.monitor,
            "trigger": self.trigger, "role": self.role, "trigger_t_us": self.at,
            "frame_obs_id": frame.map(|frame| frame.id.sequence),
            "event_ref": self.event,
            "file": file,
            "width": frame.map(|frame| frame.width), "height": frame.map(|frame| frame.height),
            "reason": reason,
            "geometry": self.geometry, "admission_epoch": self.admission, "scroll_device": self.scroll_device,
            "settled_confirmed_t_us": self.settled_confirmed_t_us, "adjustment": self.adjustment,
            "frame": self.frame, "element": self.pointer, "outcome": outcome,
            "save": self.save.as_ref().map(|save| serde_json::json!({
                "id": save.id(), "kind": save.kind(), "routing_basis": save.routing_basis(),
            })),
        })
    }

    /// The approved fallback row of this slot's reservation (schema §6).
    fn fallback(&self, reason: DropReason) -> serde_json::Value {
        let mut row = self.row_value(&SlotOutcome::dropped(reason));
        row["geometry_evidence"] = serde_json::json!(Evidence::Observed(self.geometry));
        row
    }
}

impl FrameEvidence {
    fn new(meta: &FrameMetadata, settled_confirmed: Option<SessionTimeUs>) -> Self {
        Self { id: meta.id, source_t_us: meta.source_t_us, recv_t_us: meta.recv_t_us,
            approval_generation: meta.approval_generation, width: meta.extent.width.get(),
            height: meta.extent.height.get(), color: meta.color, source_uncertainty_us: meta.source_uncertainty_us,
            coverage: meta.coverage.as_ref().map(|coverage| still_rows::StillCoverage {
                visit: Arc::clone(coverage),
                last: settled_confirmed.unwrap_or(meta.source_t_us).max(meta.source_t_us),
            }) }
    }
}

/// The readback and PNG job of one held frame, shared by every slot that
/// selected it (C2). Bytes are charged once per frame. The last attached slot
/// releases the work; a slot resolved while attached only detaches, and
/// dropping an unfinished PNG ticket cancels its job.
struct FrameWork {
    frame: FrameId,
    /// The PNG job takes the highest priority, the rate exemption and the
    /// least restrictive coverage among the slots attached before it starts.
    /// Each row is still checked against its own coverage.
    priority: still_png::Priority,
    exempt: bool,
    coverage: Option<still_rows::StillCoverage>,
    stage: WorkStage,
}

enum WorkStage {
    Readback(readback::Ticket),
    Encoding(still_png::Ticket),
    /// The slot outcome; `withdrawn` clears each slot's frame evidence.
    Done { outcome: SlotOutcome, withdrawn: bool },
}

impl FrameWork {
    /// Advance the work. Without the PNG inbox only a finished job is collected.
    fn advance(&mut self, png: Option<&still_png::Inbox>) {
        let done = |outcome: SlotOutcome, withdrawn: bool| WorkStage::Done { outcome, withdrawn };
        let png_dropped = |reason: still_png::Dropped| SlotOutcome::dropped(DropReason::Png(reason));
        self.stage = match &mut self.stage {
            WorkStage::Readback(ticket) => {
                let Some(png) = png else { return };
                let Some(result) = ticket.try_result() else { return };
                match result {
                    Ok(pixels) if pixels.metadata.covered() => match png.request(pixels, self.priority, self.exempt, self.coverage.clone()) {
                        Ok(ticket) => WorkStage::Encoding(ticket),
                        Err(reason) => done(png_dropped(reason), false),
                    },
                    Ok(_) => done(SlotOutcome::dropped(DropReason::ElevatedFrameUncertified), true),
                    Err(reason) => done(SlotOutcome::dropped(reason.into()), false),
                }
            }
            WorkStage::Encoding(ticket) => match ticket.poll() {
                None => return,
                Some(still_png::Outcome::Saved { reference, reused }) => done(SlotOutcome::Saved {
                    image: reference.relative_uri().strip_prefix("raw/").map(str::to_owned), deduplicated: reused,
                }, false),
                Some(still_png::Outcome::Dropped(reason)) => done(png_dropped(reason),
                    matches!(reason, still_png::Dropped::ApprovalSuppressed)),
                Some(still_png::Outcome::Failed { operation, native_code }) => {
                    done(SlotOutcome::Failed { operation, native_code }, false)
                }
            },
            WorkStage::Done { .. } => return,
        };
    }

    fn outcome(&self) -> Option<(SlotOutcome, bool)> {
        match &self.stage {
            WorkStage::Done { outcome, withdrawn } => Some((outcome.clone(), *withdrawn)),
            _ => None,
        }
    }
}

/// Attach a slot that retained the work's frame: its frame evidence is
/// recorded and its hold is released, because the readback copy is submitted.
fn attach(slot: &mut Public, work: &Rc<RefCell<FrameWork>>) {
    let State::Selected(Selected::Retained(frame)) = &slot.state else { return };
    slot.frame = Some(FrameEvidence::new(&frame.metadata, slot.settled_confirmed_t_us));
    let mut shared = work.borrow_mut();
    shared.priority = shared.priority.max(slot.priority());
    shared.exempt |= matches!(slot.role, Role::Before);
    if let Some(coverage) = slot.frame.as_ref().and_then(|frame| frame.coverage.as_ref())
        && shared.coverage.as_ref().is_none_or(|old| coverage.last < old.last)
    {
        shared.coverage = Some(coverage.clone());
    }
    drop(shared);
    slot.state = State::Work(Rc::clone(work));
}

struct Quiet {
    interval: Interval,
    last: FrameId,
    since: SessionTimeUs,
    candidate: Rc<HeldFrame>,
}

struct Scroll {
    device: Option<DeviceId>,
    axis: WheelAxis,
    interval: Interval,
    end: SessionTimeUs,
    event: EventRef,
    next: Selected,
}

impl Scroll {
    /// The next and settled slots of this burst, in `states` order.
    fn observations(&self, states: [State; 2]) -> [Public; 2] {
        let device = self.device.map_or(Evidence::Unavailable(crate::protocol::UnavailableReason::Ambiguous), Evidence::Observed);
        let [next, settled] = states;
        [(Trigger::Next, next), (Trigger::Settled, settled)].map(|(trigger, state)| Public {
            event: Some(self.event), interval: Some(self.interval), scroll_device: Some(device),
            ..Public::new(trigger, Role::Image, self.end, self.interval.geometry, self.interval.admission, state)
        })
    }

    fn dropped_observations(&self, reason: DropReason) -> [Public; 2] {
        self.observations([State::dropped(reason), State::dropped(reason)])
    }
}

pub(super) struct Stills {
    input: Receiver<Command>,
    rows: still_rows::Rows,
    receipt: SessionTimeUs,
    saves: Receiver<Save>,
    deferred: Option<Command>,
    deferred_save: Option<Save>,
    last_activity: Arc<AtomicI64>,
    input_before: Arc<AtomicI64>,
    last_action: Arc<AtomicI64>,
    seen_action: SessionTimeUs,
    observed_activity: SessionTimeUs,
    observe_at: SessionTimeUs,
    observed_frame: Option<FrameId>,
    change_at: Option<(Interval, SessionTimeUs)>,
    scrolls: Vec<Scroll>,
    holds: Holds,
    pending: VecDeque<Private>,
    public: VecDeque<Public>,
    start: Option<Public>,
    quiet_after: Option<SessionTimeUs>,
    quiet: Option<Quiet>,
}

impl Stills {
    pub fn new(rows: still_rows::Rows, budget: Arc<readback::Budget>) -> (Requests, Self) {
        let (send, input) = sync_channel(CHANNEL);
        let (save_send, saves) = sync_channel(SAVE_QUEUE);
        let last_activity = Arc::new(AtomicI64::new(0));
        let input_before = Arc::new(AtomicI64::new(0));
        let last_action = Arc::new(AtomicI64::new(0));
        (Requests { send, saves: save_send, owner: thread::current(), last_activity: Arc::clone(&last_activity), input_before: Arc::clone(&input_before), last_action: Arc::clone(&last_action) },
         Self { input, rows, receipt: SessionTimeUs(0), saves, deferred: None, deferred_save: None, last_activity, input_before, last_action, seen_action: SessionTimeUs(0), observed_activity: SessionTimeUs(0), observe_at: SessionTimeUs(OBSERVE_US),
             observed_frame: None, change_at: None, scrolls: Vec::with_capacity(MAX_SCROLLS), holds: Holds::new(budget), pending: VecDeque::with_capacity(PRIVATE_SLOTS),
             public: VecDeque::with_capacity(PUBLIC_SLOTS), start: None, quiet_after: None, quiet: None })
    }

    pub fn arm_start(&mut self, boundary: crate::session::AdmissionBoundary, geometry: GeometryId) {
        let mut start = Public::new(Trigger::Start, Role::Image, boundary.time, geometry, boundary.epoch,
            State::Selected(Selected::Waiting));
        start.reservation = self.rows.reserve_public(boundary.time, start.fallback(DropReason::StartFrameUnavailable));
        if start.reservation.is_some() {
            self.start = Some(start);
        }
    }

    pub fn step(
        &mut self, images: &NativeImages, mode: &MonitorCapture, policy: &FramePolicy,
        config: &ResolvedCaptureConfig, admission: AdmissionEpoch, now: SessionTimeUs,
    ) {
        let input_before = SessionTimeUs(self.input_before.load(Ordering::Acquire));
        self.receipt = self.receipt.max(now);
        let action = SessionTimeUs(self.last_action.load(Ordering::Acquire));
        if action > self.seen_action && action <= now {
            self.seen_action = action;
            self.invalidate_quiet(action);
        }
        self.start_step(images, self.receipt);
        self.drain_saves(images, mode, policy, admission, now);
        let openings_drained = self.drain_commands(images, mode, policy, config, admission, now);
        self.select_private(images, now);
        self.resolve_private(images, config, now);
        let approval = policy.current(admission, now);
        let input_before = openings_drained.then_some(input_before);
        self.scroll_step(images, now, input_before);
        self.settle_step(images, mode, admission, approval.clone(), now, input_before);
        self.observe(images, mode, admission, approval, now);
        self.rows.advance(self.receipt);
    }

    /// Take the confirmed saves received by `now`. An elevated save waits for
    /// its frame approval up to `ELEVATED_FRAME_WAIT_US`.
    fn drain_saves(&mut self, images: &NativeImages, mode: &MonitorCapture, policy: &FramePolicy,
        admission: AdmissionEpoch, now: SessionTimeUs,
    ) {
        for _ in 0..SAVE_QUEUE {
            let Some(save) = self.deferred_save.take().or_else(|| self.saves.try_recv().ok()) else { break };
            if save.confirmation.time().recv_t_us > now {
                self.deferred_save = Some(save);
                break;
            }
            let at = save.confirmation.time().recv_t_us;
            let approval = policy.for_receipt(save.proof, admission, save.visit, at, now);
            if approval.is_err() && now.0.saturating_sub(at.0) < ELEVATED_FRAME_WAIT_US {
                self.deferred_save = Some(save);
                break;
            }
            self.save_step(save, images, mode, admission, approval.ok().flatten());
        }
    }

    /// Take the openings and pointer rests received by `now`, while private
    /// slots remain. True when every command received by `now` was taken.
    fn drain_commands(&mut self, images: &NativeImages, mode: &MonitorCapture, policy: &FramePolicy,
        config: &ResolvedCaptureConfig, admission: AdmissionEpoch, now: SessionTimeUs,
    ) -> bool {
        for _ in 0..CHANNEL {
            if self.pending.len() == PRIVATE_SLOTS {
                return false;
            }
            let Some(command) = self.deferred.take().or_else(|| self.input.try_recv().ok()) else {
                return true;
            };
            let at = match &command {
                Command::Opening(opening) => opening.request.time.recv_t_us,
                Command::Rest(rest) => rest.at,
            };
            if at > now {
                self.deferred = Some(command);
                return true;
            }
            let (proof, visit) = match &command {
                Command::Opening(opening) => (opening.request.foreground_at_receipt, opening.visit),
                Command::Rest(rest) => (Some(rest.proof), rest.visit),
            };
            let approval = proof.map_or(Ok(None), |proof| policy.for_receipt(proof, admission, visit, at, now));
            // A contended or newer-than-`now` read is transient for every source;
            // an elevated one also waits for its certificate. Retry it later.
            if approval.is_err() && now.0.saturating_sub(at.0) < ELEVATED_FRAME_WAIT_US {
                self.deferred = Some(command);
                return false;
            }
            let approval = approval.ok().flatten();
            match command {
                Command::Opening(opening) => self.open_private(opening, images, mode, config, admission, approval),
                Command::Rest(rest) => self.rest(rest, images, mode, admission, approval),
            }
        }
        false
    }

    /// Select the frames of one opening at its receipt and keep it pending
    /// until its action is approved or abandoned.
    fn open_private(&mut self, opening: Opening, images: &NativeImages, mode: &MonitorCapture,
        config: &ResolvedCaptureConfig, admission: AdmissionEpoch, approval: Option<FrameApproval>,
    ) {
        let request = opening.request;
        let at = request.time.recv_t_us;
        let interval = approval.filter(|_| request.geometry == Evidence::Observed(mode.geometry.id)
            && request.admission == admission).map(|approval| Interval {
            geometry: mode.geometry.id, capture: mode.capture_epoch, admission, approval: approval.generation,
            elevated_visit: approval.elevated_visit(),
        });
        // An unread refresh rate selects no seen boundary (`UnknownRefresh`).
        let refresh = mode.refresh_numerator.zip(mode.refresh_denominator).map_or((0, 0), |(numerator, denominator)| (numerator, denominator.get()));
        // The Receiver opens a pre-motion envelope only for the first
        // motion after the IMG-2 idle period.
        let selection = if let Some(interval) = interval {
            match request.kind {
                ProvisionalInputKind::PreMotion | ProvisionalInputKind::PreAction | ProvisionalInputKind::Scroll { .. } => {
                    match Adjustment::from_receipt(at, calibration(config, request.device, interval)) {
                        Ok(adjustment) => {
                            let pair = still_time::preceding(images, interval, adjustment, refresh);
                            let before = self.retain_private(pair.before, still_png::Priority::BeforeSeen, None);
                            let seen = self.retain_private(pair.seen, still_png::Priority::BeforeSeen, Some(&before));
                            PrivateSelection::Preceding { adjustment, before, seen }
                        }
                        Err(_) => PrivateSelection::Unavailable(DropReason::AdjustmentFailed),
                    }
                }
                ProvisionalInputKind::Next => PrivateSelection::Next(self.private_next(images, interval, at)),
            }
        } else {
            PrivateSelection::Unavailable(DropReason::NoApprovedInterval)
        };
        self.pending.push_back(Private { opening, interval, selection, scroll_next: Selected::Waiting, refresh });
    }

    /// Make the waiting next selections of pending entries. They are made in
    /// place, so the private hold cap counts every pending entry.
    fn select_private(&mut self, images: &NativeImages, now: SessionTimeUs) {
        for index in 0..self.pending.len() {
            let entry = &self.pending[index];
            let Some(interval) = entry.interval else { continue };
            let at = entry.opening.request.time.recv_t_us;
            let end = SessionTimeUs(at.0.saturating_add(SCROLL_US));
            let next = matches!(entry.selection, PrivateSelection::Next(Selected::Waiting));
            let scroll = matches!(entry.opening.request.kind, ProvisionalInputKind::Scroll { .. })
                && matches!(entry.scroll_next, Selected::Waiting) && now >= end;
            if next {
                let selected = self.private_next(images, interval, at);
                self.pending[index].selection = PrivateSelection::Next(selected);
            }
            if scroll {
                let selected = self.private_next(images, interval, end);
                self.pending[index].scroll_next = selected;
            }
        }
    }

    /// Publish the pending entries whose action was approved; drop those
    /// abandoned or older than `PRIVATE_US`.
    fn resolve_private(&mut self, images: &NativeImages, config: &ResolvedCaptureConfig, now: SessionTimeUs) {
        let count = self.pending.len();
        for _ in 0..count {
            let Some(entry) = self.pending.pop_front() else { break };
            match entry.opening.decision.try_recv() {
                Ok(permission) => self.publish_private(entry, permission, images, config),
                Err(TryRecvError::Empty) if now.0.saturating_sub(entry.opening.request.time.recv_t_us.0) < PRIVATE_US => {
                    self.pending.push_back(entry);
                }
                _ => {}
            }
        }
    }

    fn start_step(&mut self, images: &NativeImages, now: SessionTimeUs) {
        let Some(start) = &self.start else { return };
        let eligible = |frame: &FrameMetadata| frame.covered() && frame.admission == start.admission
            && frame.geometry.id == start.geometry && frame.source_t_us >= start.at;
        // An evicted eligible frame preceded every frame still in history, so
        // a later frame is never presented as the first one (ALG §8.2).
        let first = if images.last_evicted().is_some_and(|frame| eligible(&frame)) {
            Some(Err(Missing::HistoryLost))
        } else {
            images.history().iter().find(|frame| eligible(&frame.metadata)).map(Ok)
        };
        if first.is_none() && now.0.saturating_sub(start.at.0) < SETTLED_US {
            return;
        }
        let Some(mut start) = self.start.take() else { return };
        let selected = self.retain(first.unwrap_or(Err(Missing::NoEligibleFrame)), still_png::Priority::Start);
        start.state = State::Selected(selected);
        self.push_public(start);
    }

    fn save_step(&mut self, command: Save, images: &NativeImages, mode: &MonitorCapture,
        admission: AdmissionEpoch, approval: Option<FrameApproval>,
    ) {
        let save = command.confirmation;
        let at = save.time().recv_t_us;
        let interval = approval.filter(|_| command.admission == admission && mode.geometry.id == command.geometry
            && save.monitor() == command.geometry.monitor)
            .map(|approval| Interval { geometry: command.geometry, capture: mode.capture_epoch, admission,
                approval: approval.generation, elevated_visit: approval.elevated_visit() });
        let (trigger, role) = save_slot(save.kind());
        let state = match (save.kind(), interval) {
            (SaveKind::Checkpoint, Some(interval)) => {
                let image = still_time::latest(images, interval, at);
                State::Selected(self.retain(image.ok_or(Missing::NoEligibleFrame), still_png::Priority::Checkpoint))
            }
            (SaveKind::Final, Some(_)) => {
                self.invalidate_quiet(at);
                State::Settling
            }
            (_, None) => State::dropped(if command.admission != admission {
                DropReason::AdmissionClosed
            } else {
                DropReason::SaveIntervalChanged
            }),
        };
        self.push_public(Public { interval, save: Some(save), reservation: Some(command.reservation),
            ..Public::new(trigger, role, at, command.geometry, command.admission, state) });
    }

    fn rest(&mut self, rest: Rest, images: &NativeImages, mode: &MonitorCapture,
        admission: AdmissionEpoch, approval: Option<FrameApproval>,
    ) {
        let changed = if rest.admission != admission {
            Some(DropReason::AdmissionChanged)
        } else if rest.geometry != mode.geometry.id {
            Some(DropReason::GeometryChanged)
        } else {
            None
        };
        let (None, Some(approval)) = (changed, approval) else {
            let outcome = SlotOutcome::dropped(changed.unwrap_or(DropReason::FrameNotApproved));
            let slot = Public::new(Trigger::PointerRest, Role::Image, rest.at, rest.geometry, rest.admission,
                State::Done(outcome.clone()));
            rest.reservation.finish(slot.row(&outcome));
            return;
        };
        let interval = Interval { geometry: rest.geometry, capture: mode.capture_epoch, admission,
            approval: approval.generation, elevated_visit: approval.elevated_visit() };
        let candidate = still_time::latest(images, interval, rest.at).ok_or(Missing::NoEligibleFrame);
        let selected = self.retain(candidate, still_png::Priority::Idle);
        self.push_public(Public { interval: Some(interval), pointer: rest.pointer, reservation: Some(rest.reservation),
            ..Public::new(Trigger::PointerRest, Role::Image, rest.at, rest.geometry, admission, State::Selected(selected)) });
    }

    pub fn changed(&mut self, frame: FrameMetadata) {
        let interval = Interval { geometry: frame.geometry.id, capture: frame.id.epoch,
            admission: frame.admission, approval: frame.approval_generation, elevated_visit: frame.elevated_visit() };
        self.change_at = Some((interval, frame.source_t_us));
    }

    fn observe(&mut self, images: &NativeImages, mode: &MonitorCapture, admission: AdmissionEpoch,
        approval: Option<FrameApproval>, now: SessionTimeUs,
    ) {
        let activity = SessionTimeUs(self.last_activity.load(Ordering::Relaxed));
        if activity > self.observed_activity {
            self.observed_activity = activity;
            self.observe_at = SessionTimeUs(activity.0.saturating_add(OBSERVE_US));
        }
        if now < self.observe_at { return; }
        self.observe_at = SessionTimeUs(now.0.saturating_add(OBSERVE_US));
        let Some(approval) = approval else { return };
        let interval = Interval { geometry: mode.geometry.id, capture: mode.capture_epoch, admission,
            approval: approval.generation, elevated_visit: approval.elevated_visit() };
        let Some(frame) = still_time::latest(images, interval, now) else { return };
        let changed = self.change_at.is_some_and(|(scope, at)| scope == interval && now.0.saturating_sub(at.0) <= OBSERVE_US);
        if Some(frame.metadata.id) == self.observed_frame || !changed {
            return;
        }
        self.observed_frame = Some(frame.metadata.id);
        let selected = self.retain(Ok(frame), still_png::Priority::Idle);
        self.push_public(Public { interval: Some(interval),
            ..Public::new(Trigger::Observe, Role::Image, now, mode.geometry.id, admission, State::Selected(selected)) });
    }

    fn scroll_step(&mut self, images: &NativeImages, now: SessionTimeUs, input_before: Option<SessionTimeUs>) {
        let mut index = 0;
        while index < self.scrolls.len() {
            let burst = &self.scrolls[index];
            let newer_pending = self.pending.iter().any(|entry| {
                let request = entry.opening.request;
                request.kind == (ProvisionalInputKind::Scroll { axis: burst.axis })
                    && entry.interval == Some(burst.interval) && request.device == burst.device
                    && request.time.recv_t_us < burst.end
                    && request.time.recv_t_us.0 > burst.end.0.saturating_sub(SCROLL_US)

            });
            let pending_prefix = input_before.is_none_or(|before| before <= burst.end);
            let can_wait = now.0.saturating_sub(burst.end.0) < NEXT_US;
            if burst.end > now || can_wait && (newer_pending || pending_prefix) {
                if burst.end <= now && matches!(burst.next, Selected::Waiting) {
                    let (interval, end) = (burst.interval, burst.end);
                    let next = self.select_next(images, interval, end);
                    self.scrolls[index].next = next;
                }
                index += 1;
                continue;
            }
            let scroll = self.scrolls.remove(index);
            if pending_prefix || newer_pending {
                self.drop_scroll(&scroll, DropReason::ScrollEndUncertified);
            } else {
                self.finish_scroll(scroll, images);
            }
        }
    }

    fn drop_scroll(&mut self, scroll: &Scroll, reason: DropReason) {
        for slot in scroll.dropped_observations(reason) {
            self.push_public(slot);
        }
    }

    fn finish_scroll(&mut self, mut scroll: Scroll, images: &NativeImages) {
        let next = match std::mem::replace(&mut scroll.next, Selected::Waiting) {
            Selected::Waiting => self.select_next(images, scroll.interval, scroll.end),
            selected => selected,
        };
        self.invalidate_quiet(scroll.end);
        for slot in scroll.observations([State::Selected(next), State::Settling]) {
            self.push_public(slot);
        }
    }

    fn invalidate_quiet(&mut self, at: SessionTimeUs) {
        self.quiet_after = Some(self.quiet_after.map_or(at, |old| old.max(at)));
        self.quiet = None;
    }

    fn settle_step(&mut self, images: &NativeImages, mode: &MonitorCapture, admission: AdmissionEpoch,
        approval: Option<FrameApproval>, now: SessionTimeUs, input_before: Option<SessionTimeUs>,
    ) {
        let current = approval.map(|approval| Interval {
            geometry: mode.geometry.id, capture: mode.capture_epoch, admission,
            approval: approval.generation, elevated_visit: approval.elevated_visit(),
        });
        self.expire_settling(current, now);
        let Some(interval) = current else {
            self.quiet = None;
            return;
        };
        if !self.public.iter().any(|slot| matches!(slot.state, State::Settling) && slot.interval == current) {
            self.quiet = None;
            self.quiet_after = None;
            return;
        }
        let Some(after) = self.quiet_after else { return };
        if self.track_quiet(images, interval, after) {
            self.confirm_quiet(interval, now, input_before);
        }
    }

    /// End the settling slots that cannot be confirmed in `current`.
    fn expire_settling(&mut self, current: Option<Interval>, now: SessionTimeUs) {
        for slot in &mut self.public {
            if !matches!(slot.state, State::Settling) { continue; }
            if slot.interval.is_none() {
                slot.state = State::dropped(DropReason::NoApprovedSettledInterval);
            } else if now.0.saturating_sub(slot.at.0) >= SETTLED_US {
                slot.state = State::dropped(DropReason::SettledDeadline);
            } else if current.is_some() && slot.interval != current {
                slot.state = State::dropped(DropReason::CaptureIntervalChanged);
            }
        }
    }

    /// Follow the quiet candidate of `interval` through the frames after
    /// `after`: a run of consecutive frames each changing less than
    /// `CHANGE_LIMIT`. False while a frame still awaits its change score.
    fn track_quiet(&mut self, images: &NativeImages, interval: Interval, after: SessionTimeUs) -> bool {
        for frame in images.history().iter().filter(|frame| interval.contains(&frame.metadata) && frame.metadata.source_t_us > after) {
            let meta = &frame.metadata;
            if let Some(quiet) = &mut self.quiet {
                if quiet.interval == interval && meta.id.sequence.0 <= quiet.last.sequence.0 {
                    continue;
                }
                if !frame.change_sampled {
                    return false;
                }
                if quiet.interval == interval && quiet.last.sequence.0.checked_add(1) == Some(meta.id.sequence.0)
                    && frame.change.is_some_and(|change| change < CHANGE_LIMIT)
                {
                    quiet.last = meta.id;
                    continue;
                }
            }
            // Release the previous candidate before claiming another distinct hold.
            self.quiet = None;
            let Selected::Retained(candidate) = self.retain(Ok(frame), still_png::Priority::Settled) else {
                self.drop_settling(DropReason::SettledRetentionPressure, |slot| slot.interval == Some(interval));
                return true;
            };
            self.quiet = Some(Quiet { interval, last: meta.id, since: meta.source_t_us, candidate });
        }
        true
    }

    /// Select the quiet candidate for the settling slots of `interval` once
    /// it stayed quiet for `QUIET_US`.
    fn confirm_quiet(&mut self, interval: Interval, now: SessionTimeUs, input_before: Option<SessionTimeUs>) {
        let Some(quiet) = &self.quiet else { return };
        let current = Some(interval);
        let eligible_at = SessionTimeUs(quiet.since.0.saturating_add(QUIET_US));
        // Wait for the existing input certification prefix and any
        // in-owner handoff which can still invalidate this candidate.
        let unresolved_action = self.pending.iter().any(|entry| {
            entry.interval == current && entry.opening.request.time.recv_t_us <= now
                && entry.opening.request.kind != ProvisionalInputKind::PreMotion
        });
        if now < eligible_at || input_before.is_none_or(|before| before <= eligible_at) || unresolved_action {
            return;
        }
        for slot in &mut self.public {
            if slot.interval == current && matches!(slot.state, State::Settling) {
                slot.settled_confirmed_t_us = Some(now);
                slot.state = State::Selected(Selected::Retained(Rc::clone(&quiet.candidate)));
            }
        }
        self.quiet = None;
        self.quiet_after = None;
    }

    /// End the settling slots that `scope` selects with `reason`.
    fn drop_settling(&mut self, reason: DropReason, scope: impl Fn(&Public) -> bool) {
        for slot in &mut self.public {
            if matches!(slot.state, State::Settling) && scope(slot) {
                slot.state = State::dropped(reason);
            }
        }
    }

    fn retain(&mut self, frame: Result<&NativeFrame, Missing>, priority: still_png::Priority) -> Selected {
        let frame = match frame { Ok(frame) => frame, Err(reason) => return Selected::Missing(reason) };
        loop {
            if let Some(held) = self.holds.retain(frame) {
                return Selected::Retained(held);
            }
            if !self.reclaim(priority, false) {
                return Selected::Capacity;
            }
        }
    }

    /// A private selection takes a new hold only while the pending private
    /// entries hold fewer than `PRIVATE_HOLDS` distinct frames, so one extra
    /// hold stays available to public requests. `extra` is a selection of the
    /// entry being built, which is not pending yet.
    fn retain_private(&mut self, frame: Result<&NativeFrame, Missing>, priority: still_png::Priority,
        extra: Option<&Selected>,
    ) -> Selected {
        let frame = match frame { Ok(frame) => frame, Err(reason) => return Selected::Missing(reason) };
        while !self.holds.holds(frame.metadata.id) && self.private_holds(extra) >= PRIVATE_HOLDS {
            if !self.reclaim(priority, true) {
                return Selected::Capacity;
            }
        }
        self.retain(Ok(frame), priority)
    }

    /// The first frame after `end`, retained at the next priority, which may
    /// release lower-priority holds (ALG §4, IMG-7).
    fn select_next(&mut self, images: &NativeImages, interval: Interval, end: SessionTimeUs) -> Selected {
        match still_time::next(images, interval, end) {
            Ok(None) => Selected::Waiting,
            Ok(Some(frame)) => self.retain(Ok(frame), still_png::Priority::Next),
            Err(reason) => Selected::Missing(reason),
        }
    }

    fn private_next(&mut self, images: &NativeImages, interval: Interval, end: SessionTimeUs) -> Selected {
        match still_time::next(images, interval, end) {
            Ok(None) => Selected::Waiting,
            Ok(Some(frame)) => self.retain_private(Ok(frame), still_png::Priority::Next, None),
            Err(reason) => Selected::Missing(reason),
        }
    }

    fn private_holds(&self, extra: Option<&Selected>) -> usize {
        let mut frames: Vec<*const HeldFrame> = Vec::new();
        for frame in self.pending.iter().flat_map(Private::held).chain(extra.and_then(Selected::retained)) {
            if !frames.contains(&Rc::as_ptr(frame)) {
                frames.push(Rc::as_ptr(frame));
            }
        }
        frames.len()
    }

    /// Release the holds of the lowest IMG-7 priority below `priority`; among
    /// equal priority the newest goes first (REL-2). Private pending
    /// selections take part; with `private_only` only they do. Preceding
    /// private selections have the highest priority and are never released.
    ///
    /// Slots that select the same frame share one hold, so releasing only
    /// some of them frees nothing (ALG §13.12). Outside `private_only` a frame
    /// is released only when every holder is a victim; a holder outside the
    /// victims, such as the frame a readback is being fitted for, keeps it.
    fn reclaim(&mut self, priority: still_png::Priority, private_only: bool) -> bool {
        use still_png::Priority::{Next, Settled};
        enum Victim { Quiet, Public(usize), Scroll(usize), PrivateNext(usize), PrivateScroll(usize) }
        /// The victims holding one frame: their highest priority, newest
        /// receipt, the frame's holder count and the victims.
        struct Group { priority: still_png::Priority, at: SessionTimeUs, frame: *const HeldFrame, holders: usize, victims: Vec<Victim> }
        let mut groups: Vec<Group> = Vec::new();
        let mut add = |low: still_png::Priority, at: SessionTimeUs, frame: &Rc<HeldFrame>, victim: Victim| {
            if low >= priority { return; }
            let pointer = Rc::as_ptr(frame);
            match groups.iter_mut().find(|group| group.frame == pointer) {
                Some(group) => {
                    group.priority = group.priority.max(low);
                    group.at = group.at.max(at);
                    group.victims.push(victim);
                }
                None => groups.push(Group { priority: low, at, frame: pointer, holders: Rc::strong_count(frame), victims: vec![victim] }),
            }
        };
        if !private_only {
            for (index, slot) in self.public.iter().enumerate() {
                if let State::Selected(Selected::Retained(frame)) = &slot.state {
                    add(slot.priority(), slot.at, frame, Victim::Public(index));
                }
            }
            if let Some(quiet) = &self.quiet {
                add(Settled, quiet.since, &quiet.candidate, Victim::Quiet);
            }
            for (index, scroll) in self.scrolls.iter().enumerate() {
                if let Some(frame) = scroll.next.retained() {
                    add(Next, scroll.end, frame, Victim::Scroll(index));
                }
            }
        }
        for (index, entry) in self.pending.iter().enumerate() {
            let at = entry.opening.request.time.recv_t_us;
            if let PrivateSelection::Next(selected) = &entry.selection && let Some(frame) = selected.retained() {
                add(Next, at, frame, Victim::PrivateNext(index));
            }
            if let Some(frame) = entry.scroll_next.retained() {
                add(Next, at, frame, Victim::PrivateScroll(index));
            }
        }
        let Some(group) = groups.into_iter().filter(|group| private_only || group.victims.len() == group.holders)
            .min_by_key(|group| (group.priority, std::cmp::Reverse(group.at))) else { return false };
        for victim in group.victims {
            match victim {
                Victim::Quiet => {
                    self.quiet = None;
                    self.drop_settling(DropReason::SettledRetentionPressure, |_| true);
                }
                Victim::Public(index) => self.public[index].state = State::dropped(DropReason::RetentionLowerPriority),
                Victim::Scroll(index) => self.scrolls[index].next = Selected::Capacity,
                Victim::PrivateNext(index) => self.pending[index].selection = PrivateSelection::Next(Selected::Capacity),
                Victim::PrivateScroll(index) => self.pending[index].scroll_next = Selected::Capacity,
            }
        }
        true
    }

    fn push_public(&mut self, mut slot: Public) {
        let Some(reservation) = slot.reservation.take()
            .or_else(|| self.rows.reserve_public(self.receipt, slot.fallback(DropReason::StillOwnerClosed))) else { return };
        if self.public.len() >= PUBLIC_SLOTS {
            let lowest = self.public.iter().enumerate()
                .filter(|(_, old)| old.priority() < slot.priority()
                    && matches!(old.state, State::Settling | State::Selected(_)))
                .min_by_key(|(index, old)| (old.priority(), std::cmp::Reverse(*index)))
                .map(|(index, _)| index);
            if let Some(mut evicted) = lowest.and_then(|index| self.public.remove(index)) {
                let row = evicted.row(&SlotOutcome::dropped(DropReason::LowerPriority));
                if let Some(ticket) = evicted.reservation.take() {
                    ticket.finish(row);
                }
            } else {
                reservation.finish(slot.row(&SlotOutcome::dropped(DropReason::StillJobCapacity)));
                return;
            }
        }
        slot.reservation = Some(reservation);
        self.public.push_back(slot);
    }

    fn publish_private(
        &mut self, mut entry: Private, mut permission: Permission, images: &NativeImages,
        config: &ResolvedCaptureConfig,
    ) {
        let request = entry.opening.request;
        // The Receiver sends a permission only for the request with its
        // geometry and foreground proof.
        // The approval guard stays at run time in case that contract breaks.
        let matches = request.geometry == Evidence::Observed(permission.geometry)
            && request.foreground_at_receipt == Some(permission.proof);
        debug_assert!(matches);
        if !matches {
            return;
        }
        let at = permission.at;
        if let Some(interval) = entry.interval {
            self.reselect_at_approval(&mut entry, interval, &permission, images, config);
        }
        let trigger = match request.kind {
            ProvisionalInputKind::PreMotion => Trigger::PreMotion,
            ProvisionalInputKind::PreAction | ProvisionalInputKind::Scroll { .. } => Trigger::PreAction,
            ProvisionalInputKind::Next => Trigger::Next,
        };
        if let ProvisionalInputKind::Scroll { axis } = request.kind && let Some(interval) = entry.interval
            && !self.stage_scroll(&mut entry, &permission, axis, interval, images)
        {
            permission.rows.suppress();
            return;
        }
        if matches!(trigger, Trigger::PreAction | Trigger::Next) {
            self.invalidate_quiet(at);
        }
        let slots = match entry.selection {
            PrivateSelection::Preceding { adjustment, before, seen } => [
                (trigger, Role::Before, Some(adjustment), State::Selected(before)),
                (trigger, Role::Seen, Some(adjustment), State::Selected(seen)),
            ],
            PrivateSelection::Next(selected) => [
                (trigger, Role::Image, None, State::Selected(selected)),
                (Trigger::Settled, Role::Image, None, State::Settling),
            ],
            PrivateSelection::Unavailable(reason) if matches!(trigger, Trigger::Next) => [
                (trigger, Role::Image, None, State::dropped(reason)),
                (Trigger::Settled, Role::Image, None, State::dropped(reason)),
            ],
            PrivateSelection::Unavailable(reason) => [
                (trigger, Role::Before, None, State::dropped(reason)),
                (trigger, Role::Seen, None, State::dropped(reason)),
            ],
        };
        for (index, (trigger, role, adjustment, state)) in slots.into_iter().enumerate() {
            let Some(reservation) = permission.rows.take(index) else { continue };
            self.push_public(Public {
                event: Some(permission.event), interval: entry.interval, adjustment, reservation: Some(reservation),
                ..Public::new(trigger, role, at, permission.geometry, request.admission, state)
            });
        }
    }

    /// Reselect a pending entry's frames at its approved action time, where
    /// that differs from the receipt.
    fn reselect_at_approval(&mut self, entry: &mut Private, interval: Interval, permission: &Permission,
        images: &NativeImages, config: &ResolvedCaptureConfig,
    ) {
        let at = permission.at;
        let received = entry.opening.request.time.recv_t_us;
        let refresh = entry.refresh;
        match &mut entry.selection {
            PrivateSelection::Preceding { adjustment, before, seen } => {
                let Ok(adjusted) = Adjustment::from_receipt(at, calibration(config, permission.device, interval)) else {
                    entry.selection = PrivateSelection::Unavailable(DropReason::AdjustmentFailed);
                    return;
                };
                if adjusted.boundary != adjustment.boundary {
                    let pair = still_time::preceding(images, interval, adjusted, refresh);
                    *before = Selected::Waiting;
                    *seen = Selected::Waiting;
                    *before = self.retain(pair.before, still_png::Priority::BeforeSeen);
                    *seen = self.retain(pair.seen, still_png::Priority::BeforeSeen);
                }
                *adjustment = adjusted;
            }
            PrivateSelection::Next(selected) if at != received => {
                *selected = self.select_next(images, interval, at);
            }
            _ => {}
        }
    }

    /// Join an approved scroll tick to its burst (ALG §8.3). False when the
    /// tick only extends a running burst, whose slots stand for it.
    fn stage_scroll(&mut self, entry: &mut Private, permission: &Permission, axis: WheelAxis, interval: Interval,
        images: &NativeImages,
    ) -> bool {
        let at = permission.at;
        // Ticks without a device form one unknown-device burst per monitor and axis;
        // known devices are never merged with each other (ALG §8.3).
        let existing = self.scrolls.iter().position(|scroll| {
            scroll.interval == interval && scroll.device == permission.device && scroll.axis == axis
        });
        let end = SessionTimeUs(at.0.saturating_add(SCROLL_US));
        let next = if at == entry.opening.request.time.recv_t_us {
            std::mem::replace(&mut entry.scroll_next, Selected::Waiting)
        } else {
            Selected::Waiting
        };
        let burst = Scroll { device: permission.device, axis, interval, end, event: permission.event, next };
        if let Some(index) = existing {
            let old = std::mem::replace(&mut self.scrolls[index], burst);
            if at < old.end {
                self.invalidate_quiet(at);
                return false;
            }
            self.finish_scroll(old, images);
        } else if self.scrolls.len() < MAX_SCROLLS {
            self.scrolls.push(burst);
        } else {
            // Terminal rows need journal reservations, but no still job slots.
            let reason = DropReason::ScrollBurstCapacity;
            for slot in burst.dropped_observations(reason) {
                if let Some(reservation) = self.rows.reserve_public(self.receipt, slot.fallback(reason)) {
                    reservation.finish(slot.row(&SlotOutcome::dropped(reason)));
                }
            }
        }
        true
    }

    pub fn advance_idle(&mut self, through: SessionTimeUs) {
        self.receipt = self.receipt.max(through);
        self.rows.advance(self.receipt);
    }

    pub fn expire_native_epoch(&mut self) {
        for row in &mut self.public {
            let reading = matches!(&row.state, State::Work(work) if matches!(work.borrow().stage, WorkStage::Readback(_)));
            if reading || matches!(row.state, State::Selected(Selected::Retained(_))) {
                row.state = State::dropped(DropReason::NativeCaptureEpochClosed);
            }
        }
    }

    pub fn geometry_changed(&mut self, geometry: GeometryId) {
        if let Some(mut start) = self.start.take() {
            start.state = State::dropped(DropReason::GeometryChangedBeforeStartFrame);
            self.push_public(start);
        }
        for row in &mut self.public {
            if row.geometry != geometry && matches!(row.state, State::Settling | State::Selected(Selected::Waiting)) {
                row.state = State::dropped(DropReason::GeometryChangedBeforeSelection);
            }
        }
        for scroll in std::mem::take(&mut self.scrolls) {
            self.drop_scroll(&scroll, DropReason::GeometryChangedBeforeScrollEnd);
        }
        self.quiet = None;
        self.quiet_after = None;
        self.observed_frame = None;
        self.change_at = None;
    }

    pub fn finish_boundary(
        &mut self, prefix: crate::session::AdmissionBoundary, cutoff: crate::session::AdmissionBoundary,
        images: &NativeImages, mode: &MonitorCapture, stopping: bool, capture_available: bool,
    ) {
        let at_boundary = |stop, pause| DropReason::at_boundary(stopping, stop, pause);
        self.receipt = self.receipt.max(cutoff.time);
        if stopping {
            let latest = capture_available.then(|| images.history().iter().rev().find(|frame| {
                frame.metadata.covered()
                    && frame.metadata.geometry.id == mode.geometry.id && frame.metadata.id.epoch == mode.capture_epoch
                    && frame.metadata.admission == prefix.epoch && frame.metadata.source_t_us < prefix.time
                    && frame.metadata.recv_t_us < prefix.time
            })).flatten();
            let state = latest.map_or_else(
                || State::dropped(DropReason::NoEligibleFinalFrameInCaptureEpoch),
                |frame| State::Selected(self.retain(Ok(frame), still_png::Priority::Final)),
            );
            self.push_public(Public::new(Trigger::Final, Role::Image, cutoff.time, mode.geometry.id, prefix.epoch, state));
        }
        while let Some(command) = self.deferred_save.take().or_else(|| self.saves.try_recv().ok()) {
            let save = command.confirmation;
            let (trigger, role) = save_slot(save.kind());
            let at = save.time().recv_t_us;
            let state = State::dropped(at_boundary(DropReason::StopBeforeSaveSelection, DropReason::PauseBeforeSaveSelection));
            self.push_public(Public { save: Some(save), reservation: Some(command.reservation),
                ..Public::new(trigger, role, at, command.geometry, command.admission, state) });
        }
        self.quiet = None;
        self.quiet_after = None;
        for slot in &mut self.public {
            if matches!(slot.state, State::Settling | State::Selected(Selected::Waiting))
                || slot.settled_confirmed_t_us.is_some_and(|confirmed| confirmed >= cutoff.time)
            {
                slot.state = State::dropped(at_boundary(DropReason::StopBeforeSelection, DropReason::PauseBeforeSelection));
            }
        }
        for scroll in std::mem::take(&mut self.scrolls) {
            self.drop_scroll(&scroll, at_boundary(DropReason::StopBeforeScrollEnd, DropReason::PauseBeforeScrollEnd));
        }
        if let Some(mut start) = self.start.take() {
            start.state = State::dropped(at_boundary(DropReason::StopBeforeStartFrame, DropReason::PauseBeforeStartFrame));
            self.push_public(start);
        }
        self.observed_frame = None;
        self.change_at = None;
        self.observe_at = SessionTimeUs(cutoff.time.0.saturating_add(OBSERVE_US));
    }

    pub fn finish_private_prefix(
        &mut self, cutoff: crate::session::AdmissionBoundary, images: &NativeImages,
        config: &ResolvedCaptureConfig, terminal: bool,
    ) {
        // Deliver only already approved requests with a reserved input
        // reference. At a boundary we never advance a new quiet window.
        let count = self.pending.len();
        for _ in 0..count {
            let Some(entry) = self.pending.pop_front() else { break };
            match entry.opening.decision.try_recv() {
                Ok(permission) if permission.at <= cutoff.time => {
                    self.publish_private(entry, permission, images, config);
                }
                Err(TryRecvError::Empty) if !terminal => self.pending.push_back(entry),
                _ => {}
            }
        }
        if terminal {
            self.deferred = None;
            for _ in 0..CHANNEL { if self.input.try_recv().is_err() { break; } }
        }
        self.quiet = None;
        self.quiet_after = None;
        for row in &mut self.public {
            if matches!(row.state, State::Settling | State::Selected(Selected::Waiting)) {
                row.state = State::dropped(DropReason::AdmissionClosedBeforeSelection);
            }
        }
        for scroll in std::mem::take(&mut self.scrolls) {
            self.drop_scroll(&scroll, DropReason::AdmissionClosedBeforeScrollEnd);
        }
    }

    pub fn advance_boundary(&self, at: SessionTimeUs) {
        self.rows.advance(SessionTimeUs(at.0.saturating_add(1)));
    }

    pub fn drained(&self) -> bool {
        self.public.is_empty()
    }

    /// Resolve every open slot at the drain deadline. A PNG job that already
    /// finished keeps its result; dropping an unfinished ticket cancels the job
    /// before its write or publication.
    pub fn expire_drain(&mut self, reason: &'static str) {
        for slot in &mut self.public {
            let finished = match &slot.state {
                State::Done(_) => continue,
                State::Work(work) => {
                    let mut work = work.borrow_mut();
                    work.advance(None);
                    work.outcome()
                }
                _ => None,
            };
            if finished.as_ref().is_some_and(|(_, withdrawn)| *withdrawn) {
                slot.frame = None;
            }
            slot.state = State::Done(finished.map_or_else(
                || SlotOutcome::dropped(DropReason::Drain(reason)), |(outcome, _)| outcome,
            ));
        }
    }

    pub fn work(
        &mut self, device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        images: &NativeImages, readbacks: &mut readback::Readbacks, png: &still_png::Inbox,
        now: SessionTimeUs, ticking: Instant,
    ) -> Result<(), ProbeError> {
        self.select_waiting(images, now);
        self.advance_slots(png);
        self.attach_shared_work();
        self.start_readback(device, images, readbacks, ticking)?;
        self.finish_rows();
        Ok(())
    }

    /// Select the next frame of each waiting public slot, up to `NEXT_US`
    /// after its trigger.
    fn select_waiting(&mut self, images: &NativeImages, now: SessionTimeUs) {
        for index in 0..self.public.len() {
            let slot = &self.public[index];
            let (Some(interval), State::Selected(Selected::Waiting)) = (slot.interval, &slot.state) else { continue };
            let at = slot.at;
            let selected = if now.0.saturating_sub(at.0) >= NEXT_US {
                Selected::Missing(Missing::NoEligibleFrame)
            } else {
                self.select_next(images, interval, at)
            };
            self.public[index].state = State::Selected(selected);
        }
    }

    /// Resolve slots without a usable frame and collect finished work.
    fn advance_slots(&mut self, png: &still_png::Inbox) {
        for slot in &mut self.public {
            match &mut slot.state {
                State::Selected(Selected::Retained(frame)) if !frame.metadata.covered() => {
                    slot.state = State::dropped(DropReason::ElevatedFrameUncertified);
                }
                State::Selected(Selected::Capacity) => slot.state = State::dropped(DropReason::RetentionCapacity),
                State::Selected(Selected::Missing(reason)) => {
                    slot.state = State::Done(SlotOutcome::Unavailable { reason: *reason });
                }
                State::Work(work) => {
                    let work = Rc::clone(work);
                    let mut work = work.borrow_mut();
                    work.advance(Some(png));
                    if let Some((outcome, withdrawn)) = work.outcome() {
                        if withdrawn { slot.frame = None; }
                        slot.state = State::Done(outcome);
                    }
                }
                _ => {}
            }
        }
    }

    /// A slot whose held frame already has shared work attaches to it.
    fn attach_shared_work(&mut self) {
        for index in 0..self.public.len() {
            let State::Selected(Selected::Retained(frame)) = &self.public[index].state else { continue };
            let id = frame.metadata.id;
            let work = self.public.iter().find_map(|slot| match &slot.state {
                State::Work(work) if work.borrow().frame == id => Some(Rc::clone(work)),
                _ => None,
            });
            if let Some(work) = work {
                attach(&mut self.public[index], &work);
            }
        }
    }

    /// Start the readback of the highest-priority held frame when no readback
    /// is pending. One readback and one PNG job serve every slot of the frame (C2).
    fn start_readback(&mut self, device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        images: &NativeImages, readbacks: &mut readback::Readbacks, ticking: Instant,
    ) -> Result<(), ProbeError> {
        if readbacks.has_pending() {
            return Ok(());
        }
        let ready = self.public.iter().enumerate()
            .filter(|(_, slot)| matches!(slot.state, State::Selected(Selected::Retained(_))))
            .min_by_key(|(index, slot)| (std::cmp::Reverse(slot.priority()), *index));
        let Some((index, slot)) = ready else { return Ok(()) };
        let State::Selected(Selected::Retained(frame)) = &slot.state else { return Ok(()) };
        // This handle keeps the frame's own hold out of `reclaim`.
        let frame = Rc::clone(frame);
        let priority = slot.priority();
        while !readbacks.fits(&frame) && self.reclaim(priority, false) {}
        let meta = &frame.metadata;
        let request = readback::Request { frame: meta.id,
            geometry: meta.geometry.id, admission: meta.admission, deadline: ticking + READBACK_WAIT };
        match readbacks.try_retained(request, &frame, device, images)? {
            Some(Ok(ticket)) => {
                let work = Rc::new(RefCell::new(FrameWork { frame: meta.id, priority, exempt: false, coverage: None,
                    stage: WorkStage::Readback(ticket) }));
                for slot in &mut self.public {
                    if matches!(&slot.state, State::Selected(Selected::Retained(held)) if held.metadata.id == meta.id) {
                        attach(slot, &work);
                    }
                }
            }
            Some(Err(reason)) => {
                let slot = &mut self.public[index];
                slot.frame = Some(FrameEvidence::new(meta, slot.settled_confirmed_t_us));
                slot.state = State::dropped(reason.into());
            }
            None => {}
        }
        Ok(())
    }

    /// Hand each resolved slot's row to its reservation.
    fn finish_rows(&mut self) {
        self.public.retain_mut(|slot| {
            let State::Done(outcome) = &slot.state else { return true };
            let row = slot.row(outcome);
            if let Some(ticket) = slot.reservation.take() {
                let coverage = slot.frame.as_mut().and_then(|frame| frame.coverage.take());
                ticket.finish_frame(row, coverage);
            }
            false
        });
    }
}

/// The receipt-delay calibration of an ordinary input device. Input of an
/// elevated visit has none.
fn calibration(config: &ResolvedCaptureConfig, device: Option<DeviceId>, interval: Interval) -> Option<DeviceCalibration> {
    device.filter(|_| interval.elevated_visit.is_none()).and_then(|device| match config.calibration(device) {
        Evidence::Observed(calibration) => Some(calibration),
        _ => None,
    })
}

/// The approved fallback row of a still slot (schema §6): the PDF fields
/// with nulls, `seen_obs_id` on a paired row, and `geometry_evidence`, which
/// every approved fallback carries.
pub(super) fn fallback_row(
    trigger: Trigger, role: Role, at: SessionTimeUs, geometry: Evidence<GeometryId>,
    admission: AdmissionEpoch, event: Option<EventRef>, reason: &'static str,
) -> serde_json::Value {
    let observed = match geometry { Evidence::Observed(geometry) => Some(geometry), _ => None };
    let mut row = serde_json::json!({ "kind": "still_dropped",
        "trigger": trigger, "role": role, "trigger_t_us": at, "monitor": observed.map(|geometry| geometry.monitor),
        "geometry": observed, "geometry_evidence": geometry, "admission_epoch": admission, "event_ref": event,
        "source_t_us": null, "frame_obs_id": null, "file": null, "width": null, "height": null, "reason": reason });
    if trigger.paired() {
        row["seen_obs_id"] = serde_json::Value::Null;
    }
    row
}
