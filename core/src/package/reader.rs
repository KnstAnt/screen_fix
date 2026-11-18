//! Streaming inspection of closed v6 evidence. Inspection never modifies an attempt.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;

use crate::protocol::{
    AdmissionEpoch, EventRef, FrameId, GeometryEpoch, GeometryId, MonitorId, SegmentId, SessionTimeUs, StreamId,
};

use super::{
    ArtifactKind, ArtifactRef,
    error::{PackageError, PackageOperation, PackageResult},
    journal::{MAX_ROW_BYTES, StreamSummary},
    manifest::{
        AttemptManifest, CaptureDisposition, HealthSummary, ImageFile, LostActivity, MANIFEST_VERSION, MAX_MANIFEST_BYTES,
        MissingVideoSlot, PackageState, SegmentSummary, SelectedSourceKind, SourceEvidence,
    },
    outcome::{GapRecord, HealthKind},
    owner::AttemptLayout,
    row::{RowCounts, RowKind},
};

/// Context is read first: its pause and discontinuity rows bound the gate 3
/// and gate 4 checks of the later journals (M1-10).
pub(super) const STREAMS: [StreamId; 4] = [StreamId::Context, StreamId::Input, StreamId::Frames, StreamId::Stills];

#[derive(Deserialize)]
struct Envelope {
    seq: u64,
    recv_t_us: SessionTimeUs,
    /// Absent only on frames rows.
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
    reason: Option<MissingVideoSlot>,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceSummary {
    pub standard_intervals: usize,
    pub enhanced_intervals: usize,
    pub standard_coverage_us: u64,
    pub enhanced_coverage_us: u64,
    pub blind_intervals: usize,
    pub uncertified_tails: usize,
    pub failure: Option<super::source::SourceFailure>,
    pub cleanup_failure: Option<super::source::SourceFailure>,
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

/// Read-only view of a v6 attempt.
pub struct PackageReader {
    layout: AttemptLayout,
    manifest: AttemptManifest,
}

impl PackageReader {
    pub fn open(root: &Path) -> PackageResult<Self> {
        let file = File::open(root.join("manifest.json"))
            .map_err(|e| PackageError::io(PackageOperation::OpenManifest, e))?;
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)
            .map_err(|e| PackageError::io(PackageOperation::ReadManifest, e))?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(PackageError::InvalidManifest);
        }
        #[derive(Deserialize)]
        struct Version { version: u16 }
        let decode_error = |error: serde_json::Error| PackageError::io(PackageOperation::ReadManifest, error.into());
        let version = serde_json::from_slice::<Version>(&bytes).map_err(decode_error)?.version;
        if version != MANIFEST_VERSION {
            return Err(PackageError::UnsupportedVersion(version));
        }
        let manifest: AttemptManifest = serde_json::from_slice(&bytes).map_err(decode_error)?;
        if manifest.selected_displays.is_empty()
            || manifest.selected_displays.len() > crate::config::MAX_SELECTED_MONITORS
            || manifest.selected_displays.iter().enumerate().any(|(index, id)| manifest.selected_displays[..index].contains(id))
            || (manifest.state == PackageState::Complete) != (manifest.capture == CaptureDisposition::Recorded)
            || (manifest.capture == CaptureDisposition::Failed) != manifest.failed_step.is_some()
        {
            return Err(PackageError::InvalidManifest);
        }
        if manifest.health.as_ref().is_some_and(|health| {
            *health != HealthSummary::new(&manifest.streams, health.activity_losses.clone(), health.omitted_health,
                health.undelivered_frames.clone())
                || (manifest.state == PackageState::Complete
                    && (!health.undelivered_frames.is_empty()
                        || health.activity_losses.iter().any(|loss| loss.activity == LostActivity::Required)))
        }) {
            return Err(PackageError::InvalidManifest);
        }
        if let Some(source) = &manifest.source_evidence {
            source.validate(manifest.clock.as_ref().and_then(|clock| clock.stop.map(|stop| stop.time)))?;
        }
        if manifest.state == PackageState::Complete && manifest.profile.is_some() {
            manifest.validate_recorded_inventory()?;
        }
        let layout = AttemptLayout::new(manifest.attempt_id.clone(), root.to_owned());
        Ok(Self { layout, manifest })
    }

