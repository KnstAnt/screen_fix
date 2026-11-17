//! Streaming inspection of closed v5 evidence. Inspection never modifies an attempt.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, BufRead, Read},
};

use serde::Deserialize;
use serde_json::Value;

use crate::protocol::{
    AdmissionEpoch, EventRef, FrameId, GeometryEpoch, GeometryId, MonitorId, SegmentId, SessionTimeUs, StreamId,
};

use super::{
    ArtifactKind, ArtifactRef,
    journal::MAX_ROW_BYTES,
    metadata::{PackageError, PackageOperation, PackageReader, PackageResult, PackageState,
        SelectedSourceKind, SourceEvidence},
};

const STREAMS: [StreamId; 4] = [StreamId::Input, StreamId::Context, StreamId::Frames, StreamId::Stills];

#[derive(Deserialize)]
struct Envelope {
    seq: u64,
    recv_t_us: Option<SessionTimeUs>,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct FrameRow {
    monitor: MonitorId,
    slot: u64,
    encoded: bool,
    admission_epoch: AdmissionEpoch,
    segment: Option<SegmentId>,
    frame_in_segment: Option<u64>,
    pts_100ns: Option<i64>,
    duration_100ns: Option<i64>,
    frame_id: Option<FrameId>,
    geometry_version: Option<GeometryEpoch>,
    geometry: Option<crate::capture::frame::MonitorGeometry>,
    reason: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceSummary {
    pub standard_intervals: usize,
    pub enhanced_intervals: usize,
    pub standard_coverage_us: u64,
    pub enhanced_coverage_us: u64,
    pub blind_intervals: usize,
    pub uncertified_tails: usize,
    pub failure: Option<crate::session::SourceFailure>,
    pub cleanup_failure: Option<crate::session::SourceFailure>,
    pub truncated: bool,
}

impl SourceSummary {
    pub fn mode_label(&self) -> Option<&'static str> {
        match (self.standard_intervals != 0, self.enhanced_intervals != 0) {
            (true, true) => Some("standard and enhanced capture"),
            (true, false) => Some("standard capture"),
            (false, true) => Some("enhanced capture"),
            (false, false) => None,
        }
    }
}

impl SourceEvidence {
    pub fn summary(&self) -> SourceSummary {
        let mut summary = SourceSummary {
            standard_intervals: 0, enhanced_intervals: 0, standard_coverage_us: 0, enhanced_coverage_us: 0,
            blind_intervals: self.blind.len(), uncertified_tails: self.uncertified_tails.len(),
            failure: self.failure, cleanup_failure: self.cleanup_failure, truncated: self.truncated,
        };
        for interval in &self.intervals {
            let duration = u64::try_from(interval.before_t_us.0.saturating_sub(interval.start_t_us.0)).unwrap_or(0);
            match interval.source {
                SelectedSourceKind::Ordinary => {
                    summary.standard_intervals += 1;
                    summary.standard_coverage_us = summary.standard_coverage_us.saturating_add(duration);
                }
                SelectedSourceKind::Elevated => {
                    summary.enhanced_intervals += 1;
                    summary.enhanced_coverage_us = summary.enhanced_coverage_us.saturating_add(duration);
                }
            }
        }
        summary
    }

    pub(super) fn validate(&self, stop: Option<SessionTimeUs>) -> PackageResult<()> {
        let valid_bound = |start: SessionTimeUs, before: SessionTimeUs| {
            start.0 >= 0 && start < before && stop.is_none_or(|end| before <= end)
        };
        let registrations: BTreeSet<_> = self.elevated.iter()
            .flat_map(|source| source.registrations.iter().map(|entry| entry.epoch.0)).collect();
        if self.intervals.iter().any(|interval| {
            interval.epoch.0 == 0 || !valid_bound(interval.start_t_us, interval.before_t_us)
                || (interval.source == SelectedSourceKind::Elevated) != interval.source_generation.is_some()
                || (interval.source == SelectedSourceKind::Elevated && !self.truncated
                    && !registrations.contains(&interval.epoch.0))
        }) || self.intervals.windows(2).any(|pair| {
            pair[0].epoch.0 > pair[1].epoch.0 || pair[0].before_t_us > pair[1].start_t_us
        }) || self.blind.iter().any(|interval| {
            interval.epoch.0 == 0 || !valid_bound(interval.start_t_us, interval.before_t_us)
        }) || self.uncertified_tails.iter().any(|entry| {
            entry.epoch.0 == 0 || entry.tail.first_t_us.0 < 0 || entry.tail.first_t_us > entry.tail.cutoff_t_us
                || stop.is_some_and(|end| entry.tail.cutoff_t_us > end)
        }) || self.elevated.as_ref().is_some_and(|elevated| {
            (elevated.authenticated && !elevated.requested)
                || ((elevated.configured || elevated.stopped || !elevated.registrations.is_empty()) && !elevated.authenticated)
                || elevated.registrations.iter().any(|entry| entry.epoch.0 == 0)
        }) {
            return Err(PackageError::InvalidManifest);
        }
        Ok(())
    }
}

impl PackageReader {
    /// The source timeline is optional.
    pub fn source_summary(&self) -> Option<SourceSummary> {
        self.manifest().source_evidence.as_ref().map(SourceEvidence::summary)
    }

