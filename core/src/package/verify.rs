//! The read-only acceptance check of one closed attempt (M1-10): gates 1–4
//! of `PackageReader::validate_gates`, then the decode part of gate 2 for
//! each published segment. Nothing in the attempt is modified.

use std::{collections::BTreeMap, io::BufRead, path::Path};

use serde::Deserialize;

use crate::protocol::{MonitorId, SegmentId, StreamId};

use super::{
    ArtifactKind, ArtifactRef,
    error::{PackageError, PackageOperation, PackageResult},
    reader::PackageReader,
};

pub struct Verification {
    pub reader: PackageReader,
    /// Gates 1–4 at row level: the first failing rule.
    pub gates: PackageResult<()>,
    /// The gate 2 decode comparison of each published segment. Empty unless
    /// the row-level gates pass and the frames journal is listed.
    pub segments: Vec<SegmentDecode>,
}

pub struct SegmentDecode {
    pub monitor: MonitorId,
    pub segment: SegmentId,
    /// The `encoded:true` frames rows of the segment.
    pub frames: usize,
    /// Why the decoded pictures do not match those rows.
    pub failure: Option<String>,
}

/// The fields of a frames row that place an encoded picture.
#[derive(Deserialize)]
struct EncodedRow {
    monitor: MonitorId,
    encoded: bool,
    segment: Option<SegmentId>,
    pts_100ns: Option<i64>,
    duration_100ns: Option<i64>,
}

/// Open the attempt at `root` and check it. An attempt that cannot be opened
/// is an error; a failing check is reported in the `Verification`.
pub fn verify_attempt(root: &Path) -> PackageResult<Verification> {
    let reader = PackageReader::open(root)?;
    let mut segments = Vec::new();
    let gates = reader.validate_gates().and_then(|()| {
        segments = decode_segments(&reader)?;
        Ok(())
    });
    Ok(Verification { reader, gates, segments })
}

fn decode_segments(reader: &PackageReader) -> PackageResult<Vec<SegmentDecode>> {
    let manifest = reader.manifest();
    if !manifest.streams.contains_key(&StreamId::Frames) {
        return Ok(Vec::new());
    }
    let artifact = |kind| ArtifactRef { attempt: manifest.attempt_id.clone(), kind };
    let mut timing = BTreeMap::<_, Vec<_>>::new();
    let journal = reader.open_artifact(&artifact(ArtifactKind::Journal(StreamId::Frames)))?;
    for (index, line) in journal.lines().enumerate() {
        let line = line.map_err(|error| PackageError::io(PackageOperation::ReadArtifact, error))?;
        let row: EncodedRow = serde_json::from_str(&line)
            .map_err(|_| PackageError::InvalidJournal { stream: StreamId::Frames, row: index as u64 + 1 })?;
        if let (true, Some(segment), Some(pts), Some(duration)) = (row.encoded, row.segment, row.pts_100ns, row.duration_100ns) {
            timing.entry((row.monitor, segment)).or_default().push((pts, duration));
        }
    }
    let mut decoded = Vec::new();
    for video in &manifest.monitor_video {
        for summary in &video.segments {
            let path = reader.artifact_path(&artifact(ArtifactKind::VideoSegment { monitor: video.monitor, segment: summary.segment }))?;
            let expected = timing.remove(&(video.monitor, summary.segment)).unwrap_or_default();
            let failure = crate::platform::windows::verify_segment_decoding(&path, &expected, summary.geometry.video_size)
                .err().map(|error| error.to_string());
            decoded.push(SegmentDecode { monitor: video.monitor, segment: summary.segment, frames: expected.len(), failure });
        }
    }
    Ok(decoded)
}
