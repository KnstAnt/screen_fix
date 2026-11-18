//! Public still slots and the still journal row each one writes (schema §6).

use std::{cell::RefCell, rc::Rc, sync::Arc};

use crate::{
    capture::{
        frame::{ApprovalGeneration, FrameMetadata},
        observation::ProvisionalInputKind,
        save::{MonitorRoutingBasis, SaveKind, SaveObservation},
    },
    package::row::RowKind,
    protocol::{
        AdmissionEpoch, DeviceId, EventRef, Evidence, FrameId, FrameSequence, GeometryId, MonitorId, SaveId, SessionTimeUs,
    },
};

use super::super::{
    context::PointerLink, native::HeldFrame, readback, still_png, still_rows,
    still_time::{Adjustment, Interval, Missing},
};
use super::work::FrameWork;

pub(super) enum Selected {
    Retained(Rc<HeldFrame>),
    Missing(Missing),
    Capacity,
    Waiting,
}

impl Selected {
    pub(super) fn retained(&self) -> Option<&Rc<HeldFrame>> {
        match self { Self::Retained(frame) => Some(frame), _ => None }
    }
}

/// A still row's `trigger` (schema §6).
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::platform::windows) enum Trigger {
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
    pub(super) fn paired(self) -> bool {
        matches!(self, Self::PreMotion | Self::PreAction)
    }
}

/// The trigger of the slots an approved provisional input opens; a scroll
/// tick is a pre-action.
impl From<ProvisionalInputKind> for Trigger {
    fn from(kind: ProvisionalInputKind) -> Self {
        match kind {
            ProvisionalInputKind::PreMotion => Self::PreMotion,
            ProvisionalInputKind::PreAction | ProvisionalInputKind::Scroll { .. } => Self::PreAction,
            ProvisionalInputKind::Next => Self::Next,
        }
    }
}

/// A still row's `role` (schema §6).
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::platform::windows) enum Role {
    Before,
    Seen,
    Image,
    FinalSaveSettled,
}

/// The trigger and role of a save-linked still.
pub(in crate::platform::windows) fn save_slot(kind: SaveKind) -> (Trigger, Role) {
    match kind {
        SaveKind::Checkpoint => (Trigger::Checkpoint, Role::Image),
        SaveKind::Final => (Trigger::Final, Role::FinalSaveSettled),
    }
}