    pub fn manifest(&self) -> &AttemptManifest {
        &self.manifest
    }

    pub fn artifact_for_uri(&self, uri: &str) -> Option<ArtifactRef> {
        ArtifactRef::from_relative_uri(self.layout.id().clone(), uri)
    }

    pub fn open_artifact(&self, artifact: &ArtifactRef) -> PackageResult<BufReader<File>> {
        open_artifact(artifact.kind, self.artifact_path(artifact)?)
    }

    fn artifact_path(&self, artifact: &ArtifactRef) -> PackageResult<PathBuf> {
        if artifact.attempt != *self.layout.id() {
            return Err(PackageError::WrongAttempt);
        }
        let listed = match artifact.kind {
            ArtifactKind::Journal(stream) if self.manifest.state == PackageState::Complete => {
                self.manifest.streams.contains_key(&stream)
            }
            ArtifactKind::StillImage(_) | ArtifactKind::CursorImage(_) => self.image_entry(artifact).is_some(),
            ArtifactKind::VideoSegment { monitor, segment } => self.manifest.monitor_video.iter()
                .any(|video| video.monitor == monitor && video.segments.iter().any(|entry| entry.segment == segment)),
            _ => true,
        };
        if !listed {
            return Err(PackageError::ArtifactNotInventoried(artifact.kind));
        }
        Ok(self.layout.path(artifact.kind))
    }

    /// The output inventory entry of a still or cursor image: `None` when the
    /// image is not listed, `Some(None)` when its owner reported no inventory.
    fn image_entry(&self, artifact: &ArtifactRef) -> Option<Option<&ImageFile>> {
        let files = match artifact.kind {
            ArtifactKind::StillImage(_) => &self.manifest.images,
            ArtifactKind::CursorImage(_) => &self.manifest.cursors,
            _ => return None,
        };
        let Some(files) = files else { return Some(None) };
        let uri = artifact.relative_uri();
        files.binary_search_by(|entry| entry.file.as_str().cmp(uri.as_str())).ok().map(|index| Some(&files[index]))
    }

    /// The source timeline is optional.
    pub fn source_summary(&self) -> Option<SourceSummary> {
        self.manifest.source_evidence.as_ref().map(SourceEvidence::summary)
    }

    /// Inspect listed JSONL journals and media references with one bounded line at a time.
    /// MP4 decode/PTS validation is performed by the segment owner before publication.
    /// Incomplete attempts can lack summaries for journals that failed to close.
    /// Gates 1–4 of M1-10 are checked in the same single pass over each journal;
    /// the first failing rule is returned with its gate, stream and row.
    pub fn validate_gates(&self) -> PackageResult<()> {
        let mut scan = JournalScan::new(self)?;
        for stream in STREAMS {
            let Some(summary) = self.manifest.streams.get(&stream) else {
                if scan.strict { return Err(PackageError::InvalidManifest); }
                continue;
            };
            scan.journal(stream, summary)?;
        }
        scan.finish()
    }

