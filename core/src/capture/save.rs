//! App-neutral save confirmation consumed by the observation owner.

use std::fmt;

use crate::{
    config::{ExecutableIdentity, ResolvedCaptureConfig},
    protocol::{AttemptId, BoundedList, MonitorId, RecordTime, RequestId, SaveId, UnavailableReason},
};

use crate::config::MAX_SELECTED_MONITORS;

/// A UI checkpoint request has no confirmed-save semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckpointRequest {
    pub(crate) id: RequestId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveKind {
    Checkpoint,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmationSource {
    Fixture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SaveEvidence {
    pub(crate) id: SaveId,
    pub(crate) time: RecordTime,
    pub(crate) kind: SaveKind,
    pub(crate) source: ConfirmationSource,
    pub(crate) executable: ExecutableIdentity,
    pub(crate) request: Option<RequestId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MonitorRoutingBasis {
    ApplicationGeometry,
    ApprovedWindowGeometry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SaveRouting {
    Monitors {
        monitors: BoundedList<MonitorId, MAX_SELECTED_MONITORS>,
        basis: MonitorRoutingBasis,
    },
    Unavailable(UnavailableReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmedSave {
    attempt: AttemptId,
    evidence: SaveEvidence,
    routing: SaveRouting,
}

impl ConfirmedSave {
    pub(crate) fn new(
        attempt: AttemptId,
        config: &ResolvedCaptureConfig,
        evidence: SaveEvidence,
        routing: SaveRouting,
    ) -> Result<Self, SaveSignalError> {
        if !config.profile().approved_identities().contains(&evidence.executable) {
            return Err(SaveSignalError::UnapprovedExecutable);
        }

        if let SaveRouting::Monitors { monitors, .. } = &routing {
            let monitors = monitors.as_slice();
            if monitors.is_empty() {
                return Err(SaveSignalError::EmptyRouting);
            }
            for (index, monitor) in monitors.iter().enumerate() {
                if !config.monitors().iter().any(|selected| selected.id == *monitor) {
                    return Err(SaveSignalError::UnselectedMonitor);
                }
                if monitors[..index].contains(monitor) {
                    return Err(SaveSignalError::RepeatedMonitor);
                }
            }
        }

        Ok(Self { attempt, evidence, routing })
    }

    /// A save observation can be requested only for a routed selected monitor.
    pub(crate) fn for_monitor(&self, monitor: MonitorId) -> Option<SaveObservation> {
        let SaveRouting::Monitors { monitors, basis } = &self.routing else {
            return None;
        };
        if !monitors.as_slice().contains(&monitor) {
            return None;
        }
        Some(SaveObservation {
            save: self.evidence.id,
            kind: self.evidence.kind,
            time: self.evidence.time,
            monitor,
            basis: *basis,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SaveObservation {
    save: SaveId,
    kind: SaveKind,
    time: RecordTime,
    monitor: MonitorId,
    basis: MonitorRoutingBasis,
}

impl SaveObservation {
    pub(crate) fn id(&self) -> SaveId {
        self.save
    }

    pub(crate) fn kind(&self) -> SaveKind {
        self.kind
    }

    pub(crate) fn time(&self) -> RecordTime {
        self.time
    }

    pub(crate) fn monitor(&self) -> MonitorId {
        self.monitor
    }

    pub(crate) fn routing_basis(&self) -> MonitorRoutingBasis {
        self.basis
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveSignalError {
    UnapprovedExecutable,
    EmptyRouting,
    UnselectedMonitor,
    RepeatedMonitor,
}

impl fmt::Display for SaveSignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnapprovedExecutable => "Save confirmation originated outside the approved selection.",
            Self::EmptyRouting => "Save confirmation listed no affected monitors.",
            Self::UnselectedMonitor => "Save confirmation listed an unselected monitor.",
            Self::RepeatedMonitor => "Save confirmation listed a monitor more than once.",
        })
    }
}

impl std::error::Error for SaveSignalError {}

/// A checkpoint request may carry approved native context. Application or
/// fixture proof is still required before it can become a confirmed save.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SaveWindowContext {
    pub(crate) window: crate::protocol::WindowId,
    pub(crate) focus: crate::protocol::FocusGeneration,
    pub(crate) geometries: BoundedList<crate::protocol::GeometryId, MAX_SELECTED_MONITORS>,
    pub(crate) busy: crate::capture::context::BusyLevel,
}

impl SaveWindowContext {
    pub(crate) fn fallback_routing(&self) -> SaveRouting {
        if self.geometries.as_slice().is_empty() {
            return SaveRouting::Unavailable(UnavailableReason::NotObserved);
        }
        let monitors = BoundedList::new(self.geometries.as_slice().iter().map(|geometry| geometry.monitor).collect())
            .expect("routing uses the existing selected-monitor bound");
        SaveRouting::Monitors { monitors, basis: MonitorRoutingBasis::ApprovedWindowGeometry }
    }
}

/// A trusted in-process fixture calls this input after its own save has
/// succeeded. Recorder input, including the checkpoint hotkey, cannot create it.
#[derive(Debug, Clone)]
pub struct FixtureSave {
    pub kind: SaveKind,
    pub executable: ExecutableIdentity,
    pub affected_monitors: Option<BoundedList<MonitorId, MAX_SELECTED_MONITORS>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureSaveError {
    AdmissionClosed,
    ControllerUnavailable,
    Busy,
    ClockUnavailable,
    UnapprovedContext,
    InvalidRouting,
    MonitorUnavailable,
    PartiallyRouted { id: SaveId, admitted: usize },
}

pub type FixtureSaveResult = Result<SaveId, FixtureSaveError>;