/// The outcome of one still slot. It is serialized only into its row, as
/// `outcome`, and gives the row its `kind`, `file` and `reason` (schema §6).
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum SlotOutcome {
    /// `image` is relative to `raw/`, where the journal lives (D-1).
    Saved { image: Option<String>, deduplicated: bool },
    Dropped { reason: DropReason },
    Failed { operation: &'static str, native_code: Option<i32> },
    Unavailable { reason: Missing },
}

impl SlotOutcome {
    pub(super) fn dropped(reason: DropReason) -> Self {
        Self::Dropped { reason }
    }

    /// The row `reason` of this outcome; a saved still has none.
    fn reason(&self) -> Option<StillReason> {
        match self {
            Self::Saved { .. } => None,
            Self::Dropped { reason } => Some(StillReason::Dropped(*reason)),
            Self::Failed { operation, .. } => Some(StillReason::Failed(operation)),
            Self::Unavailable { reason } => Some(StillReason::Unavailable(*reason)),
        }
    }
}

/// A still row's `reason`: why its slot has no image (schema §6).
#[derive(Clone, Copy, serde::Serialize)]
#[serde(untagged)]
enum StillReason {
    Dropped(DropReason),
    /// The operation that failed.
    Failed(&'static str),
    Unavailable(Missing),
}

/// Why a still slot has no image.
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::platform::windows) enum DropReason {
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
    MonitorRoutingUnknown,
    NativeCaptureEpochClosed,
    NoApprovedInterval,
    NoApprovedSettledInterval,
    NoEligibleFinalFrameInCaptureEpoch,
    PauseBeforeSaveSelection,
    PauseBeforeScrollEnd,
    PauseBeforeSelection,
    PauseBeforeStartFrame,
    PauseDrainDeadline,
    PointerRestHandoffUnavailable,
    ProvisionalRetentionUnavailable,
    ReadbackCapacity,
    ReadbackDeadline,
    RetentionCapacity,
    RetentionLowerPriority,
    SaveHandoffUnavailable,
    SaveIntervalChanged,
    ScrollBurstCapacity,
    ScrollEndUncertified,
    SettledDeadline,
    SettledRetentionPressure,
    StartFrameUnavailable,
    StillJobCapacity,
    /// The slot did not finish by the ordered still deadline.
    StillOrderedDeadline,
    /// The still owner closed before the slot finished.
    StillOwnerClosed,
    StopBeforeSaveSelection,
    StopBeforeScrollEnd,
    StopBeforeSelection,
    StopBeforeStartFrame,
    StopDrainDeadline,
    /// The PNG worker did not save the image.
    #[serde(untagged)]
    Png(still_png::Dropped),
}

impl DropReason {
    /// The reason at a Stop or a pause boundary.
    pub(super) fn at_boundary(stopping: bool, stop: Self, pause: Self) -> Self {
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

pub(super) struct Public {
    pub(super) event: Option<EventRef>,
    pub(super) trigger: Trigger,
    pub(super) role: Role,
    pub(super) at: SessionTimeUs,
    pub(super) geometry: GeometryId,
    pub(super) admission: AdmissionEpoch,
    pub(super) interval: Option<Interval>,
    pub(super) adjustment: Option<Adjustment>,
    pub(super) frame: Option<FrameEvidence>,
    /// The bound the row of an elevated visit's frame is checked against.
    pub(super) coverage: Option<still_rows::StillCoverage>,
    pub(super) state: State,
    pub(super) pointer: Option<PointerLink>,
    pub(super) save: Option<SaveObservation>,
    pub(super) reservation: Option<still_rows::Reservation>,
    pub(super) settled_confirmed_t_us: Option<SessionTimeUs>,
    pub(super) scroll_device: Option<Evidence<DeviceId>>,
}

#[derive(Clone, Copy, serde::Serialize)]
pub(super) struct FrameEvidence {
    id: FrameId,
    source_t_us: SessionTimeUs,
    recv_t_us: SessionTimeUs,
    approval_generation: ApprovalGeneration,
    width: u32,
    height: u32,
    color: crate::capture::frame::CaptureColor,
    source_uncertainty_us: Option<crate::protocol::UncertaintyUs>,
}

pub(super) enum State {
    Settling,
    Selected(Selected),
    Work(Rc<RefCell<FrameWork>>),
    Done(SlotOutcome),
}

impl State {
    pub(super) fn dropped(reason: DropReason) -> Self {
        Self::Done(SlotOutcome::dropped(reason))
    }
}

impl Public {
    /// A slot without an event, interval, frame or reservation yet.
    pub(super) fn new(
        trigger: Trigger, role: Role, at: SessionTimeUs, geometry: GeometryId, admission: AdmissionEpoch, state: State,
    ) -> Self {
        Self { event: None, trigger, role, at, geometry, admission, interval: None, adjustment: None, frame: None,
            coverage: None, state, pointer: None, save: None, reservation: None, settled_confirmed_t_us: None,
            scroll_device: None }
    }

    pub(super) fn priority(&self) -> still_png::Priority {
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

    /// Whether this slot still waits for its frame to be selected.
    pub(super) fn unselected(&self) -> bool {
        matches!(self.state, State::Settling | State::Selected(Selected::Waiting))
    }

    /// Record the evidence of the frame this slot selected, and the bound
    /// its row is checked against when the frame belongs to an elevated visit.
    pub(super) fn record_frame(&mut self, meta: &FrameMetadata) {
        self.frame = Some(FrameEvidence::new(meta));
        self.coverage = meta.coverage.as_ref().map(|coverage| still_rows::StillCoverage {
            visit: Arc::clone(coverage),
            last: self.settled_confirmed_t_us.unwrap_or(meta.source_t_us).max(meta.source_t_us),
        });
    }

    /// Clear the frame evidence of a withdrawn image.
    pub(super) fn withdraw_frame(&mut self) {
        self.frame = None;
        self.coverage = None;
    }

    /// The row of this slot. A seen slot is the second half of its trigger's
    /// row, which `still_rows` joins to the before half (IMG-2).
    pub(super) fn row(&self, outcome: &SlotOutcome) -> StillRow {
        crate::devlog::devlog!("stills", "slot m{} trigger={} role={} trigger_t_us={} outcome={} reason={}",
            self.geometry.monitor.0, json_text(&self.trigger), json_text(&self.role), self.at.0, json_text(outcome),
            json_text(&outcome.reason()));
        self.row_value(outcome)
    }

    fn row_value(&self, outcome: &SlotOutcome) -> StillRow {
        let (kind, file) = match outcome {
            SlotOutcome::Saved { image, .. } => (RowKind::Still, image.clone()),
            _ => (RowKind::StillDropped, None),
        };
        let frame = self.frame;
        StillRow {
            kind, trigger: self.trigger, role: self.role, trigger_t_us: self.at,
            monitor: Some(self.geometry.monitor), geometry: Some(self.geometry), geometry_evidence: None,
            admission_epoch: self.admission, event_ref: self.event,
            source_t_us: frame.map(|frame| frame.source_t_us), frame_obs_id: frame.map(|frame| frame.id.sequence), file,
            width: frame.map(|frame| frame.width), height: frame.map(|frame| frame.height),
            reason: outcome.reason(), fallback_reason: None,
            save: Some(self.save.as_ref().map(SaveRef::of)),
            slot: Some(SlotFields {
                scroll_device: self.scroll_device, settled_confirmed_t_us: self.settled_confirmed_t_us,
                adjustment: self.adjustment, frame, element: self.pointer.clone(), outcome: outcome.clone(),
            }),
            seen: None,
        }
    }

    /// The approved fallback row of this slot's reservation (schema §6).
    pub(super) fn fallback(&self, reason: DropReason) -> StillRow {
        self.row_value(&SlotOutcome::dropped(reason)).fallback(Evidence::Observed(self.geometry))
    }
}

/// The JSON text of a row field, for the developer log.
fn json_text(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

impl FrameEvidence {
    fn new(meta: &FrameMetadata) -> Self {
        Self { id: meta.id, source_t_us: meta.source_t_us, recv_t_us: meta.recv_t_us,
            approval_generation: meta.approval_generation, width: meta.extent.width.get(),
            height: meta.extent.height.get(), color: meta.color, source_uncertainty_us: meta.source_uncertainty_us }
    }
}

/// A still journal row (schema §6). A slot's row carries every slot field,
/// null when unknown; an approved fallback written without its slot carries
/// only what its reservation knew. The row is encoded once, at submit.
#[derive(serde::Serialize)]
pub(in crate::platform::windows) struct StillRow {
    kind: RowKind,
    trigger: Trigger,
    role: Role,
    trigger_t_us: SessionTimeUs,
    monitor: Option<MonitorId>,
    geometry: Option<GeometryId>,
    /// Carried by every approved fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    geometry_evidence: Option<Evidence<GeometryId>>,
    admission_epoch: AdmissionEpoch,
    event_ref: Option<EventRef>,
    source_t_us: Option<SessionTimeUs>,
    frame_obs_id: Option<FrameSequence>,
    /// Relative to `raw/`, where the journal lives (D-1).
    file: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    reason: Option<StillReason>,
    /// The own reason of an approved fallback written at the ordered deadline or at close.
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_reason: Option<StillReason>,
    /// A slot's row names its save, or null; a fallback written without its
    /// slot names only the save it was reserved for.
    #[serde(skip_serializing_if = "Option::is_none")]
    save: Option<Option<SaveRef>>,
    #[serde(flatten)]
    slot: Option<SlotFields>,
    #[serde(flatten)]
    seen: Option<Seen>,
}

/// The fields of a still row that only its slot writes.
#[derive(serde::Serialize)]
struct SlotFields {
    scroll_device: Option<Evidence<DeviceId>>,
    settled_confirmed_t_us: Option<SessionTimeUs>,
    adjustment: Option<Adjustment>,
    frame: Option<FrameEvidence>,
    element: Option<PointerLink>,
    outcome: SlotOutcome,
}

/// The seen half of a pre-motion or pre-action row (IMG-2). Its approved
/// fallback has only a null `seen_obs_id`.
#[derive(serde::Serialize)]
struct Seen {
    seen_obs_id: Option<FrameSequence>,
    #[serde(flatten)]
    joined: Option<SeenHalf>,
}

/// What the seen half's own row adds when it joins the before half.
#[derive(serde::Serialize)]
struct SeenHalf {
    /// Only when it differs from the row's `file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    seen_file: Option<String>,
    seen_frame: Option<FrameEvidence>,
    seen_outcome: Option<SlotOutcome>,
}

/// The save a save-linked still row references (schema §6).
#[derive(Clone, Copy, serde::Serialize)]
struct SaveRef {
    id: SaveId,
    kind: SaveKind,
    routing_basis: MonitorRoutingBasis,
}

impl SaveRef {
    fn of(save: &SaveObservation) -> Self {
        Self { id: save.id(), kind: save.kind(), routing_basis: save.routing_basis() }
    }
}

impl StillRow {
    /// The encoded journal row.
    pub(in crate::platform::windows) fn value(&self) -> serde_json::Result<serde_json::Value> {
        serde_json::to_value(self)
    }

    /// This row as an approved fallback, which carries `geometry_evidence`.
    fn fallback(mut self, geometry: Evidence<GeometryId>) -> Self {
        self.geometry_evidence = Some(geometry);
        self
    }

    /// This fallback row, reserved for a confirmed `save`.
    pub(in crate::platform::windows) fn with_save(mut self, save: &SaveObservation) -> Self {
        self.save = Some(Some(SaveRef::of(save)));
        self
    }

    /// Withdraw the image of a still past its elevated visit's certified
    /// bound: the row becomes a dropped row without frame evidence.
    pub(in crate::platform::windows) fn withdraw_uncertified(&mut self) {
        // Only slot rows carry a frame coverage, so only they are withdrawn.
        debug_assert!(self.slot.is_some(), "an uncertified still row must be a slot row");
        let reason = DropReason::ElevatedFrameUncertified;
        self.kind = RowKind::StillDropped;
        self.source_t_us = None;
        self.frame_obs_id = None;
        self.file = None;
        self.width = None;
        self.height = None;
        self.reason = Some(StillReason::Dropped(reason));
        if let Some(slot) = &mut self.slot {
            slot.frame = None;
            slot.settled_confirmed_t_us = None;
            slot.outcome = SlotOutcome::dropped(reason);
        }
    }

    /// One row per trigger (IMG-2): this before half's row with the `seen`
    /// half's frame, and its file only when it differs from this row's file.
    pub(in crate::platform::windows) fn join_seen(mut self, seen: Self) -> Self {
        let seen_file = seen.file.filter(|file| self.file.as_ref() != Some(file));
        let (seen_frame, seen_outcome) = seen.slot.map_or((None, None), |slot| (slot.frame, Some(slot.outcome)));
        self.seen = Some(Seen { seen_obs_id: seen.frame_obs_id, joined: Some(SeenHalf { seen_file, seen_frame, seen_outcome }) });
        self
    }

    /// An approved fallback written because its slot missed the ordered
    /// deadline, or the owner closed first: its own reason moves to `fallback_reason`.
    pub(in crate::platform::windows) fn deadline(&mut self, finishing: bool) {
        let reason = if finishing { DropReason::StillOwnerClosed } else { DropReason::StillOrderedDeadline };
        self.fallback_reason = self.reason.replace(StillReason::Dropped(reason));
    }

    /// The image files this row names, relative to `raw/`.
    pub(in crate::platform::windows) fn images(&self) -> impl Iterator<Item = &str> {
        let seen = self.seen.as_ref().and_then(|seen| seen.joined.as_ref()).and_then(|joined| joined.seen_file.as_deref());
        self.file.as_deref().into_iter().chain(seen)
    }
}

/// The approved fallback row of a still slot (schema §6): the PDF fields
/// with nulls, `seen_obs_id` on a paired row, and `geometry_evidence`, which
/// every approved fallback carries.
pub(in crate::platform::windows) fn fallback_row(
    trigger: Trigger, role: Role, at: SessionTimeUs, geometry: Evidence<GeometryId>,
    admission: AdmissionEpoch, event: Option<EventRef>, reason: DropReason,
) -> StillRow {
    let observed = geometry.observed();
    StillRow {
        kind: RowKind::StillDropped, trigger, role, trigger_t_us: at, monitor: observed.map(|geometry| geometry.monitor),
        geometry: observed, geometry_evidence: None, admission_epoch: admission, event_ref: event,
        source_t_us: None, frame_obs_id: None, file: None, width: None, height: None,
        reason: Some(StillReason::Dropped(reason)), fallback_reason: None, save: None, slot: None,
        seen: trigger.paired().then_some(Seen { seen_obs_id: None, joined: None }),
    }.fallback(geometry)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        capture::frame::CaptureColor,
        protocol::{CaptureEpoch, GeometryEpoch, StreamId, StreamSequence, UnavailableReason},
    };

    const GEOMETRY: GeometryId = GeometryId { monitor: MonitorId(1), epoch: GeometryEpoch(2) };

    fn slot() -> Public {
        Public::new(Trigger::PreAction, Role::Before, SessionTimeUs(1000), GEOMETRY, AdmissionEpoch(3), State::Settling)
    }

    fn frame(sequence: u64) -> FrameEvidence {
        FrameEvidence {
            id: FrameId { monitor: MonitorId(1), epoch: CaptureEpoch(5), sequence: FrameSequence(sequence) },
            source_t_us: SessionTimeUs(900), recv_t_us: SessionTimeUs(950), approval_generation: ApprovalGeneration(6),
            width: 8, height: 6, color: CaptureColor::Sdr, source_uncertainty_us: None,
        }
    }

    fn saved(image: &str, deduplicated: bool) -> SlotOutcome {
        SlotOutcome::Saved { image: Some(image.into()), deduplicated }
    }

    /// The saved row of a before slot with its frame and settled confirmation.
    pub(in crate::platform::windows) fn saved_row() -> StillRow {
        let slot = Public { frame: Some(frame(4)), settled_confirmed_t_us: Some(SessionTimeUs(980)), ..slot() };
        slot.row_value(&saved("images/a.png", false))
    }

    /// A slot's row before a frame is selected: saved, dropped and its approved fallback.
    #[test]
    fn row_value_bytes() -> serde_json::Result<()> {
        let mut slot = slot();
        let saved = SlotOutcome::Saved { image: Some("images/a.png".into()), deduplicated: false };
        assert_eq!(serde_json::to_string(&slot.row_value(&saved).value()?)?,
            r#"{"adjustment":null,"admission_epoch":3,"element":null,"event_ref":null,"file":"images/a.png","frame":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"height":null,"kind":"still","monitor":1,"outcome":{"deduplicated":false,"image":"images/a.png","kind":"saved"},"reason":null,"role":"before","save":null,"scroll_device":null,"settled_confirmed_t_us":null,"source_t_us":null,"trigger":"pre_action","trigger_t_us":1000,"width":null}"#);
        assert_eq!(serde_json::to_string(&slot.row_value(&SlotOutcome::dropped(DropReason::FrameNotApproved)).value()?)?,
            r#"{"adjustment":null,"admission_epoch":3,"element":null,"event_ref":null,"file":null,"frame":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"height":null,"kind":"still_dropped","monitor":1,"outcome":{"kind":"dropped","reason":"frame_not_approved"},"reason":"frame_not_approved","role":"before","save":null,"scroll_device":null,"settled_confirmed_t_us":null,"source_t_us":null,"trigger":"pre_action","trigger_t_us":1000,"width":null}"#);
        slot.event = Some(EventRef { stream: StreamId::Input, seq: StreamSequence(7) });
        assert_eq!(serde_json::to_string(&slot.fallback(DropReason::StillOwnerClosed).value()?)?,
            r#"{"adjustment":null,"admission_epoch":3,"element":null,"event_ref":{"seq":7,"stream":"input"},"file":null,"frame":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"geometry_evidence":{"detail":{"epoch":2,"monitor":1},"status":"observed"},"height":null,"kind":"still_dropped","monitor":1,"outcome":{"kind":"dropped","reason":"still_owner_closed"},"reason":"still_owner_closed","role":"before","save":null,"scroll_device":null,"settled_confirmed_t_us":null,"source_t_us":null,"trigger":"pre_action","trigger_t_us":1000,"width":null}"#);
        Ok(())
    }

    /// The approved fallback of a slot that never existed: paired and unpaired.
    #[test]
    fn fallback_row_bytes() -> serde_json::Result<()> {
        let paired = fallback_row(Trigger::PreMotion, Role::Before, SessionTimeUs(500), Evidence::Observed(GEOMETRY),
            AdmissionEpoch(3), None, DropReason::NoApprovedInterval);
        assert_eq!(serde_json::to_string(&paired.value()?)?,
            r#"{"admission_epoch":3,"event_ref":null,"file":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"geometry_evidence":{"detail":{"epoch":2,"monitor":1},"status":"observed"},"height":null,"kind":"still_dropped","monitor":1,"reason":"no_approved_interval","role":"before","seen_obs_id":null,"source_t_us":null,"trigger":"pre_motion","trigger_t_us":500,"width":null}"#);
        let event = Some(EventRef { stream: StreamId::Input, seq: StreamSequence(9) });
        let unpaired = fallback_row(Trigger::Checkpoint, Role::Image, SessionTimeUs(500),
            Evidence::Unavailable(UnavailableReason::Ambiguous), AdmissionEpoch(3), event, DropReason::AdmissionClosed);
        assert_eq!(serde_json::to_string(&unpaired.value()?)?,
            r#"{"admission_epoch":3,"event_ref":{"seq":9,"stream":"input"},"file":null,"frame_obs_id":null,"geometry":null,"geometry_evidence":{"detail":"ambiguous","status":"unavailable"},"height":null,"kind":"still_dropped","monitor":null,"reason":"admission_closed","role":"image","source_t_us":null,"trigger":"checkpoint","trigger_t_us":500,"width":null}"#);
        Ok(())
    }

    /// A saved slot's row with its frame evidence.
    #[test]
    fn saved_row_bytes() -> serde_json::Result<()> {
        assert_eq!(serde_json::to_string(&saved_row().value()?)?,
            r#"{"adjustment":null,"admission_epoch":3,"element":null,"event_ref":null,"file":"images/a.png","frame":{"approval_generation":6,"color":"sdr","height":6,"id":{"epoch":5,"monitor":1,"sequence":4},"recv_t_us":950,"source_t_us":900,"source_uncertainty_us":null,"width":8},"frame_obs_id":4,"geometry":{"epoch":2,"monitor":1},"height":6,"kind":"still","monitor":1,"outcome":{"deduplicated":false,"image":"images/a.png","kind":"saved"},"reason":null,"role":"before","save":null,"scroll_device":null,"settled_confirmed_t_us":980,"source_t_us":900,"trigger":"pre_action","trigger_t_us":1000,"width":8}"#);
        Ok(())
    }

    /// The seen half joins the before half's row (IMG-2), naming its file
    /// only when it differs.
    #[test]
    fn join_seen_bytes() -> serde_json::Result<()> {
        const BEFORE: &str = r#"{"adjustment":null,"admission_epoch":3,"element":null,"event_ref":null,"file":"images/a.png","frame":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"height":null,"kind":"still","monitor":1,"outcome":{"deduplicated":false,"image":"images/a.png","kind":"saved"},"reason":null,"role":"before","save":null,"scroll_device":null,"#;
        const AFTER: &str = r#""settled_confirmed_t_us":null,"source_t_us":null,"trigger":"pre_action","trigger_t_us":1000,"width":null}"#;
        const FRAME: &str = r#"{"approval_generation":6,"color":"sdr","height":6,"id":{"epoch":5,"monitor":1,"sequence":5},"recv_t_us":950,"source_t_us":900,"source_uncertainty_us":null,"width":8}"#;
        let before = || slot().row_value(&saved("images/a.png", false));
        let seen = Public { role: Role::Seen, frame: Some(frame(5)), ..slot() };
        let joined = before().join_seen(seen.row_value(&saved("images/b.png", false)));
        assert_eq!(serde_json::to_string(&joined.value()?)?, format!(
            r#"{BEFORE}"seen_file":"images/b.png","seen_frame":{FRAME},"seen_obs_id":5,"seen_outcome":{{"deduplicated":false,"image":"images/b.png","kind":"saved"}},{AFTER}"#));
        let joined = before().join_seen(seen.row_value(&saved("images/a.png", true)));
        assert_eq!(serde_json::to_string(&joined.value()?)?, format!(
            r#"{BEFORE}"seen_frame":{FRAME},"seen_obs_id":5,"seen_outcome":{{"deduplicated":true,"image":"images/a.png","kind":"saved"}},{AFTER}"#));
        let dropped = Public { role: Role::Seen, ..slot() }.row_value(&SlotOutcome::dropped(DropReason::LowerPriority));
        assert_eq!(serde_json::to_string(&before().join_seen(dropped).value()?)?, format!(
            r#"{BEFORE}"seen_frame":null,"seen_obs_id":null,"seen_outcome":{{"kind":"dropped","reason":"lower_priority"}},{AFTER}"#));
        Ok(())
    }

    /// An approved fallback written at the ordered deadline or at close.
    #[test]
    fn deadline_bytes() -> serde_json::Result<()> {
        let fallback = || fallback_row(Trigger::PreMotion, Role::Before, SessionTimeUs(500), Evidence::Observed(GEOMETRY),
            AdmissionEpoch(3), None, DropReason::NoApprovedInterval);
        let mut row = fallback();
        row.deadline(false);
        assert_eq!(serde_json::to_string(&row.value()?)?,
            r#"{"admission_epoch":3,"event_ref":null,"fallback_reason":"no_approved_interval","file":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"geometry_evidence":{"detail":{"epoch":2,"monitor":1},"status":"observed"},"height":null,"kind":"still_dropped","monitor":1,"reason":"still_ordered_deadline","role":"before","seen_obs_id":null,"source_t_us":null,"trigger":"pre_motion","trigger_t_us":500,"width":null}"#);
        let mut row = fallback();
        row.deadline(true);
        assert_eq!(serde_json::to_string(&row.value()?)?,
            r#"{"admission_epoch":3,"event_ref":null,"fallback_reason":"no_approved_interval","file":null,"frame_obs_id":null,"geometry":{"epoch":2,"monitor":1},"geometry_evidence":{"detail":{"epoch":2,"monitor":1},"status":"observed"},"height":null,"kind":"still_dropped","monitor":1,"reason":"still_owner_closed","role":"before","seen_obs_id":null,"source_t_us":null,"trigger":"pre_motion","trigger_t_us":500,"width":null}"#);
        Ok(())
    }

    /// The drop reasons named outside the slot pipeline keep their strings.
    #[test]
    fn drop_reason_names() {
        let names = [
            (DropReason::StopDrainDeadline, "stop_drain_deadline"),
            (DropReason::PauseDrainDeadline, "pause_drain_deadline"),
            (DropReason::SaveHandoffUnavailable, "save_handoff_unavailable"),
            (DropReason::PointerRestHandoffUnavailable, "pointer_rest_handoff_unavailable"),
            (DropReason::ProvisionalRetentionUnavailable, "provisional_retention_unavailable"),
            (DropReason::MonitorRoutingUnknown, "monitor_routing_unknown"),
            (DropReason::StillOrderedDeadline, "still_ordered_deadline"),
        ];
        for (reason, name) in names {
            assert_eq!(serde_json::to_value(reason).ok(), Some(serde_json::Value::from(name)));
        }
    }
}