    /// Check one image a journal row names. A name that is not an inventoried
    /// image of the expected kind is an error of the naming row (`row_error`).
    fn validate_image(
        &self, artifact: Option<ArtifactRef>, still: bool, row_error: impl Fn() -> PackageError,
    ) -> PackageResult<[u32; 2]> {
        let artifact = artifact.ok_or_else(&row_error)?;
        if !matches!((artifact.kind, still), (ArtifactKind::StillImage(_), true) | (ArtifactKind::CursorImage(_), false)) {
            return Err(row_error());
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
        // `open_artifact` admits only inventoried images; the entry must describe the file.
        if let Some(Some(entry)) = self.image_entry(&artifact) {
            let bytes = file.get_ref().metadata()
                .map_err(|error| PackageError::io(PackageOperation::InspectArtifact, error))?.len();
            if (entry.width, entry.height, entry.bytes) != (width, height, bytes) {
                return Err(invalid());
            }
        }
        Ok([width, height])
    }
}

fn open_artifact(kind: ArtifactKind, path: PathBuf) -> PackageResult<BufReader<File>> {
    // Empty journals are legitimate. Media and the physical manifest must be nonempty.
    let meta = fs::symlink_metadata(&path).map_err(|e| PackageError::io(PackageOperation::InspectArtifact, e))?;
    if !meta.file_type().is_file() || (meta.len() == 0 && !matches!(kind, ArtifactKind::Journal(_))) {
        return Err(PackageError::InvalidArtifact(kind));
    }
    File::open(path).map(BufReader::new)
        .map_err(|e| PackageError::io(PackageOperation::OpenArtifact, e))
}

/// The frames rows seen so far of one inventoried segment.
struct SegmentProgress<'a> {
    summary: &'a SegmentSummary,
    frames: u64,
    last_pts: Option<i64>,
    /// The end of the last frame: its PTS plus its duration.
    end_100ns: i64,
}

/// The single pass over an attempt's journals (M1-10 gates 1–4).
struct JournalScan<'a> {
    reader: &'a PackageReader,
    /// A complete attempt with a profile: every inventory rule applies.
    strict: bool,
    segments: BTreeMap<(MonitorId, SegmentId), SegmentProgress<'a>>,
    /// The next slot of each monitor's frames rows.
    monitor_slots: BTreeMap<MonitorId, u64>,
    gates: Gates,
}

impl<'a> JournalScan<'a> {
    /// Every inventoried segment is a nonempty file of its listed size.
    fn new(reader: &'a PackageReader) -> PackageResult<Self> {
        let manifest = &reader.manifest;
        let mut segments = BTreeMap::new();
        for video in &manifest.monitor_video {
            for segment in &video.segments {
                let artifact = ArtifactRef {
                    attempt: manifest.attempt_id.clone(),
                    kind: ArtifactKind::VideoSegment { monitor: video.monitor, segment: segment.segment },
                };
                let file = reader.open_artifact(&artifact)?;
                let bytes = file.get_ref().metadata()
                    .map_err(|error| PackageError::io(PackageOperation::InspectArtifact, error))?.len();
                if segment.published_bytes.is_some_and(|listed| listed != bytes) {
                    return Err(PackageError::InvalidArtifact(artifact.kind));
                }
                let progress = SegmentProgress { summary: segment, frames: 0, last_pts: None, end_100ns: 0 };
                if segments.insert((video.monitor, segment.segment), progress).is_some() {
                    return Err(PackageError::InvalidManifest);
                }
            }
        }
        Ok(Self {
            reader, strict: manifest.state == PackageState::Complete && manifest.profile.is_some(),
            segments, monitor_slots: BTreeMap::new(), gates: Gates::default(),
        })
    }

