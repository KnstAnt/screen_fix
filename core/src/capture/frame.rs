use std::{num::NonZeroU32, sync::{Arc, atomic::{AtomicI64, Ordering}}};

use screen_fix_protocol::ipc::elevated::VisitBinding;

use crate::protocol::{
    AdmissionEpoch, ClockOrigin, FrameId, GeometryId, MonitorId, PhysicalRect, QpcTick, SegmentId,
    SessionTimeUs, UncertaintyUs,
};

use super::DropReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MonitorGeometry {
    pub id: GeometryId,
    pub bounds: PhysicalRect,
    pub dpi_x: NonZeroU32,
    pub dpi_y: NonZeroU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureColor {
    Sdr,
    HdrToneMappedToSdr,
    #[serde(rename = "system_bgra8")]
    HdrSystemBgra8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameExtent {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl FrameExtent {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Some(Self { width: NonZeroU32::new(width)?, height: NonZeroU32::new(height)? })
    }
}

/// Shared retirement bound for unpublished media from one exact visit.
#[derive(Debug)]
pub(crate) struct FrameCoverage {
    pub visit: VisitBinding,
    clock: ClockOrigin,
    before: AtomicI64,
}

impl FrameCoverage {
    pub fn new(visit: VisitBinding, clock: ClockOrigin) -> Self {
        Self { visit, clock, before: AtomicI64::new(i64::MAX) }
    }

    pub fn restrict(&self, before: QpcTick) {
        let before = self.clock.time(before).map_or(i64::MIN, |time| time.0);
        self.before.fetch_min(before, Ordering::AcqRel);
    }

    pub fn before(&self) -> Option<SessionTimeUs> {
        let before = self.before.load(Ordering::Acquire);
        (before != i64::MAX).then_some(SessionTimeUs(before))
    }
}

/// Metadata for one actual capture. The native image lease belongs to its platform owner.
#[derive(Debug, Clone)]
pub struct FrameMetadata {
    pub id: FrameId,
    pub geometry: MonitorGeometry,
    pub admission: AdmissionEpoch,
    pub approval_generation: u64,
    pub(crate) coverage: Option<Arc<FrameCoverage>>,
    pub source_t_us: SessionTimeUs,
    pub recv_t_us: SessionTimeUs,
    pub source_uncertainty_us: Option<UncertaintyUs>,
    pub color: CaptureColor,
    pub extent: FrameExtent,
}

impl FrameMetadata {
    pub(crate) fn elevated_visit(&self) -> Option<VisitBinding> {
        self.coverage.as_ref().map(|coverage| coverage.visit)
    }

    pub(crate) fn covered(&self) -> bool {
        self.coverage.as_ref().and_then(|coverage| coverage.before()).is_none_or(|before| self.source_t_us < before)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentPts100ns(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSlot {
    pub monitor: MonitorId,
    pub segment: SegmentId,
    pub slot_t_us: SessionTimeUs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotDisposition {
    New { frame: FrameId, pts: SegmentPts100ns },
    Repeated { frame: FrameId, pts: SegmentPts100ns },
    Missing,
    Skipped(DropReason),
}
