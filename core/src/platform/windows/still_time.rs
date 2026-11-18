//! Capture-owner selectors. Every comparison uses an actual capture's source time.

use crate::{
    capture::frame::FrameMetadata,
    config::DeviceCalibration,
    protocol::{AdmissionEpoch, CaptureEpoch, GeometryId, SessionTimeUs},
};

use super::native::{NativeFrame, NativeImages};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Interval {
    pub geometry: GeometryId,
    pub capture: CaptureEpoch,
    pub admission: AdmissionEpoch,
    pub approval: u64,
    pub elevated_visit: Option<screen_fix_protocol::ipc::elevated::VisitBinding>,
}

impl Interval {
    pub fn contains(self, frame: &FrameMetadata) -> bool {
        frame.geometry.id == self.geometry && frame.id.epoch == self.capture
            && frame.admission == self.admission && frame.approval_generation == self.approval
            && frame.elevated_visit() == self.elevated_visit && frame.covered()
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Missing {
    HistoryLost,
    UnknownRefresh,
    OutsideTimeline,
    NoEligibleFrame,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(super) struct Adjustment {
    pub boundary: SessionTimeUs,
    pub measured_delay_us: Option<u64>,
    pub uncertainty: Option<crate::protocol::UnavailableReason>,
    pub calibration_revision: Option<u16>,
}

impl Adjustment {
    pub fn from_receipt(receipt: SessionTimeUs, calibration: Option<DeviceCalibration>) -> Result<Self, Missing> {
        match calibration {
            Some(calibration) => {
                let delay = calibration.receipt_delay_p999_us.0;
                let boundary = i128::from(receipt.0) - i128::from(delay);
                let boundary = i64::try_from(boundary).map_err(|_| Missing::OutsideTimeline)?;
                Ok(Self { boundary: SessionTimeUs(boundary), measured_delay_us: Some(delay), uncertainty: None,
                    calibration_revision: Some(calibration.calibration_revision.get()) })
            }
            None => Ok(Self { boundary: receipt, measured_delay_us: None, uncertainty: Some(crate::protocol::UnavailableReason::MissingCalibration), calibration_revision: None }),
        }
    }

    fn seen_boundary(self, refresh: (u32, u32)) -> Result<SessionTimeUs, Missing> {
        let (numerator, denominator) = refresh;
        if numerator == 0 || denominator == 0 {
            return Err(Missing::UnknownRefresh);
        }
        // Integer source stamps at or before the exact fractional boundary
        // are also at or before its floor.
        let period = (1_000_000 * u64::from(denominator)).div_ceil(u64::from(numerator));
        self.boundary.0.checked_sub(period as i64).map(SessionTimeUs).ok_or(Missing::OutsideTimeline)
    }
}

pub(super) struct BeforeSeen<'a> {
    pub before: Result<&'a NativeFrame, Missing>,
    pub seen: Result<&'a NativeFrame, Missing>,
}

pub(super) fn preceding(
    images: &NativeImages, interval: Interval, adjustment: Adjustment, refresh: (u32, u32),
) -> BeforeSeen<'_> {
    // History is lost only when the evicted frame would itself have been
    // eligible (ALG §8.2), as in `next`.
    let select = |eligible: &dyn Fn(SessionTimeUs) -> bool| {
        images.history().iter().rev()
            .find(|frame| interval.contains(&frame.metadata) && eligible(frame.metadata.source_t_us))
            .ok_or_else(|| {
                if images.last_evicted().is_some_and(|frame| interval.contains(&frame) && eligible(frame.source_t_us)) {
                    Missing::HistoryLost
                } else {
                    Missing::NoEligibleFrame
                }
            })
    };
    BeforeSeen {
        before: select(&|at| at < adjustment.boundary),
        seen: adjustment.seen_boundary(refresh).and_then(|boundary| select(&|at| at <= boundary)),
    }
}

pub(super) fn next(
    images: &NativeImages, interval: Interval, end: SessionTimeUs,
) -> Result<Option<&NativeFrame>, Missing> {
    if images.last_evicted().is_some_and(|frame| interval.contains(&frame) && frame.source_t_us > end) {
        return Err(Missing::HistoryLost);
    }
    Ok(images.history().iter().find(|frame| interval.contains(&frame.metadata) && frame.metadata.source_t_us > end))
}

pub(super) fn latest(images: &NativeImages, interval: Interval, cutoff: SessionTimeUs) -> Option<&NativeFrame> {
    images.history().iter().rev().find(|frame| interval.contains(&frame.metadata) && frame.metadata.source_t_us < cutoff)
}
