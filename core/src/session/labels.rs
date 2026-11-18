//! Platform-neutral label rules that the controller and the input owner
//! apply to the evidence of one attempt.

use std::collections::BTreeSet;

use crate::{
    package::metadata::{InputTailReason, PenCapture, PenPressure, SourceBlindReason, TabletDriverMode, TabletInputPath},
    protocol::{Evidence, SourceFailureReason, UnavailableReason, ipc::{DeviceDescriptor, InputRegistration}},
};

use super::{PauseReason, SourceFailure};

/// The label of a source-transition pause: its actual pause reason, and
/// `elevation_transition` only when the transition latch alone paused (S2).
/// The input blind rows of the pause follow the same rule.
pub(crate) fn pause_blind_reason(reasons: &BTreeSet<PauseReason>) -> SourceBlindReason {
    reasons.iter().copied().find(|reason| *reason != PauseReason::ElevationTransition)
        .map_or(SourceBlindReason::ElevationTransition, SourceBlindReason::Pause)
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PauseBlindLabel {
    SecureDesktop,
    ElevationTransition,
    User,
    Sleep,
    LowDisk,
}

/// The labels of a pause interval's input blind rows, in row order: a lock
/// or secure desktop pause is `secure_desktop`. A source-transition pause is
/// also labelled with its other actual reasons, or `elevation_transition`
/// when the transition latch alone paused: the rule of the source blind
/// interval ([`pause_blind_reason`], S2).
pub(crate) fn pause_blind_labels(reasons: &[PauseReason]) -> Vec<PauseBlindLabel> {
    let set: BTreeSet<_> = reasons.iter().copied().collect();
    let mut labels = Vec::new();
    if set.contains(&PauseReason::LockScreen) || set.contains(&PauseReason::SecureDesktop) {
        labels.push(PauseBlindLabel::SecureDesktop);
    }
    if set.contains(&PauseReason::ElevationTransition) {
        if pause_blind_reason(&set) == SourceBlindReason::ElevationTransition {
            labels.push(PauseBlindLabel::ElevationTransition);
        } else {
            labels.extend(reasons.iter().filter_map(|reason| match reason {
                PauseReason::User => Some(PauseBlindLabel::User),
                PauseReason::Sleep => Some(PauseBlindLabel::Sleep),
                PauseReason::LowDisk => Some(PauseBlindLabel::LowDisk),
                PauseReason::LockScreen | PauseReason::SecureDesktop | PauseReason::ElevationTransition => None,
            }));
        }
    }
    labels
}

/// The reason of an uncertified input tail, by priority: a lost required
/// hook, then any source failure, then a pending elevated activation, then
/// the elevated coverage, and otherwise the input producer's progress.
pub(crate) fn input_tail_reason(
    hook_lost: bool, failure: Option<SourceFailure>, certified: bool, synchronized: bool,
) -> InputTailReason {
    if hook_lost || failure.is_some_and(|failure| failure.reason == SourceFailureReason::RequiredHook) {
        InputTailReason::RequiredHook
    } else if failure.is_some() {
        InputTailReason::SourceFailure
    } else if certified && !synchronized {
        InputTailReason::ActivationPending
    } else if certified {
        InputTailReason::ElevatedCoverage
    } else {
        InputTailReason::ProducerProgress
    }
}

/// The pen capture evidence of the manifest: why pressure is unavailable
/// when no pressure was observed.
pub(crate) fn pen_capture(
    (pen_observed, pressure_observed): (bool, bool), registration: InputRegistration, remote: bool,
    devices: &[DeviceDescriptor], inventory_truncated: bool,
) -> PenCapture {
    let pressure = if pressure_observed {
        PenPressure::Captured
    } else if !registration.pen_hid {
        PenPressure::UnavailableUnknown
    } else if remote {
        PenPressure::UnavailableVmOrRemote
    } else if !inventory_truncated && !devices.iter().any(|descriptor| descriptor.kind.is_pen()) {
        PenPressure::UnavailableNoHidCollection
    } else {
        PenPressure::UnavailableUnknown
    };
    PenCapture {
        pressure,
        pen_hid_collection_enabled: registration.pen_hid,
        observed_path: pen_observed.then_some(TabletInputPath::NativeDigitizerHid),
        driver_mode: TabletDriverMode::Unknown,
        app_tablet_settings: Evidence::Unavailable(UnavailableReason::Unsupported),
    }
}
