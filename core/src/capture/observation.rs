use std::fmt;

use crate::protocol::{
    AdmissionEpoch, DeviceId, Evidence, FrameId, GeometryId, ObservationId, PhysicalPoint, RecordTime, RequestId, SessionTimeUs, SourceSequence,
    ipc::{ForegroundAtReceipt, Producer},
};

use super::{DropReason, save::SaveObservation};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationTrigger {
    Start,
    PreMotion { input: RequestId },
    PointerRest,
    PreAction { input: RequestId },
    Next { input: RequestId },
    Settled { input: RequestId },
    Observe,
    ConfirmedSave(SaveObservation),
    Final,
}

/// Input uses only a private ID and direct receipt evidence here. P10 can
/// retain candidates from this envelope before matching or focus resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProvisionalInputObservation {
    pub id: ObservationId,
    pub admission: AdmissionEpoch,
    pub time: RecordTime,
    pub kind: ProvisionalInputKind,
    pub geometry: Evidence<GeometryId>,
    pub position: Evidence<PhysicalPoint>,
    pub device: Option<DeviceId>,
    pub foreground_at_receipt: Option<ForegroundAtReceipt>,
    pub producer: Producer,
    pub source_sequence: SourceSequence,
    pub packet_part: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProvisionalInputKind {
    PreMotion,
    PreAction,
    Next,
    Scroll { axis: super::input::WheelAxis },
}

/// A candidate may reserve a frame before input publication is decided.
/// The input request ID is private and is resolved to a journal reference only on publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRequest {
    id: ObservationId,
    trigger: ObservationTrigger,
    geometry: GeometryId,
    admission: AdmissionEpoch,
    trigger_t_us: SessionTimeUs,
    deadline_t_us: SessionTimeUs,
}

impl ObservationRequest {
    pub fn new(
        id: ObservationId,
        trigger: ObservationTrigger,
        geometry: GeometryId,
        admission: AdmissionEpoch,
        trigger_t_us: SessionTimeUs,
        deadline_t_us: SessionTimeUs,
    ) -> Result<Self, ObservationRequestError> {
        if deadline_t_us < trigger_t_us {
            return Err(ObservationRequestError::ReversedTimes);
        }
        if let ObservationTrigger::ConfirmedSave(save) = &trigger {
            if save.monitor() != geometry.monitor || save.time().recv_t_us != trigger_t_us {
                return Err(ObservationRequestError::MismatchedSave);
            }
        }
        Ok(Self { id, trigger, geometry, admission, trigger_t_us, deadline_t_us })
    }

    pub fn id(&self) -> ObservationId {
        self.id
    }

    pub fn trigger(&self) -> &ObservationTrigger {
        &self.trigger
    }

    pub fn geometry(&self) -> GeometryId {
        self.geometry
    }

    pub fn admission(&self) -> AdmissionEpoch {
        self.admission
    }

    pub fn trigger_time(&self) -> SessionTimeUs {
        self.trigger_t_us
    }

    pub fn deadline(&self) -> SessionTimeUs {
        self.deadline_t_us
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationRequestError {
    ReversedTimes,
    MismatchedSave,
}

impl fmt::Display for ObservationRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ReversedTimes => "Observation deadline precedes the trigger.",
            Self::MismatchedSave => "Observation monitor or trigger time differs from the save confirmation.",
        })
    }
}

impl std::error::Error for ObservationRequestError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationSelection {
    BeforeAndSeen { before: Evidence<FrameId>, seen: Evidence<FrameId> },
    Single(Evidence<FrameId>),
    Suppressed,
    Dropped(DropReason),
}
