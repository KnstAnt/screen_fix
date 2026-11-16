//! App-neutral save confirmation consumed by the observation owner.

use std::fmt;

use crate::{
    config::{ExecutableIdentity, ResolvedCaptureConfig},
    protocol::{AttemptId, BoundedList, MonitorId, RecordTime, RequestId, SaveId, UnavailableReason},
};

use crate::config::MAX_SELECTED_MONITORS;

/// A UI checkpoint request has no confirmed-save semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointRequest {
    pub id: RequestId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveKind {
    Checkpoint,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationSource {
    Fixture,
    ApplicationNotification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveEvidence {
    pub id: SaveId,
    pub time: RecordTime,
    pub kind: SaveKind,
    pub source: ConfirmationSource,
    pub executable: ExecutableIdentity,
    pub request: Option<RequestId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorRoutingBasis {
    ApplicationGeometry,
    ApprovedWindowGeometry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveRouting {
    Monitors {
        monitors: BoundedList<MonitorId, MAX_SELECTED_MONITORS>,
        basis: MonitorRoutingBasis,
    },
    Unavailable(UnavailableReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedSave {
    attempt: AttemptId,
    evidence: SaveEvidence,
    routing: SaveRouting,
}

impl ConfirmedSave {
    pub fn new(
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

    pub fn routing(&self) -> &SaveRouting {
        &self.routing
    }

    pub fn evidence(&self) -> &SaveEvidence {
        &self.evidence
    }

    pub fn attempt(&self) -> &AttemptId {
        &self.attempt
    }

    /// A save observation can be requested only for a routed selected monitor.
    pub fn for_monitor(&self, monitor: MonitorId) -> Option<SaveObservation> {
        let SaveRouting::Monitors { monitors, basis } = &self.routing else {
            return None;
        };
        if !monitors.as_slice().contains(&monitor) {
            return None;
        }
        Some(SaveObservation {
            attempt: self.attempt.clone(),
            save: self.evidence.id,
            kind: self.evidence.kind,
            time: self.evidence.time,
            monitor,
            basis: *basis,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveObservation {
    attempt: AttemptId,
    save: SaveId,
    kind: SaveKind,
    time: RecordTime,
    monitor: MonitorId,
    basis: MonitorRoutingBasis,
}

impl SaveObservation {
    pub fn attempt(&self) -> &AttemptId {
        &self.attempt
    }

    pub fn id(&self) -> SaveId {
        self.save
    }

    pub fn kind(&self) -> SaveKind {
        self.kind
    }

    pub fn time(&self) -> RecordTime {
        self.time
    }

    pub fn monitor(&self) -> MonitorId {
        self.monitor
    }

    pub fn routing_basis(&self) -> MonitorRoutingBasis {
        self.basis
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveSignalError {
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
pub struct SaveWindowContext {
    pub window: crate::protocol::WindowId,
    pub focus: crate::protocol::FocusGeneration,
    pub geometries: BoundedList<crate::protocol::GeometryId, MAX_SELECTED_MONITORS>,
    pub busy: crate::capture::context::BusyLevel,
}

impl SaveWindowContext {
    pub fn fallback_routing(&self) -> SaveRouting {
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