    /// Check one journal row by row and reconcile it with its summary.
    fn journal(&mut self, stream: StreamId, summary: &StreamSummary) -> PackageResult<()> {
        let manifest = &self.reader.manifest;
        let mut file = self.reader.open_artifact(&ArtifactRef {
            attempt: manifest.attempt_id.clone(), kind: ArtifactKind::Journal(stream),
        })?;
        let mut count = 0_u64;
        let mut receipt = None;
        let mut gap_rows = 0_u64;
        let mut dropped = 0_u64;
        let mut counts = RowCounts::default();
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
            if envelope.seq != count.saturating_add(1) || receipt.is_some_and(|previous| envelope.recv_t_us < previous) {
                return Err(error());
            }
            let kind = envelope.kind.as_deref().map(RowKind::parse);
            if let Some(kind) = kind {
                counts.count_row(&kind.tally(&value));
            }
            match (stream, kind) {
                // The frames stream has no gap or health rows.
                (StreamId::Frames, Some(_)) if self.strict => return Err(error()),
                (_, Some(RowKind::Gap)) => {
                    let gap = GapRecord::deserialize(&value).map_err(|_| error())?;
                    if !gap.is_valid(envelope.recv_t_us) {
                        return Err(error());
                    }
                    gap_rows += 1;
                    if let Some(count) = gap.count {
                        dropped = dropped.saturating_add(count);
                    }
                    counts.count_gap(&gap);
                }
                // A per-second `input_health` row carries counts, not a health condition.
                (_, Some(kind)) if kind == RowKind::health(stream) && value.get("health").is_some() => {
                    if HealthKind::deserialize(&value["health"]).is_err() {
                        return Err(error());
                    }
                    counts.count_health();
                }
                (StreamId::Frames, None) => self.check_frame_row(&value, error)?,
                (_, None) => return Err(error()),
                (StreamId::Stills, Some(kind @ (RowKind::Still | RowKind::StillDropped))) => {
                    self.check_still_row(kind, &value, envelope.seq, error)?;
                }
                (StreamId::Context, Some(RowKind::CursorAsset)) => self.check_cursor_asset(&value, error)?,
                _ => {}
            }
            self.gates.row(stream, envelope.seq, envelope.recv_t_us, kind, &value)?;
            count = envelope.seq;
            receipt = Some(envelope.recv_t_us);
        }
        if count != summary.rows || receipt != summary.last_receipt
            || dropped != summary.dropped || (manifest.profile.is_some() && gap_rows != summary.gap_rows)
            || (manifest.health.is_some() && !counts.reconciles(summary))
        {
            return Err(PackageError::InvalidJournal { stream, row: count });
        }
        self.gates.finish(stream)
    }

    /// A frames row: a selected monitor, the next slot of that monitor and,
    /// when encoded, the next frame of its listed segment.
    fn check_frame_row(&mut self, value: &Value, error: impl Fn() -> PackageError + Copy) -> PackageResult<()> {
        let row = FrameRow::deserialize(value).map_err(|_| error())?;
        if !self.reader.manifest.selected_displays.contains(&row.monitor)
            || row.frame_id.is_some_and(|frame| frame.monitor != row.monitor)
            || row.geometry.is_some_and(|geometry| geometry.id.monitor != row.monitor
                || Some(geometry.id.epoch) != row.geometry_version)
        {
            return Err(error());
        }
        let next = self.monitor_slots.entry(row.monitor).or_default();
        if self.strict && row.slot != *next {
            return Err(error());
        }
        *next = next.checked_add(1).ok_or_else(error)?;
        if !row.encoded {
            if row.reason.is_none() || row.segment.is_some() || row.frame_in_segment.is_some()
                || row.pts_100ns.is_some() || row.duration_100ns.is_some()
            {
                return Err(error());
            }
            return Ok(());
        }
        let id = row.segment.ok_or_else(error)?;
        let progress = self.segments.get_mut(&(row.monitor, id)).ok_or_else(error)?;
        let pts = row.pts_100ns.ok_or_else(error)?;
        let duration = row.duration_100ns.filter(|value| *value > 0).ok_or_else(error)?;
        let first = progress.frames == 0;
        if row.reason.is_some() || row.frame_id.is_none() || row.frame_in_segment != Some(progress.frames)
            || (first && pts.unsigned_abs() > 10_000)
            || progress.last_pts.is_some_and(|previous| pts <= previous)
        {
            return Err(error());
        }
        if let Some(encoding) = progress.summary.encoding.as_ref()
            && (row.admission_epoch != encoding.admission_epoch
                || (first && (row.frame_id != Some(encoding.first_frame)
                    || row.geometry_version != Some(encoding.first_geometry))))
        {
            return Err(error());
        }
        progress.frames = progress.frames.checked_add(1).ok_or_else(error)?;
        progress.last_pts = Some(pts);
        progress.end_100ns = pts.checked_add(duration).ok_or_else(error)?;
        if progress.frames > progress.summary.frame_count {
            return Err(error());
        }
        Ok(())
    }

    /// A still or dropped-still row at `seq`: its event reference names an
    /// input action row (gate 2), and the images it names are inventoried.
    fn check_still_row(
        &self, kind: RowKind, value: &Value, seq: u64, error: impl Fn() -> PackageError + Copy,
    ) -> PackageResult<()> {
        let reader = self.reader;
        let manifest = &reader.manifest;
        if let Some(event) = value.get("event_ref").filter(|value| !value.is_null()) {
            let event = EventRef::deserialize(event).map_err(|_| error())?;
            let input = manifest.streams.get(&StreamId::Input);
            if event.stream != StreamId::Input || event.seq.0 == 0
                || input.is_some_and(|summary| event.seq.0 > summary.rows) || (self.strict && input.is_none())
            {
                return Err(error());
            }
            if input.is_some() && !self.gates.action(event.seq.0) {
                return Err(PackageError::GateViolation { gate: 2, stream: StreamId::Stills, row: seq,
                    reason: "the event reference names no input action row" });
            }
        }
        // Still files are relative to `raw/`, where the journal lives (D-1).
        let file = value["file"].as_str();
        let journal_image = |file: &str| ArtifactKind::from_journal_file(file)
            .map(|kind| ArtifactRef { attempt: manifest.attempt_id.clone(), kind });
        if kind == RowKind::Still {
            let geometry = GeometryId::deserialize(&value["geometry"]).map_err(|_| error())?;
            let frame = FrameId::deserialize(&value["frame"]["id"]).map_err(|_| error())?;
            if frame.monitor != geometry.monitor || !manifest.selected_displays.contains(&frame.monitor) {
                return Err(error());
            }
            let size = reader.validate_image(journal_image(file.ok_or_else(error)?), true, error)?;
            if value["width"].as_u64() != Some(u64::from(size[0]))
                || value["height"].as_u64() != Some(u64::from(size[1]))
            {
                return Err(error());
            }
        }
        // IMG-2: a seen image is named only when it differs from the row's image.
        if let Some(seen) = value.get("seen_file") {
            let seen = seen.as_str().filter(|seen| Some(*seen) != file).ok_or_else(error)?;
            reader.validate_image(journal_image(seen), true, error)?;
        }
        Ok(())
    }

    /// A cursor asset row: an available asset names inventoried cursor images
    /// of one size; an unavailable one names none.
    fn check_cursor_asset(&self, value: &Value, error: impl Fn() -> PackageError + Copy) -> PackageResult<()> {
        let reader = self.reader;
        let image = &value["image"];
        match (value["available"].as_bool(), image["png"].as_str()) {
            (Some(true), Some(png)) => {
                let size = reader.validate_image(reader.artifact_for_uri(png), false, error)?;
                if let Some(mask) = image["xor_mask"].as_str() {
                    if reader.validate_image(reader.artifact_for_uri(mask), false, error)? != size {
                        return Err(error());
                    }
                } else if !image["xor_mask"].is_null() {
                    return Err(error());
                }
            }
            (Some(false), None) if image.is_null() => {}
            _ => return Err(error()),
        }
        Ok(())
    }

    /// Every listed segment and monitor slot of a complete attempt was journaled.
    fn finish(self) -> PackageResult<()> {
        if self.strict && (self.segments.values().any(|progress| {
            progress.frames != progress.summary.frame_count || progress.end_100ns != progress.summary.local_end_100ns
        }) || self.reader.manifest.monitor_video.iter().any(|video| {
            self.monitor_slots.get(&video.monitor) != Some(&video.slots)
        })) {
            return Err(PackageError::InvalidJournal { stream: StreamId::Frames, row: 0 });
        }
        Ok(())
    }
}

