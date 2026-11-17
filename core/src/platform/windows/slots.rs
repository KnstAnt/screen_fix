//! An integer encoding timeline tied to one admission epoch.

use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use serde::{Deserialize, Serialize};

use crate::{
    capture::frame::{FrameCoverage, FrameMetadata},
    protocol::{AdmissionEpoch, FrameId, GeometryEpoch, MonitorId, SegmentId, SessionTimeUs},
};

use super::{ProbeError, unavailable};

#[derive(Clone, Copy)]
pub(super) struct Timeline {
    origin: SessionTimeUs,
    fps: u32,
    first: u64,
}

impl Timeline {
    pub(super) fn new(origin: SessionTimeUs, fps: u32) -> Result<Self, ProbeError> {
        if fps == 0 || fps > 1_000_000 {
            return Err(unavailable("Invalid video slot frame rate"));
        }
        Ok(Self { origin, fps, first: 0 })
    }

    pub(super) fn with_first(mut self, first: u64) -> Self {
        self.first = first;
        self
    }

    pub(super) fn slot(self, index: u64) -> Result<Slot, ProbeError> {
        let next = index.checked_add(1).ok_or_else(range)?;
        Ok(Slot {
            index,
            start: self.boundary(index)?,
            end: self.boundary(next)?,
        })
    }

    /// Number of slots partially or fully elapsed when admission closes.
    /// A cutoff exactly at a boundary excludes the next slot.
    pub(super) fn closed_prefix(self, cutoff: SessionTimeUs) -> Result<u64, ProbeError> {
        let elapsed = i128::from(cutoff.0) - i128::from(self.origin.0);
        if elapsed <= 0 {
            return Ok(self.first);
        }
        let remaining = (elapsed * i128::from(self.fps) + 999_999).div_euclid(1_000_000);
        let mut candidate = self.first.checked_add(u64::try_from(remaining).map_err(|_| range())?).ok_or_else(range)?;
        if candidate > self.first && self.boundary(candidate - 1)? >= cutoff {
            candidate -= 1;
        }
        Ok(candidate)
    }

    pub(super) fn containing(self, time: SessionTimeUs) -> Result<u64, ProbeError> {
        let end = time.0.max(self.origin.0).checked_add(1).ok_or_else(range)?;
        self.closed_prefix(SessionTimeUs(end))?.checked_sub(1).ok_or_else(range)
    }

    fn boundary(self, index: u64) -> Result<SessionTimeUs, ProbeError> {
        let offset = i128::from(index.checked_sub(self.first).ok_or_else(range)?) * 1_000_000 / i128::from(self.fps);
        let time = i64::try_from(i128::from(self.origin.0) + offset).map_err(|_| range())?;
        Ok(SessionTimeUs(time))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Slot {
    pub index: u64,
    pub start: SessionTimeUs,
    pub end: SessionTimeUs,
}

impl Slot {
    pub(super) fn pts(self) -> Result<i64, ProbeError> {
        self.start.0.checked_mul(10).ok_or_else(range)
    }

    pub(super) fn duration(self, cutoff: Option<SessionTimeUs>) -> Result<i64, ProbeError> {
        let end = cutoff.map_or(self.end, |cutoff| self.end.min(cutoff));
        end.0.checked_sub(self.start.0)
            .and_then(|duration| duration.checked_mul(10))
            .filter(|&duration| duration > 0)
            .ok_or_else(range)
    }
}

// One segment plus the permitted rotation delay and a bounded margin.
const EVIDENCE_SLOTS: usize = 2048;
const CAPTURE_PRESSURE: u64 = 1;
const QUEUE_PRESSURE: u64 = 2;
const CHANGED_LATE: u64 = 4;

pub(super) struct Evidence(Box<[AtomicU64]>);

impl Evidence {
    pub(super) fn new() -> Self {
        Self((0..EVIDENCE_SLOTS).map(|_| AtomicU64::new(0)).collect())
    }

    pub(super) fn capture_pressure(&self, slot: u64) {
        self.mark(slot, CAPTURE_PRESSURE);
    }

    pub(super) fn queue_pressure(&self, slot: u64) {
        self.mark(slot, QUEUE_PRESSURE);
    }

    pub(super) fn changed_late(&self, slot: u64) {
        self.mark(slot, CHANGED_LATE);
    }

    pub(super) fn pressure(&self, slot: u64) -> Option<Missing> {
        let tags = self.tags(slot);
        if tags & QUEUE_PRESSURE != 0 {
            Some(Missing::QueuePressure)
        } else if tags & CAPTURE_PRESSURE != 0 {
            Some(Missing::ConversionPressure)
        } else {
            None
        }
    }

    pub(super) fn late(&self, slot: u64) -> bool {
        self.tags(slot) & CHANGED_LATE != 0
    }

    fn mark(&self, slot: u64, tag: u64) {
        let Some(sequence) = slot.checked_add(1).filter(|&slot| slot <= (u64::MAX >> 3)) else { return };
        let cell = &self.0[(slot % EVIDENCE_SLOTS as u64) as usize];
        let _ = cell.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            let previous = old >> 3;
            if previous > sequence {
                None
            } else if previous == sequence {
                Some(old | tag)
            } else {
                Some((sequence << 3) | tag)
            }
        });
    }