    /// Inspect listed JSONL journals and media references with one bounded line at a time.
    /// MP4 decode/PTS validation is performed by the segment owner before publication.
    /// Incomplete attempts can lack summaries for journals that failed to close.
    pub fn validate_v5_artifacts(&self) -> PackageResult<()> {
        let manifest = self.manifest();
        let strict = manifest.state == PackageState::Complete && manifest.profile.is_some();
        let mut segments = BTreeMap::new();
        let mut monitor_slots = BTreeMap::<MonitorId, u64>::new();
        for video in &manifest.monitor_video {
            for segment in &video.segments {
                let artifact = ArtifactRef {
                    attempt: manifest.attempt_id.clone(),
                    kind: ArtifactKind::VideoSegment { monitor: video.monitor, segment: segment.segment },
                };
                let file = self.open_artifact(&artifact)?;
                let bytes = file.get_ref().metadata()
                    .map_err(|error| PackageError::io(PackageOperation::InspectArtifact, error))?.len();
                if segment.published_bytes.is_some_and(|listed| listed != bytes) {
                    return Err(PackageError::InvalidArtifact(artifact.kind));
                }
                if segments.insert((video.monitor, segment.segment), (segment, 0_u64, None, 0_i64)).is_some() {
                    return Err(PackageError::InvalidManifest);
                }
            }
        }
        for stream in STREAMS {
            let Some(summary) = manifest.streams.get(&stream) else {
                if strict { return Err(PackageError::InvalidManifest); }
                continue;
            };
            let mut file = self.open_artifact(&ArtifactRef {
                attempt: manifest.attempt_id.clone(), kind: ArtifactKind::Journal(stream),
            })?;
            let mut count = 0_u64;
            let mut receipt = None;
            let mut gap_rows = 0_u64;
            let mut dropped = 0_u64;
            let mut line = Vec::new();
            loop {
                let error = || PackageError::InvalidJournal { stream, row: count.saturating_add(1) };
                line.clear();
                let bytes = (&mut file).take((MAX_ROW_BYTES + 32) as u64).read_until(b'\n', &mut line)
                    .map_err(|e| PackageError::io(PackageOperation::ReadArtifact, e))?;
                if bytes == 0 { break; }
                if line.last() != Some(&b'\n') { return Err(error()); }
                let value: Value = serde_json::from_slice(&line).map_err(|_| error())?;
                let envelope = Envelope::deserialize(&value).map_err(|_| error())?;
                if envelope.seq != count.saturating_add(1)
                    || envelope.recv_t_us.zip(receipt).is_some_and(|(next, previous)| next < previous)
                {
                    return Err(error());
                }
                match (stream, envelope.kind.as_deref()) {
                    (_, Some("gap")) => {
                        let count = value["count"].as_u64().filter(|count| *count != 0).ok_or_else(error)?;
                        gap_rows += 1;
                        dropped = dropped.saturating_add(count);
                    }
                    (StreamId::Frames, None) => {
                        let row = FrameRow::deserialize(&value).map_err(|_| error())?;
                        if !manifest.selected_displays.contains(&row.monitor)
                            || row.frame_id.is_some_and(|frame| frame.monitor != row.monitor)
                            || row.geometry.is_some_and(|geometry| geometry.id.monitor != row.monitor
                                || Some(geometry.id.epoch) != row.geometry_version)
                        {
                            return Err(error());
                        }
                        let next = monitor_slots.entry(row.monitor).or_default();
                        if strict && row.slot != *next {
                            return Err(error());
                        }
                        *next = next.checked_add(1).ok_or_else(error)?;
                        if row.encoded {
                            let id = row.segment.ok_or_else(error)?;
                            let (segment, seen, previous_pts, end) = segments.get_mut(&(row.monitor, id)).ok_or_else(error)?;
                            let pts = row.pts_100ns.ok_or_else(error)?;
                            let duration = row.duration_100ns.filter(|value| *value > 0).ok_or_else(error)?;
                            if row.reason.is_some() || row.frame_id.is_none() || row.frame_in_segment != Some(*seen)
                                || (*seen == 0 && pts.unsigned_abs() > 10_000)
                                || previous_pts.is_some_and(|previous| pts <= previous)
                            {
                                return Err(error());
                            }
                            if let Some(encoding) = segment.encoding.as_ref()
                                && (row.admission_epoch != encoding.admission_epoch
                                    || (*seen == 0 && (row.frame_id != Some(encoding.first_frame)
                                        || row.geometry_version != Some(encoding.first_geometry))))
                            {
                                return Err(error());
                            }
                            *seen = seen.checked_add(1).ok_or_else(error)?;
                            *previous_pts = Some(pts);
                            *end = pts.checked_add(duration).ok_or_else(error)?;
                            if *seen > segment.frame_count {
                                return Err(error());
                            }
                        } else if row.reason.is_none() || row.segment.is_some() || row.frame_in_segment.is_some()
                            || row.pts_100ns.is_some() || row.duration_100ns.is_some()
                        {
                            return Err(error());
                        }
                    }
                    (StreamId::Stills, Some("still" | "still_dropped")) => {
                        if let Some(event) = value.get("event_ref").filter(|value| !value.is_null()) {
                            let event = EventRef::deserialize(event).map_err(|_| error())?;
                            let input = manifest.streams.get(&StreamId::Input);
                            if event.stream != StreamId::Input || event.seq.0 == 0
                                || input.is_some_and(|summary| event.seq.0 > summary.rows) || (strict && input.is_none())
                            {
                                return Err(error());
                            }
                        }
                        if envelope.kind.as_deref() == Some("still") {
                            let geometry = GeometryId::deserialize(&value["geometry"]).map_err(|_| error())?;
                            let frame = FrameId::deserialize(&value["frame"]["id"]).map_err(|_| error())?;
                            if frame.monitor != geometry.monitor || !manifest.selected_displays.contains(&frame.monitor) {
                                return Err(error());
                            }
                            let file = value["file"].as_str().ok_or_else(error)?;
                            if value["seen_file"].as_str().is_some_and(|seen| seen != file) {
                                return Err(error());
                            }
                            let size = self.validate_image(file, true)?;
                            if value["width"].as_u64() != Some(u64::from(size[0]))
                                || value["height"].as_u64() != Some(u64::from(size[1]))
                            {
                                return Err(error());
                            }
                        }
                    }
                    (StreamId::Context, Some("cursor_asset")) => {
                        let image = &value["image"];
                        match (value["available"].as_bool(), image["png"].as_str()) {
                            (Some(true), Some(png)) => {
                                let size = self.validate_image(png, false)?;
                                if let Some(mask) = image["xor_mask"].as_str() {
                                    if self.validate_image(mask, false)? != size {
                                        return Err(error());
                                    }
                                } else if !image["xor_mask"].is_null() {
                                    return Err(error());
                                }
                            }
                            (Some(false), None) if image.is_null() => {}
                            _ => return Err(error()),
                        }
                    }
                    (StreamId::Frames, Some(_)) if strict => return Err(error()),
                    _ => {}
                }
                count = envelope.seq;
                receipt = envelope.recv_t_us.or(receipt);
            }
            if count != summary.rows || receipt != summary.last_receipt
                || dropped != summary.dropped || (manifest.profile.is_some() && gap_rows != summary.gap_rows)
            {
                return Err(PackageError::InvalidJournal { stream, row: count });
            }
        }
        if strict && (segments.values().any(|(segment, frames, _, end)| {
            *frames != segment.frame_count || *end != segment.local_end_100ns
        }) || manifest.monitor_video.iter().any(|video| monitor_slots.get(&video.monitor) != Some(&video.slots))) {
            return Err(PackageError::InvalidJournal { stream: StreamId::Frames, row: 0 });
        }
        Ok(())
    }

    fn validate_image(&self, uri: &str, still: bool) -> PackageResult<[u32; 2]> {
        let artifact = self.artifact_for_uri(uri).ok_or(PackageError::InvalidManifest)?;
        if !matches!((artifact.kind, still), (ArtifactKind::StillImage(_), true) | (ArtifactKind::CursorImage(_), false)) {
            return Err(PackageError::InvalidArtifact(artifact.kind));
        }
        let invalid = || PackageError::InvalidArtifact(artifact.kind);
        let mut file = self.open_artifact(&artifact)?;
        let mut header = [0; 24];
        file.read_exact(&mut header).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof { invalid() }
            else { PackageError::io(PackageOperation::ReadArtifact, error) }
        })?;
        if &header[..16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR" {
            return Err(invalid());
        }
        let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
        let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
        if width == 0 || height == 0 { return Err(invalid()); }
        Ok([width, height])
    }
}
