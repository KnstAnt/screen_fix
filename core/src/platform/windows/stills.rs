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
    config::ResolvedCaptureConfig,
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

pub(super) struct EarlyRows {
    lane: still_rows::Rows,
    rows: [EarlyRow; 2],
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
    pub fn reserve(lane: &still_rows::Rows, at: SessionTimeUs) -> Self {
        Self { lane: lane.clone(), rows: [EarlyRow::reserve(lane, at), EarlyRow::reserve(lane, at)] }
    }

    pub fn approve(&mut self, kind: ProvisionalInputKind, event: EventRef, geometry: Evidence<GeometryId>,
        admission: AdmissionEpoch, at: SessionTimeUs, missing: &'static str,
    ) {
        let roles = match kind {
            ProvisionalInputKind::PreMotion => [("pre_motion", "before"), ("pre_motion", "seen")],
            ProvisionalInputKind::PreAction | ProvisionalInputKind::Scroll { .. } =>
                [("pre_action", "before"), ("pre_action", "seen")],
            ProvisionalInputKind::Next => [("next", "image"), ("settled", "image")],
        };
        let observed = match geometry { Evidence::Observed(geometry) => Some(geometry), _ => None };
        for (entry, (trigger, role)) in self.rows.iter_mut().zip(roles) {
            let fallback = serde_json::json!({ "kind": "still_dropped", "schema_version": 1,
                "trigger": trigger, "role": role, "trigger_t_us": at, "geometry": observed, "geometry_evidence": geometry,
                "monitor": observed.map(|geo| geo.monitor), "admission_epoch": admission, "event_ref": event,
                "source_t_us": null, "file": null, "width": null, "height": null, "reason": missing,
            });
            let lost = match entry {
                // The ordered still deadline expired the slot before approval.
                EarlyRow::Reserved(row) => (!row.approve(fallback)).then_some(LossReason::ProducerStall),
                EarlyRow::Unavailable(reason) => Some(*reason),
                EarlyRow::HandedOff => None,
            };
            if let Some(reason) = lost {
                *entry = EarlyRow::Unavailable(reason);
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
    fn from(frame: Result<&NativeFrame, Missing>, holds: &mut Holds) -> Self {
        match frame {
            Ok(frame) => holds.retain(frame).map_or(Self::Capacity, Self::Retained),
            Err(reason) => Self::Missing(reason),
        }
    }
}

enum PrivateSelection {
    Preceding { adjustment: Adjustment, before: Selected, seen: Selected },
    Next(Selected),
    /// No still can be selected for the approved action, with the reason.
    Unavailable(&'static str),
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
        selection.into_iter().flatten().chain([&self.scroll_next]).filter_map(|selected| match selected {
            Selected::Retained(frame) => Some(frame),
            _ => None,
        })
    }
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Trigger {
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

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Before,
    Seen,
    Image,
    FinalSaveSettled,
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
    Done(serde_json::Value),
}

impl Public {
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

    fn row(&self, outcome: &serde_json::Value) -> serde_json::Value {
        let saved = outcome.get("kind").and_then(serde_json::Value::as_str) == Some("saved");
        let frame = self.frame.as_ref();
        let seen = matches!(self.role, Role::Seen);
        let file = outcome.get("image");
        let frame_obs_id = frame.map(|frame| frame.id.sequence);
        serde_json::json!({
            "schema_version": 1,
            "kind": if saved { "still" } else { "still_dropped" },
            "source_t_us": frame.map(|frame| frame.source_t_us),
            "monitor": self.geometry.monitor,
            "trigger": self.trigger, "role": self.role, "trigger_t_us": self.at,
            "frame_obs_id": frame_obs_id,
            "seen_obs_id": seen.then_some(frame_obs_id).flatten(),
            "event_ref": self.event,
            "file": file,
            "seen_file": seen.then_some(file).flatten(),
            "width": frame.map(|frame| frame.width), "height": frame.map(|frame| frame.height),
            "reason": outcome.get("reason").or_else(|| outcome.get("operation")),
            "geometry": self.geometry, "admission_epoch": self.admission, "scroll_device": self.scroll_device,
            "settled_confirmed_t_us": self.settled_confirmed_t_us, "adjustment": self.adjustment,
            "frame": self.frame, "pointer_element": self.pointer, "outcome": outcome,
            "save": self.save.as_ref().map(|save| serde_json::json!({
                "id": save.id(), "kind": save.kind(), "routing_basis": save.routing_basis(),
            })),
        })
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
    Done { outcome: serde_json::Value, withdrawn: bool },
}

impl FrameWork {
    /// Advance the work. Without the PNG inbox only a finished job is collected.
    fn advance(&mut self, png: Option<&still_png::Inbox>) {
        let done = |outcome: serde_json::Value, withdrawn: bool| WorkStage::Done { outcome, withdrawn };
        self.stage = match &mut self.stage {
            WorkStage::Readback(ticket) => {
                let Some(png) = png else { return };
                let Some(result) = ticket.try_result() else { return };
                match result {
                    Ok(pixels) if pixels.metadata.covered() => match png.request(pixels, self.priority, self.exempt, self.coverage.clone()) {
                        Ok(ticket) => WorkStage::Encoding(ticket),
                        Err(reason) => done(serde_json::json!({ "kind": "dropped", "reason": reason }), false),
                    },
                    Ok(_) => done(dropped("elevated_frame_uncertified"), true),
                    Err(reason) => done(readback_failure(reason), false),
                }
            }
            WorkStage::Encoding(ticket) => match ticket.poll() {
                None => return,
                Some(still_png::Outcome::Saved { reference, reused }) => done(serde_json::json!({
                    "kind": "saved", "image": reference.relative_uri(), "deduplicated": reused,
                }), false),
                Some(still_png::Outcome::Dropped(reason)) => done(serde_json::json!({ "kind": "dropped", "reason": reason }),
                    matches!(reason, still_png::Dropped::ApprovalSuppressed)),
                Some(still_png::Outcome::Failed { operation, native_code }) => done(serde_json::json!({
                    "kind": "failed", "operation": operation, "native_code": native_code,
                }), false),
            },
            WorkStage::Done { .. } => return,
        };
    }

    fn outcome(&self) -> Option<(serde_json::Value, bool)> {
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
    fn dropped_observations(self, outcome: &serde_json::Value) -> [Public; 2] {
        let device = self.device.map_or(Evidence::Unavailable(crate::protocol::UnavailableReason::Ambiguous), Evidence::Observed);
        [Trigger::Next, Trigger::Settled].map(|trigger| Public {
            event: Some(self.event), trigger, role: Role::Image,
            at: self.end, geometry: self.interval.geometry, admission: self.interval.admission,
            interval: Some(self.interval), adjustment: None, frame: None, pointer: None, save: None,
            reservation: None, settled_confirmed_t_us: None, scroll_device: Some(device),
            state: State::Done(outcome.clone()),
        })
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
        let (save_send, saves) = sync_channel(8);
        let last_activity = Arc::new(AtomicI64::new(0));
        let input_before = Arc::new(AtomicI64::new(0));
        let last_action = Arc::new(AtomicI64::new(0));
        (Requests { send, saves: save_send, owner: thread::current(), last_activity: Arc::clone(&last_activity), input_before: Arc::clone(&input_before), last_action: Arc::clone(&last_action) },
         Self { input, rows, receipt: SessionTimeUs(0), saves, deferred: None, deferred_save: None, last_activity, input_before, last_action, seen_action: SessionTimeUs(0), observed_activity: SessionTimeUs(0), observe_at: SessionTimeUs(OBSERVE_US),
             observed_frame: None, change_at: None, scrolls: Vec::with_capacity(MAX_SCROLLS), holds: Holds::new(budget), pending: VecDeque::with_capacity(PRIVATE_SLOTS),
             public: VecDeque::with_capacity(PUBLIC_SLOTS), start: None, quiet_after: None, quiet: None })
    }

    pub fn arm_start(&mut self, boundary: crate::session::AdmissionBoundary, geometry: GeometryId) {
        let mut start = Public { event: None, trigger: Trigger::Start, role: Role::Image,
            at: boundary.time, geometry, admission: boundary.epoch, interval: None, adjustment: None,
            frame: None, pointer: None, save: None, reservation: None, settled_confirmed_t_us: None,
            scroll_device: None, state: State::Selected(Selected::Waiting) };
        match self.rows.reserve(boundary.time, Some(start.row(&dropped("start_frame_unavailable")))) {
            Ok(reservation) => start.reservation = Some(reservation),
            Err(reason) => {
                self.rows.note_loss(boundary.time, reason);
                return;
            }
        }
        self.start = Some(start);
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
        for _ in 0..8 {
            let Some(save) = self.deferred_save.take().or_else(|| self.saves.try_recv().ok()) else { break };
            if save.confirmation.time().recv_t_us > now {
                self.deferred_save = Some(save);
                break;
            }
            let at = save.confirmation.time().recv_t_us;
            let approval = policy.for_receipt(save.proof, admission, save.visit, at, now);
            if save.visit.is_some() && approval.is_err() && now.0.saturating_sub(at.0) < ELEVATED_FRAME_WAIT_US {
                self.deferred_save = Some(save);
                break;
            }
            self.save_step(save, images, mode, admission, approval.ok().flatten());
        }
        let mut openings_drained = false;
        for _ in 0..CHANNEL {
            if self.pending.len() == PRIVATE_SLOTS {
                break;
            }
            let Some(command) = self.deferred.take().or_else(|| self.input.try_recv().ok()) else {
                openings_drained = true;
                break;
            };
            let at = match &command {
                Command::Opening(opening) => opening.request.time.recv_t_us,
                Command::Rest(rest) => rest.at,
            };
            if at > now {
                self.deferred = Some(command);
                openings_drained = true;
                break;
            }
            let (proof, visit) = match &command {
                Command::Opening(opening) => (opening.request.foreground_at_receipt, opening.visit),
                Command::Rest(rest) => (Some(rest.proof), rest.visit),
            };
            let approval = proof.map_or(Ok(None), |proof| policy.for_receipt(proof, admission, visit, at, now));
            if visit.is_some() && approval.is_err() && now.0.saturating_sub(at.0) < ELEVATED_FRAME_WAIT_US {
                self.deferred = Some(command);
                break;
            }
            let approval = approval.ok().flatten();
            let opening = match command {
                Command::Opening(opening) => opening,
                Command::Rest(rest) => {
                    self.rest(rest, images, mode, admission, approval);
                    continue;
                }
            };
            let request = opening.request;
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
                        let calibration = request.device.filter(|_| interval.elevated_visit.is_none())
                            .and_then(|device| match config.calibration(device) {
                            Evidence::Observed(calibration) => Some(calibration),
                            _ => None,
                        });
                        match Adjustment::from_receipt(at, calibration) {
                            Ok(adjustment) => {
                                let pair = still_time::preceding(images, interval, adjustment, refresh);
                                let before = self.retain_private(pair.before, still_png::Priority::BeforeSeen, None);
                                let seen = self.retain_private(pair.seen, still_png::Priority::BeforeSeen, Some(&before));
                                PrivateSelection::Preceding { adjustment, before, seen }
                            }
                            Err(_) => PrivateSelection::Unavailable("adjustment_failed"),
                        }
                    }
                    ProvisionalInputKind::Next => PrivateSelection::Next(self.private_next(images, interval, at)),
                }
            } else {
                PrivateSelection::Unavailable("no_approved_interval")
            };
            self.pending.push_back(Private { opening, interval, selection, scroll_next: Selected::Waiting, refresh });
        }
        // Waiting private selections are made in place, so the private hold
        // cap counts every pending entry.
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
        let approval = policy.current(admission, now);
        let input_before = openings_drained.then_some(input_before);
        self.scroll_step(images, now, input_before);
        self.settle_step(images, mode, admission, approval.clone(), now, input_before);
        self.observe(images, mode, admission, approval, now);
        self.rows.advance(self.receipt);
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
        let mut start = self.start.take().expect("start has a reserved still row");
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
        let ineligible = || State::Done(dropped(
            if command.admission != admission { "admission_closed" } else { "save_interval_changed" }));
        let (trigger, role, state) = match (save.kind(), interval) {
            (SaveKind::Checkpoint, Some(interval)) => {
                let image = still_time::latest(images, interval, at);
                let selected = self.retain(image.ok_or(Missing::NoEligibleFrame), still_png::Priority::Checkpoint);
                (Trigger::Checkpoint, Role::Image, State::Selected(selected))
            }
            (SaveKind::Checkpoint, None) => (Trigger::Checkpoint, Role::Image, ineligible()),
            (SaveKind::Final, Some(_)) => {
                self.invalidate_quiet(at);
                (Trigger::Final, Role::FinalSaveSettled, State::Settling)
            }
            (SaveKind::Final, None) => (Trigger::Final, Role::FinalSaveSettled, ineligible()),
        };
        self.push_public(Public { event: None, trigger, role, at, geometry: command.geometry,
            admission: command.admission, interval, adjustment: None, frame: None, pointer: None, save: Some(save), reservation: Some(command.reservation), settled_confirmed_t_us: None, scroll_device: None, state });
    }

    fn rest(&mut self, rest: Rest, images: &NativeImages, mode: &MonitorCapture,
        admission: AdmissionEpoch, approval: Option<FrameApproval>,
    ) {
        let changed = if rest.admission != admission {
            Some("admission_changed")
        } else if rest.geometry != mode.geometry.id {
            Some("geometry_changed")
        } else {
            None
        };
        let (None, Some(approval)) = (changed, approval) else {
            let reason = changed.unwrap_or("frame_not_approved");
            rest.reservation.finish(dropped_trigger("pointer_rest", "image", rest.at, rest.geometry, rest.admission, reason));
            return;
        };
        let interval = Interval { geometry: rest.geometry, capture: mode.capture_epoch, admission,
            approval: approval.generation, elevated_visit: approval.elevated_visit() };
        let candidate = still_time::latest(images, interval, rest.at).ok_or(Missing::NoEligibleFrame);
        let selected = self.retain(candidate, still_png::Priority::Idle);
        self.push_public(Public { event: None, trigger: Trigger::PointerRest, role: Role::Image,
            at: rest.at, geometry: rest.geometry, admission, interval: Some(interval), adjustment: None,
            frame: None, pointer: rest.pointer, save: None, reservation: Some(rest.reservation), settled_confirmed_t_us: None, scroll_device: None, state: State::Selected(selected) });
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
        self.push_public(Public { event: None, trigger: Trigger::Observe, role: Role::Image,
            at: now, geometry: mode.geometry.id, admission, interval: Some(interval), adjustment: None,
            frame: None, pointer: None, save: None, reservation: None, settled_confirmed_t_us: None, scroll_device: None, state: State::Selected(selected) });
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
                    let burst = &mut self.scrolls[index];
                    burst.next = select_next(images, burst.interval, burst.end, &mut self.holds);
                }
                index += 1;
                continue;
            }
            let scroll = self.scrolls.remove(index);
            if pending_prefix || newer_pending {
                self.drop_scroll(scroll, "scroll_end_uncertified");
            } else {
                self.finish_scroll(scroll, images);
            }
        }
    }