    fn tags(&self, slot: u64) -> u64 {
        let current = self.0[(slot % EVIDENCE_SLOTS as u64) as usize].load(Ordering::Acquire);
        if Some(current >> 3) == slot.checked_add(1) { current & 7 } else { 0 }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Source<'a> {
    pub metadata: &'a FrameMetadata,
    pub change: Option<f64>,
    pub repeated: bool,
}

pub(super) use crate::package::metadata::MissingVideoSlot as Missing;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Row {
    pub monitor: MonitorId,
    pub slot: u64,
    pub pts_us: SessionTimeUs,
    pub slot_decided_t_us: Option<SessionTimeUs>,
    pub admission_epoch: AdmissionEpoch,
    pub encoded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Missing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment: Option<SegmentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_in_segment: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pts_100ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_100ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obs_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<FrameId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_t_us: Option<SessionTimeUs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_recv_t_us: Option<SessionTimeUs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry_version: Option<GeometryEpoch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<crate::capture::frame::MonitorGeometry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late: Option<bool>,
    #[serde(skip)]
    pub(super) coverage: Option<Arc<FrameCoverage>>,
    #[serde(skip)]
    coverage_end: Option<SessionTimeUs>,
}

impl Row {
    pub(super) fn pending(
        monitor: MonitorId, epoch: AdmissionEpoch, slot: Slot,
        decided: Option<SessionTimeUs>, source: Option<Source<'_>>, changed_arrived_late: Option<bool>,
    ) -> Self {
        Self {
            monitor, slot: slot.index, pts_us: slot.start, slot_decided_t_us: decided,
            admission_epoch: epoch, encoded: false, reason: None, segment: None,
            frame_in_segment: None, pts_100ns: None, duration_100ns: None,
            obs_id: source.map(|source| source.metadata.id.sequence.0),
            frame_id: source.map(|source| source.metadata.id),
            source_t_us: source.map(|source| source.metadata.source_t_us),
            capture_recv_t_us: source.map(|source| source.metadata.recv_t_us),
            geometry_version: source.map(|source| source.metadata.geometry.id.epoch),
            geometry: source.map(|source| source.metadata.geometry),
            repeated: source.map(|source| source.repeated),
            change: source.filter(|source| !source.repeated).and_then(|source| source.change),
            late: changed_arrived_late,
            coverage: source.and_then(|source| source.metadata.coverage.clone()), coverage_end: Some(slot.end),
        }
    }

    pub(super) fn limit_coverage(&mut self, end: SessionTimeUs) { self.coverage_end = Some(end); }

    pub(super) fn revoked(&self) -> bool {
        self.coverage.as_ref().and_then(|coverage| coverage.before()).is_some_and(|before|
            self.coverage_end.is_some_and(|end| end > before) || self.source_t_us.is_some_and(|source| source >= before))
    }

    pub(super) fn miss(&mut self, reason: Missing) {
        self.reason = Some(reason);
    }

    pub(super) fn withdraw(&mut self, reason: Missing) {
        self.miss(reason);
        self.encoded = false;
        self.segment = None;
        self.frame_in_segment = None;
        self.pts_100ns = None;
        self.duration_100ns = None;
        self.obs_id = None;
        self.frame_id = None;
        self.source_t_us = None;
        self.capture_recv_t_us = None;
        self.geometry_version = None;
        self.geometry = None;
        self.repeated = None;
        self.change = None;
        self.late = None;
        self.coverage = None;
    }

    pub(super) fn verified(&mut self, segment: SegmentId, index: u64, pts: i64, duration: i64) {
        self.encoded = true;
        self.reason = None;
        self.segment = Some(segment);
        self.frame_in_segment = Some(index);
        self.pts_100ns = Some(pts);
        self.duration_100ns = Some(duration);
    }
}

fn range() -> ProbeError {
    unavailable("The video slot is outside the recording time range")
}