/// A pause from its cutoff to its resume receipt; an unresumed pause is open
/// to the end of the attempt.
#[derive(Clone, Copy)]
struct PauseInterval {
    cutoff: SessionTimeUs,
    resume: Option<SessionTimeUs>,
}

impl PauseInterval {
    fn contains(self, at: SessionTimeUs) -> bool {
        at > self.cutoff && self.resume.is_none_or(|resume| at < resume)
    }
}

/// A gate 3 candidate failure at `row`, with the span from `from` to `to` in
/// which the missing transition could lie.
struct Finding {
    row: u64,
    from: SessionTimeUs,
    to: SessionTimeUs,
    reason: &'static str,
}

/// Evidence of gates 2–4 gathered in the single journal pass (M1-10).
#[derive(Default)]
struct Gates {
    pauses: Vec<PauseInterval>,
    /// Whether the context journal is between a pause row and its resume row.
    paused: bool,
    /// Gate 3 excluded intervals: pause, blind, gap, discontinuity and suppression.
    excluded: Vec<(SessionTimeUs, SessionTimeUs)>,
    /// Gate 3 open presses by (family, code, device), with their row and receipt.
    presses: BTreeMap<(&'static str, String, Option<u64>), (u64, SessionTimeUs)>,
    /// Gate 3 candidate failures, judged against `excluded` at the end of the input journal.
    findings: Vec<Finding>,
    /// The first pause cutoff the input journal has not passed yet.
    next_pause: usize,
    /// Gate 2: a bit per input row sequence that is an action row.
    actions: Vec<u64>,
}

impl Gates {
    fn action(&self, seq: u64) -> bool {
        usize::try_from(seq / 64).ok().and_then(|index| self.actions.get(index)).is_some_and(|bits| bits & (1 << (seq % 64)) != 0)
    }

