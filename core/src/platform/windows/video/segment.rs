//! One monitor's open MP4 segment, and the bookkeeping of a finalized part
//! until its verification result.

use std::{sync::Arc, time::Duration};

use windows::Win32::Media::MediaFoundation::*;

use crate::{
    capture::frame::FrameCoverage,
    package::metadata::{Geometry, SegmentEncoding, SegmentSummary},
    protocol::{ClockOrigin, MonitorId, SegmentId, SessionTimeUs},
    platform::windows::{ProbeError, clock, controller::{AwakeMark, WorkerSession}, mp4, slots, stage, unavailable},
};

use super::{RecordedSlot, VERIFICATION_ALLOWANCE_US};

/// A segment handed to the verifier. Its rows stay staged until the verdict.
pub(super) struct Verifying {
    pub(super) summary: SegmentSummary,
    pub(super) index: Vec<slots::Row>,
    pub(super) coverage: Option<Arc<FrameCoverage>>,
    claim: mp4::Claim,
    /// The QPC at which the part was closed, with the suspend sequence read after it.
    pub(super) handoff: AwakeMark,
    /// The verification allowance left at that QPC, sleep excluded.
    pub(super) allowance: Duration,
}

impl Verifying {
    /// Overdue: its rows are resolved and a late verdict is discarded.
    pub(super) fn abandoned(&self) -> bool {
        self.claim.is_abandoned()
    }

    /// Abandon the part and take its rows, unless already abandoned or its
    /// publication began (its verdict then follows).
    pub(super) fn abandon(&mut self) -> Option<Vec<slots::Row>> {
        self.claim.try_abandon().then(|| std::mem::take(&mut self.index))
    }
}

pub(super) enum Closed {
    /// No part remains: an empty or revoked segment with its resolved rows.
    Resolved(Vec<slots::Row>),
    /// A finalized part for the verifier, and a final row withdrawn now.
    Part { pending: Verifying, part: mp4::SegmentToVerify, suppressed: Option<slots::Row> },
}

pub(super) struct Segment {
    pub(super) id: SegmentId,
    pub(super) start: SessionTimeUs,
    offset: i64,
    encoding: SegmentEncoding,
    mux: mp4::Recording,
    index: Vec<slots::Row>,
    pending: Option<(RecordedSlot, IMFSample, bool)>,
    coverage: Option<Arc<FrameCoverage>>,
}

impl Segment {
    /// A segment whose media starts at `start`, PTS `offset` in 100 ns.
    pub(super) fn open(
        id: SegmentId, start: SessionTimeUs, offset: i64, encoding: SegmentEncoding, mux: mp4::Recording,
        coverage: Option<Arc<FrameCoverage>>,
    ) -> Self {
        Self { id, start, offset, encoding, mux, index: Vec::new(), pending: None, coverage }
    }

    /// Drop the segment and remove its `.part`.
    pub(super) fn discard(self) -> Result<(), ProbeError> {
        self.mux.discard()
    }

    pub(super) fn push(&mut self, slot: RecordedSlot, sample: &IMFSample, idr: bool) -> Result<(), ProbeError> {
        if self.coverage.as_ref().map(|coverage| coverage.visit) != slot.row.coverage.as_ref().map(|coverage| coverage.visit) {
            return Err(unavailable("An encoded slot changed the segment's elevated visit"));
        }
        if let Some((prior, picture, prior_idr)) = self.pending.take() {
            let duration = slot.pts.checked_sub(prior.pts)
                .filter(|&duration| duration > 0).ok_or_else(|| unavailable("The MP4 timestamps are not increasing"))?;
            self.write(prior, picture, duration, prior_idr)?;
        }
        self.pending = Some((slot, sample.clone(), idr));
        Ok(())
    }

