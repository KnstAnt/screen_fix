use crate::protocol::{
    AdmissionEpoch, DeviceId, Evidence, GeometryId, ObservationId, PhysicalPoint, RecordTime, SourceSequence,
    ipc::{ForegroundAtReceipt, Producer},
};

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
