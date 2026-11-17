//! Private input evidence. Only the focus/publication owner may map an input
//! request to a persisted EventRef.

use std::sync::Arc;

use crate::protocol::{
    AdmissionEpoch, DeviceId, Evidence, GeometryId, MessageTickAnchor, MonitorId, ObservationId, PhysicalPoint, RecordTime,
    RequestId, SessionTimeUs, ipc::CollectedInput,
};

pub(crate) use sampling::{MotionSampler, Sampled};

mod control;
mod evidence;
mod held;
mod matching;
mod normalize;
mod sampling;
pub(crate) use control::{ClassifiedControlInput, ControlChords, ControlDisposition, modifier_of};
pub(crate) use evidence::NormalizedInput;
pub(crate) use held::{
    HeldBoundary, HeldIdentity, HeldInput, Holding, HeldSynchronization, PhysicalHeld, PublishedHeld, SyntheticHeldEdge,
    button_bit, modifier_bit, modifier_vk,
};
pub(crate) use matching::promoted as has_promotion;
pub(crate) use matching::{CorrelatedInput, Correlation, Discarded, PenPromotion, InputGap, InputMatcher, InputStep, ProvisionalInputChange};
pub(crate) use normalize::{Action, Button, Key, Modifier, Motion, ScanCode, ScanPrefix, WheelAxis, startup_modifier};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingInput {
    pub id: RequestId,
    pub admission: AdmissionEpoch,
    pub time: RecordTime,
    pub tick_anchor: Option<MessageTickAnchor>,
    pub packet: Arc<CollectedInput>,
    pub part: u8,
    pub action: Action,
    pub geometry: Evidence<GeometryId>,
    pub observation: Option<ObservationId>,
    pub pen_transition: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputOrigin {
    Hardware,
    Touchpad,
    PenPromoted,
    TouchPromoted,
    Injected,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PointerEvidence {
    pub origin: InputOrigin,
    pub device: Evidence<DeviceId>,
    pub monitor: Evidence<MonitorId>,
    pub geometry: Evidence<GeometryId>,
    pub event_position: Evidence<PhysicalPoint>,
    pub receipt_position: Evidence<PhysicalPoint>,
    /// These are the receipts of the records that carry the two points.
    pub event_position_receipt_t_us: Option<SessionTimeUs>,
    pub receipt_position_t_us: Option<SessionTimeUs>,
}