    fn drop_scroll(&mut self, scroll: Scroll, reason: &'static str) {
        for slot in scroll.dropped_observations(&dropped(reason)) {
            self.push_public(slot);
        }
    }

    fn finish_scroll(&mut self, scroll: Scroll, images: &NativeImages) {
        let next = if matches!(scroll.next, Selected::Waiting) {
            select_next(images, scroll.interval, scroll.end, &mut self.holds)
        } else { scroll.next };
        self.invalidate_quiet(scroll.end);
        let device = scroll.device.map_or(Evidence::Unavailable(crate::protocol::UnavailableReason::Ambiguous), Evidence::Observed);
        for (trigger, state) in [(Trigger::Next, State::Selected(next)), (Trigger::Settled, State::Settling)] {
            self.push_public(Public { event: Some(scroll.event), trigger, role: Role::Image,
                at: scroll.end, geometry: scroll.interval.geometry, admission: scroll.interval.admission,
                interval: Some(scroll.interval), adjustment: None, frame: None, pointer: None, save: None,
                reservation: None, settled_confirmed_t_us: None, scroll_device: Some(device), state });
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
        for slot in &mut self.public {
            if !matches!(slot.state, State::Settling) { continue; }
            if slot.interval.is_none() {
                slot.state = State::Done(dropped("no_approved_settled_interval"));
            } else if now.0.saturating_sub(slot.at.0) >= SETTLED_US {
                slot.state = State::Done(dropped("settled_deadline"));
            } else if current.is_some() && slot.interval != current {
                slot.state = State::Done(dropped("capture_interval_changed"));
            }
        }
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
        let mut awaiting_score = false;
        for frame in images.history().iter().filter(|frame| interval.contains(&frame.metadata) && frame.metadata.source_t_us > after) {
            let meta = &frame.metadata;
            if let Some(quiet) = &mut self.quiet {
                if quiet.interval == interval && meta.id.sequence.0 <= quiet.last.sequence.0 {
                    continue;
                }
                if !frame.change_sampled {
                    awaiting_score = true;
                    break;
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
            if let Selected::Retained(candidate) = self.retain(Ok(frame), still_png::Priority::Settled) {
                self.quiet = Some(Quiet { interval, last: meta.id, since: meta.source_t_us, candidate });
            } else {
                for slot in &mut self.public {
                    if slot.interval == current && matches!(slot.state, State::Settling) {
                        slot.state = State::Done(dropped("settled_retention_pressure"));
                    }
                }
                return;
            }
        }
        if !awaiting_score && let Some(quiet) = &self.quiet {
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

    fn private_next(&mut self, images: &NativeImages, interval: Interval, end: SessionTimeUs) -> Selected {
        match still_time::next(images, interval, end) {
            Ok(None) => Selected::Waiting,
            Ok(Some(frame)) => self.retain_private(Ok(frame), still_png::Priority::Next, None),
            Err(reason) => Selected::Missing(reason),
        }
    }

    fn private_holds(&self, extra: Option<&Selected>) -> usize {
        let mut frames: Vec<*const HeldFrame> = Vec::new();
        let extra = extra.and_then(|selected| match selected { Selected::Retained(frame) => Some(frame), _ => None });
        for frame in self.pending.iter().flat_map(Private::held).chain(extra) {
            if !frames.contains(&Rc::as_ptr(frame)) {
                frames.push(Rc::as_ptr(frame));
            }
        }
        frames.len()
    }

    /// Release the hold of the lowest IMG-7 priority below `priority`; among
    /// equal priority the newest goes first (REL-2). Private pending
    /// selections take part; with `private_only` only they do. Preceding
    /// private selections have the highest priority and are never released.
    fn reclaim(&mut self, priority: still_png::Priority, private_only: bool) -> bool {
        use still_png::Priority::{Next, Settled};
        enum Victim { Quiet, Public(usize), Scroll(usize), PrivateNext(usize), PrivateScroll(usize) }
        let retained = |selected: &Selected| matches!(selected, Selected::Retained(_));
        let mut victims = Vec::new();
        if !private_only {
            victims.extend(self.public.iter().enumerate()
                .filter(|(_, slot)| matches!(slot.state, State::Selected(Selected::Retained(_))))
                .map(|(index, slot)| (slot.priority(), slot.at, Victim::Public(index))));
            victims.extend(self.quiet.as_ref().map(|quiet| (Settled, quiet.since, Victim::Quiet)));
            victims.extend(self.scrolls.iter().enumerate().filter(|(_, scroll)| retained(&scroll.next))
                .map(|(index, scroll)| (Next, scroll.end, Victim::Scroll(index))));
        }
        for (index, entry) in self.pending.iter().enumerate() {
            let at = entry.opening.request.time.recv_t_us;
            if matches!(&entry.selection, PrivateSelection::Next(selected) if retained(selected)) {
                victims.push((Next, at, Victim::PrivateNext(index)));
            }
            if retained(&entry.scroll_next) {
                victims.push((Next, at, Victim::PrivateScroll(index)));
            }
        }
        let Some((_, _, victim)) = victims.into_iter().filter(|(low, ..)| *low < priority)
            .min_by_key(|(low, at, _)| (*low, std::cmp::Reverse(*at))) else { return false };
        match victim {
            Victim::Quiet => {
                self.quiet = None;
                for slot in &mut self.public {
                    if matches!(slot.state, State::Settling) {
                        slot.state = State::Done(dropped("settled_retention_pressure"));
                    }
                }
            }
            Victim::Public(index) => self.public[index].state = State::Done(dropped("retention_lower_priority")),
            Victim::Scroll(index) => self.scrolls[index].next = Selected::Capacity,
            Victim::PrivateNext(index) => self.pending[index].selection = PrivateSelection::Next(Selected::Capacity),
            Victim::PrivateScroll(index) => self.pending[index].scroll_next = Selected::Capacity,
        }
        true
    }

    fn push_public(&mut self, mut slot: Public) {
        let reservation = match slot.reservation.take() {
            Some(reservation) => reservation,
            None => {
                match self.rows.reserve(self.receipt, Some(slot.row(&dropped("still_owner_closed")))) {
                    Ok(reservation) => reservation,
                    Err(reason) => {
                        self.rows.note_loss(self.receipt, reason);
                        return;
                    }
                }
            }
        };
        if self.public.len() >= PUBLIC_SLOTS {
            let lowest = self.public.iter().enumerate()
                .filter(|(_, old)| old.priority() < slot.priority()
                    && matches!(old.state, State::Settling | State::Selected(_)))
                .min_by_key(|(index, old)| (old.priority(), std::cmp::Reverse(*index)))
                .map(|(index, _)| index);
            if let Some(mut evicted) = lowest.and_then(|index| self.public.remove(index)) {
                let row = evicted.row(&dropped("lower_priority"));
                if let Some(ticket) = evicted.reservation.take() {
                    ticket.finish(row);
                }
            } else {
                reservation.finish(slot.row(&dropped("still_job_capacity")));
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
        let geometry = permission.geometry;
        if request.geometry != Evidence::Observed(geometry) || request.foreground_at_receipt != Some(permission.proof) {
            return;
        }
        let at = permission.at;
        let event = permission.event;
        if let Some(interval) = entry.interval {
            match &mut entry.selection {
                PrivateSelection::Preceding { adjustment, before, seen } => {
                    let calibration = permission.device.filter(|_| interval.elevated_visit.is_none()).and_then(|device| match config.calibration(device) {
                        Evidence::Observed(calibration) => Some(calibration),
                        _ => None,
                    });
                    if let Ok(adjusted) = Adjustment::from_receipt(at, calibration) {
                        if adjusted.boundary != adjustment.boundary {
                            let pair = still_time::preceding(images, interval, adjusted, entry.refresh);
                            *before = Selected::Waiting;
                            *seen = Selected::Waiting;
                            *before = self.retain(pair.before, still_png::Priority::BeforeSeen);
                            *seen = self.retain(pair.seen, still_png::Priority::BeforeSeen);
                        }
                        *adjustment = adjusted;
                    } else {
                        entry.selection = PrivateSelection::Unavailable("adjustment_failed");
                    }
                }
                PrivateSelection::Next(selected) if at != request.time.recv_t_us => {
                    *selected = select_next(images, interval, at, &mut self.holds);
                }
                _ => {}
            }
        }
        let trigger = match request.kind {
            ProvisionalInputKind::PreMotion => Trigger::PreMotion,
            ProvisionalInputKind::PreAction | ProvisionalInputKind::Scroll { .. } => Trigger::PreAction,
            ProvisionalInputKind::Next => Trigger::Next,
        };
        if let ProvisionalInputKind::Scroll { axis } = request.kind && let Some(interval) = entry.interval {
            // Ticks without a device form one unknown-device burst per monitor and axis;
            // known devices are never merged with each other (ALG §8.3).
            let existing = self.scrolls.iter().position(|scroll| {
                scroll.interval == interval && scroll.device == permission.device && scroll.axis == axis
            });
            let end = SessionTimeUs(at.0.saturating_add(SCROLL_US));
            let next = Scroll { device: permission.device, axis, interval, end, event,
                next: if at == request.time.recv_t_us { entry.scroll_next } else { Selected::Waiting } };
            if let Some(index) = existing {
                let old = std::mem::replace(&mut self.scrolls[index], next);
                if at < old.end {
                    self.invalidate_quiet(at);
                    permission.rows.suppress();
                    return;
                }
                self.finish_scroll(old, images);
            } else if self.scrolls.len() < MAX_SCROLLS {
                self.scrolls.push(next);
            } else {
                // Terminal rows need journal reservations, but no still job slots.
                let outcome = dropped("scroll_burst_capacity");
                for slot in next.dropped_observations(&outcome) {
                    let row = slot.row(&outcome);
                    match self.rows.reserve(self.receipt, Some(row.clone())) {
                        Ok(reservation) => reservation.finish(row),
                        Err(reason) => self.rows.note_loss(self.receipt, reason),
                    }
                }
            }
        }
        if matches!(trigger, Trigger::PreAction | Trigger::Next) {
            self.invalidate_quiet(at);
        }
        let mut add = |index, trigger, role, adjustment, state, rows: &mut EarlyRows| {
            let Some(reservation) = rows.take(index) else { return; };
            self.push_public(Public {
                event: Some(event), trigger, role, at, geometry,
                admission: request.admission, interval: entry.interval, adjustment, frame: None,
                state, pointer: None, save: None,
                reservation: Some(reservation), settled_confirmed_t_us: None, scroll_device: None,
            });
        };
        match entry.selection {
            PrivateSelection::Preceding { adjustment, before, seen } => {
                add(0, trigger, Role::Before, Some(adjustment), State::Selected(before), &mut permission.rows);
                add(1, trigger, Role::Seen, Some(adjustment), State::Selected(seen), &mut permission.rows);
            }
            PrivateSelection::Next(selected) => {
                add(0, trigger, Role::Image, None, State::Selected(selected), &mut permission.rows);
                add(1, Trigger::Settled, Role::Image, None, State::Settling, &mut permission.rows);
            }
            PrivateSelection::Unavailable(reason) if matches!(trigger, Trigger::Next) => {
                add(0, trigger, Role::Image, None, State::Done(dropped(reason)), &mut permission.rows);
                add(1, Trigger::Settled, Role::Image, None, State::Done(dropped(reason)), &mut permission.rows);
            }
            PrivateSelection::Unavailable(reason) => {
                add(0, trigger, Role::Before, None, State::Done(dropped(reason)), &mut permission.rows);
                add(1, trigger, Role::Seen, None, State::Done(dropped(reason)), &mut permission.rows);
            }
        }
    }

    pub fn advance_idle(&mut self, through: SessionTimeUs) {
        self.receipt = self.receipt.max(through);
        self.rows.advance(self.receipt);
    }

    pub fn expire_native_epoch(&mut self) {
        for row in &mut self.public {
            let reading = matches!(&row.state, State::Work(work) if matches!(work.borrow().stage, WorkStage::Readback(_)));
            if reading || matches!(row.state, State::Selected(Selected::Retained(_))) {
                row.state = State::Done(dropped("native_capture_epoch_closed"));
            }
        }
    }

    pub fn geometry_changed(&mut self, geometry: GeometryId) {
        if let Some(mut start) = self.start.take() {
            start.state = State::Done(dropped("geometry_changed_before_start_frame"));
            self.push_public(start);
        }
        for row in &mut self.public {
            if row.geometry != geometry && matches!(row.state, State::Settling | State::Selected(Selected::Waiting)) {
                row.state = State::Done(dropped("geometry_changed_before_selection"));
            }
        }
        for scroll in std::mem::take(&mut self.scrolls) {
            self.drop_scroll(scroll, "geometry_changed_before_scroll_end");
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
        self.receipt = self.receipt.max(cutoff.time);
        if stopping {
            let latest = capture_available.then(|| images.history().iter().rev().find(|frame| {
                frame.metadata.covered()
                    && frame.metadata.geometry.id == mode.geometry.id && frame.metadata.id.epoch == mode.capture_epoch
                    && frame.metadata.admission == prefix.epoch && frame.metadata.source_t_us < prefix.time
                    && frame.metadata.recv_t_us < prefix.time
            })).flatten();
            let state = latest.map_or_else(
                || State::Done(dropped("no_eligible_final_frame_in_capture_epoch")),
                |frame| State::Selected(self.retain(Ok(frame), still_png::Priority::Final)),
            );
            self.push_public(Public { event: None, trigger: Trigger::Final, role: Role::Image,
                at: cutoff.time, geometry: mode.geometry.id, admission: prefix.epoch, interval: None,
                adjustment: None, frame: None, pointer: None, save: None, reservation: None,
                settled_confirmed_t_us: None, scroll_device: None, state });
        }
        while let Some(command) = self.deferred_save.take().or_else(|| self.saves.try_recv().ok()) {
            let save = command.confirmation;
            let (trigger, role) = match save.kind() {
                SaveKind::Checkpoint => (Trigger::Checkpoint, Role::Image),
                SaveKind::Final => (Trigger::Final, Role::FinalSaveSettled),
            };
            self.push_public(Public { event: None, trigger, role, at: save.time().recv_t_us,
                geometry: command.geometry, admission: command.admission, interval: None, adjustment: None,
                frame: None, pointer: None, save: Some(save), reservation: Some(command.reservation), settled_confirmed_t_us: None, scroll_device: None,
                state: State::Done(dropped(if stopping { "stop_before_save_selection" } else { "pause_before_save_selection" })) });
        }
        self.quiet = None;
        self.quiet_after = None;
        for slot in &mut self.public {
            if matches!(slot.state, State::Settling | State::Selected(Selected::Waiting))
                || slot.settled_confirmed_t_us.is_some_and(|confirmed| confirmed >= cutoff.time)
            {
                slot.state = State::Done(dropped(if stopping { "stop_before_selection" } else { "pause_before_selection" }));
            }
        }
        for scroll in std::mem::take(&mut self.scrolls) {
            self.drop_scroll(scroll, if stopping { "stop_before_scroll_end" } else { "pause_before_scroll_end" });
        }
        if let Some(mut start) = self.start.take() {
            start.state = State::Done(dropped(if stopping { "stop_before_start_frame" } else { "pause_before_start_frame" }));
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
                row.state = State::Done(dropped("admission_closed_before_selection"));
            }
        }
        for scroll in std::mem::take(&mut self.scrolls) {
            self.drop_scroll(scroll, "admission_closed_before_scroll_end");
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
            slot.state = State::Done(finished.map_or_else(|| dropped(reason), |(outcome, _)| outcome));
        }
    }

    pub fn work(
        &mut self, device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        images: &NativeImages, readbacks: &mut readback::Readbacks, png: &still_png::Inbox,
        now: SessionTimeUs, ticking: Instant,
    ) -> Result<(), ProbeError> {
        for slot in &mut self.public {
            if let (Some(interval), State::Selected(selected @ Selected::Waiting)) = (slot.interval, &mut slot.state) {
                *selected = if now.0.saturating_sub(slot.at.0) >= NEXT_US {
                    Selected::Missing(Missing::NoEligibleFrame)
                } else {
                    select_next(images, interval, slot.at, &mut self.holds)
                };
            }
            match &mut slot.state {
                State::Selected(Selected::Retained(frame)) if !frame.metadata.covered() => {
                    slot.state = State::Done(dropped("elevated_frame_uncertified"));
                }
                State::Selected(Selected::Capacity) => slot.state = State::Done(dropped("retention_capacity")),
                State::Selected(Selected::Missing(reason)) => slot.state = State::Done(serde_json::json!({
                    "kind": "unavailable", "reason": reason,
                })),
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

        // A slot whose held frame already has shared work attaches to it.
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
        let ready = self.public.iter().enumerate()
            .filter(|(_, slot)| matches!(slot.state, State::Selected(Selected::Retained(_))))
            .min_by_key(|(index, slot)| (std::cmp::Reverse(slot.priority()), *index));
        if !readbacks.has_pending() && let Some((index, slot)) = ready
            && let State::Selected(Selected::Retained(frame)) = &slot.state
        {
            let frame = Rc::clone(frame);
            let priority = slot.priority();
            while !readbacks.fits(&frame) && self.reclaim(priority, false) {}
            let meta = &frame.metadata;
            let request = readback::Request { frame: meta.id,
                geometry: meta.geometry.id, admission: meta.admission, deadline: ticking + READBACK_WAIT };
            match readbacks.try_retained(request, &frame, device, images)? {
                Some(Ok(ticket)) => {
                    // One readback and one PNG job serve every slot of this frame (C2).
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
                    slot.state = State::Done(readback_failure(reason));
                }
                None => {}
            }
        }
        self.public.retain_mut(|slot| {
            let State::Done(outcome) = &slot.state else { return true };
            let row = slot.row(outcome);
            if let Some(ticket) = slot.reservation.take() {
                let coverage = slot.frame.as_mut().and_then(|frame| frame.coverage.take());
                ticket.finish_frame(row, coverage);
            }
            false
        });
        Ok(())
    }
}

fn select_next(images: &NativeImages, interval: Interval, end: SessionTimeUs, holds: &mut Holds) -> Selected {
    match still_time::next(images, interval, end) {
        Ok(None) => Selected::Waiting,
        Ok(Some(frame)) => Selected::from(Ok(frame), holds),
        Err(reason) => Selected::Missing(reason),
    }
}

pub(super) fn dropped_trigger(
    trigger: &'static str, role: &'static str, at: SessionTimeUs, geometry: GeometryId,
    admission: AdmissionEpoch, reason: &'static str,
) -> serde_json::Value {
    serde_json::json!({ "schema_version": 1, "kind": "still_dropped",
        "trigger": trigger, "role": role, "trigger_t_us": at, "monitor": geometry.monitor,
        "geometry": geometry, "admission_epoch": admission, "event_ref": null, "source_t_us": null,
        "file": null, "width": null, "height": null, "reason": reason })
}

fn dropped(reason: &'static str) -> serde_json::Value {
    serde_json::json!({ "kind": "dropped", "reason": reason })
}

fn readback_failure(reason: readback::Rejected) -> serde_json::Value {
    use readback::Rejected::*;
    dropped(match reason {
        Pressure => "readback_capacity",
        EpochMismatch => "capture_interval_changed",
        ApprovalSuppressed => "elevated_frame_uncertified",
        Expired => "readback_deadline",
        Closed => "admission_closed",
        CaptureFailed => "capture_failed",
    })
}