    fn write(&mut self, mut slot: RecordedSlot, sample: IMFSample, duration: i64, idr: bool) -> Result<(), ProbeError> {
        let pts = slot.pts.checked_sub(self.offset)
            .filter(|&pts| pts >= 0).ok_or_else(|| unavailable("The segment-relative PTS is invalid"))?;
        stage(unsafe { sample.SetSampleTime(pts) }, "Set the segment-local PTS")?;
        stage(unsafe { sample.SetSampleDuration(duration) }, "Set the segment-local duration")?;
        stage(unsafe {
            sample.SetUINT64(&MFSampleExtension_DecodeTimestamp, pts as u64)
        }, "Set the segment-local DTS")?;
        stage(unsafe {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, u32::from(idr))
        }, "Set verified H.264 sample attributes")?;
        if self.index.len() as u64 >= self.encoding.capacity.segment_pictures {
            return Err(unavailable("The open MP4 segment exceeded its frame index bound"));
        }
        self.mux.write(&sample)?;
        slot.row.verified(self.id, self.index.len() as u64, pts, duration);
        self.index.push(slot.row);
        Ok(())
    }

    /// Write the held picture and finalize, release and flush the part on
    /// the video worker. Verification and publication follow on the verifier.
    pub(super) fn close(
        mut self, monitor: MonitorId, geometry: Geometry, worker: &WorkerSession, origin: ClockOrigin,
    ) -> Result<Closed, ProbeError> {
        let coverage = self.coverage.take();
        let cutoff = || coverage.as_ref().and_then(|coverage| coverage.before());
        let mut suppressed = None;
        if let Some((slot, picture, idr)) = self.pending.take() {
            if cutoff().is_some_and(|before| !within_media_prefix(&slot.row, slot.duration, before)) {
                let mut row = slot.row;
                row.withdraw(slots::Missing::ApprovalSuppressed);
                suppressed = Some(row);
            } else {
                let duration = slot.duration;
                self.write(slot, picture, duration, idr)?;
            }
        }
        if self.index.is_empty() {
            self.mux.discard()?;
            return Ok(Closed::Resolved(suppressed.into_iter().collect()));
        }
        // A muxed sample can display through a later slot. The on-disk part
        // is indivisible once its sink has accepted an unprovable sample.
        let media_before = self.index.iter().map(|row| {
            let end = row.pts_us.0.saturating_add(ceil_us(row.duration_100ns.unwrap_or(0)));
            SessionTimeUs(end.max(row.source_t_us.map_or(i64::MIN, |source| source.0.saturating_add(1))))
        }).max().expect("the segment has encoded rows");
        let start = self.start;
        let authorization = coverage.clone();
        let authorized = move |decoded_end: Option<i64>| {
            let decoded_before = decoded_end.map_or(media_before, |end| {
                SessionTimeUs(start.0.saturating_add(ceil_us(end)))
            });
            authorization.as_ref().and_then(|coverage| coverage.before())
                .is_none_or(|before| media_before.max(decoded_before) <= before)
        };
        let expected = self.index.iter().map(|row| row.pts_100ns.zip(row.duration_100ns)
            .ok_or_else(|| unavailable("The segment frame has no planned timing"))).collect::<Result<Vec<_>, _>>()?;
        let Some(part) = self.mux.close(self.id, expected, geometry.video_size, || worker.segment_deadline(), Box::new(authorized))? else {
            let before = cutoff().ok_or_else(|| unavailable("Revoked video media has no coverage cutoff"))?;
            withdraw_revoked(&mut self.index, before);
            self.index.extend(suppressed);
            return Ok(Closed::Resolved(self.index));
        };
        // The allowance runs from the end of the segment's media and includes
        // any wait for a verifier slot.
        let closed_qpc = clock::qpc()?;
        let suspends = worker.suspend_marks().sequence;
        let lag = origin.time(closed_qpc)?.0.saturating_sub(media_before.0);
        let allowance = Duration::from_micros(VERIFICATION_ALLOWANCE_US.saturating_sub(u64::try_from(lag).unwrap_or(0)));
        let pending = Verifying {
            summary: SegmentSummary {
                monitor, segment: self.id, common_start_us: self.start, frame_count: self.index.len() as u64,
                local_end_100ns: 0, published_bytes: None, geometry, encoding: Some(self.encoding),
            },
            index: self.index, coverage, claim: part.claim.clone(), handoff: AwakeMark { qpc: closed_qpc, suspends }, allowance,
        };
        Ok(Closed::Part { pending, part, suppressed })
    }
}

/// Withdraw the rows of revoked media: a row within the media prefix
/// authorized `before` the cutoff is unpublished, any later one suppressed.
pub(super) fn withdraw_revoked(rows: &mut [slots::Row], before: SessionTimeUs) {
    for row in rows {
        let reason = if within_media_prefix(row, row.duration_100ns.unwrap_or(0), before) {
            slots::Missing::SegmentUnpublished
        } else {
            slots::Missing::ApprovalSuppressed
        };
        row.withdraw(reason);
    }
}

fn within_media_prefix(row: &slots::Row, duration_100ns: i64, before: SessionTimeUs) -> bool {
    row.pts_us.0.saturating_add(ceil_us(duration_100ns)) <= before.0
        && row.source_t_us.is_some_and(|source| source < before)
}

/// 100 ns units rounded up to microseconds.
fn ceil_us(hundred_ns: i64) -> i64 {
    hundred_ns.saturating_add(9).div_euclid(10)
}