    fn paused_at(&self, at: SessionTimeUs) -> bool {
        self.pauses.iter().any(|pause| pause.contains(at))
    }

    fn row(&mut self, stream: StreamId, seq: u64, at: SessionTimeUs, kind: Option<RowKind>, value: &Value) -> PackageResult<()> {
        let violation = |gate, reason| Err(PackageError::GateViolation { gate, stream, row: seq, reason });
        let time = |field: &str| value[field].as_i64().map(SessionTimeUs);
        match kind {
            Some(RowKind::Gap) => self.excluded.extend(time("first_recv_t_us").zip(time("last_recv_t_us"))),
            Some(RowKind::Blind) => self.excluded.extend(time("start_us").zip(time("end_us"))),
            Some(RowKind::Discontinuity | RowKind::SuppressedInterval) => {
                self.excluded.extend(time("start_t_us").zip(time("end_t_us")));
            }
            _ => {}
        }
        // Gate 4: only boundary, loss and approval-free rows inside a pause interval.
        match stream {
            StreamId::Context => match kind {
                Some(RowKind::Pause) => {
                    self.pauses.push(PauseInterval { cutoff: time("cutoff_t_us").unwrap_or(at), resume: None });
                    self.paused = true;
                }
                Some(RowKind::Resume) => {
                    if let Some(pause) = self.pauses.last_mut() { pause.resume = Some(at); }
                    self.paused = false;
                }
                Some(kind) if kind.allowed_in_pause(stream) => {}
                Some(RowKind::Foreground) if value["approved"] == false => {}
                _ if self.paused => return violation(4, "a context row other than a boundary or loss row while paused"),
                _ => {}
            },
            StreamId::Input if self.paused_at(at) && !kind.is_some_and(|kind| kind.allowed_in_pause(stream)) =>
                return violation(4, "an input row inside a pause interval"),
            StreamId::Frames if kind.is_none() && time("pts_us").is_some_and(|slot| self.paused_at(slot)) =>
                return violation(4, "a frames row for a paused slot"),
            StreamId::Stills if matches!(kind, Some(RowKind::Still | RowKind::StillDropped)) && time("trigger_t_us").is_some_and(|trigger| self.paused_at(trigger)) =>
                return violation(4, "a still triggered inside a pause interval"),
            _ => {}
        }
        if stream != StreamId::Input { return Ok(()) }
        if kind.is_some_and(RowKind::is_action) && value["synthetic"] != true {
            let index = usize::try_from(seq / 64).unwrap_or(usize::MAX);
            if self.actions.len() <= index { self.actions.resize(index.saturating_add(1), 0); }
            self.actions[index] |= 1 << (seq % 64);
        }
        self.pass_pauses(Some(at));
        let Some(kind @ (RowKind::KeyDown | RowKind::KeyUp | RowKind::PointerDown | RowKind::PointerUp)) = kind else {
            return Ok(())
        };
        let synthetic = value["synthetic"] == true;
        let key = matches!(kind, RowKind::KeyDown | RowKind::KeyUp);
        // Synthetic and ordinary rows carry the same key, button and device
        // fields; a touchpad-attributed row keeps its reporting device apart.
        let device = if value["source_device"].is_null() { &value["device"] } else { &value["source_device"] };
        let device = (device["status"] == "observed").then(|| device["detail"].as_u64()).flatten();
        let identity = if key { ("key", value["vk"].to_string(), device) } else { ("button", value["button"].to_string(), device) };
        if matches!(kind, RowKind::KeyDown | RowKind::PointerDown) {
            // An autorepeat or an unknown repeat state (I4) does not open a press.
            let opens = synthetic || value.get("repeat").is_none_or(|repeat| *repeat == false);
            if opens && let Some((row, opened)) = self.presses.insert(identity, (seq, at)) {
                self.findings.push(Finding { row, from: opened, to: at, reason: "a press has no release" });
            }
        } else if self.presses.remove(&identity).is_none() && !synthetic && value["press_unavailable"] != true {
            self.findings.push(Finding {
                row: seq, from: at, to: at, reason: "a release has no press and no press_unavailable mark",
            });
        }
        Ok(())
    }

