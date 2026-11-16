use std::num::NonZeroU32;

use crate::protocol::{
    AdmissionEpoch, FrameId, GeometryId, MonitorId, PhysicalRect, SegmentId,
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

/// Metadata for one actual capture. The native image lease belongs to its platform owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMetadata {
    pub id: FrameId,
    pub geometry: MonitorGeometry,
    pub admission: AdmissionEpoch,
    pub approval_generation: u64,
    pub source_t_us: SessionTimeUs,
    pub recv_t_us: SessionTimeUs,
    pub source_uncertainty_us: Option<UncertaintyUs>,
    pub color: CaptureColor,
    pub extent: FrameExtent,
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