    /// Gate 3: a press open at a pause boundary must be closed there by a
    /// synthetic release. Passes every pause cutoff before `at`, or every
    /// remaining cutoff when `at` is `None`.
    fn pass_pauses(&mut self, at: Option<SessionTimeUs>) {
        while let Some(&PauseInterval { cutoff, .. }) = self.pauses.get(self.next_pause) && at.is_none_or(|at| at > cutoff) {
            // The span ends before the cutoff: the pause itself must not excuse it.
            let before = SessionTimeUs(cutoff.0.saturating_sub(1));
            for &(row, opened) in self.presses.values().filter(|(_, opened)| *opened < cutoff) {
                self.findings.push(Finding { row, from: opened, to: before, reason: "a press stays open across a pause boundary" });
            }
            self.presses.retain(|_, (_, opened)| *opened >= cutoff);
            self.next_pause += 1;
        }
    }

    fn finish(&mut self, stream: StreamId) -> PackageResult<()> {
        match stream {
            StreamId::Context => {
                let end = SessionTimeUs(i64::MAX);
                self.excluded.extend(self.pauses.iter().map(|pause| (pause.cutoff, pause.resume.unwrap_or(end))));
            }
            StreamId::Input => {
                // A press opened before a pause cutoff that no later row passed (a Stop while paused).
                self.pass_pauses(None);
                let open = std::mem::take(&mut self.presses);
                let end = SessionTimeUs(i64::MAX);
                self.findings.extend(open.into_values().map(|(row, opened)| {
                    Finding { row, from: opened, to: end, reason: "a press has no release" }
                }));
                self.findings.sort_unstable_by_key(|finding| finding.row);
                // A finding is excused when an excluded interval overlaps the span in which its missing transition could lie.
                let excluded = |from: SessionTimeUs, to: SessionTimeUs| self.excluded.iter().any(|(start, end)| *start <= to && from <= *end);
                if let Some(finding) = self.findings.iter().find(|finding| !excluded(finding.from, finding.to)) {
                    return Err(PackageError::GateViolation { gate: 3, stream, row: finding.row, reason: finding.reason });
                }
            }
            _ => {}
        }
        Ok(())
    }
}
