//! References into one attempt's M1 artifact inventory.

use std::fmt;
 
pub mod metadata;
pub mod diagnostics;
pub(crate) mod journal;
mod reader;
pub use journal::StreamSummary;

use crate::protocol::{AttemptId, MonitorId, SegmentId, StreamId};

/// Digest bytes used in an immutable image filename. The image owner defines the digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageDigest(pub [u8; 32]);

impl fmt::Display for ImageDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl ImageDigest {
    fn parse(hex: &str) -> Option<Self> {
        fn digit(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            }
        }
        if hex.len() != 64 { return None; }
        let mut digest = [0; 32];
        for (out, pair) in digest.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            *out = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        Some(Self(digest))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Manifest,
    Journal(StreamId),
    VideoSegment { monitor: MonitorId, segment: SegmentId },
    StillImage(ImageDigest),
    CursorImage(ImageDigest),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactRef {
    pub attempt: AttemptId,
    pub kind: ArtifactKind,
}

impl ArtifactRef {
    /// Resolve only the canonical, attempt-relative paths emitted by the owners.
    pub fn from_relative_uri(attempt: AttemptId, uri: &str) -> Option<Self> {
        let kind = match uri {
            "manifest.json" => ArtifactKind::Manifest,
            "raw/input.jsonl" => ArtifactKind::Journal(StreamId::Input),
            "raw/context.jsonl" => ArtifactKind::Journal(StreamId::Context),
            "raw/frames.jsonl" => ArtifactKind::Journal(StreamId::Frames),
            "raw/stills.jsonl" => ArtifactKind::Journal(StreamId::Stills),
            _ if uri.starts_with("raw/images/") => {
                ArtifactKind::StillImage(ImageDigest::parse(uri.strip_prefix("raw/images/")?.strip_suffix(".png")?)?)
            }
            _ if uri.starts_with("raw/cursors/") => {
                ArtifactKind::CursorImage(ImageDigest::parse(uri.strip_prefix("raw/cursors/")?.strip_suffix(".png")?)?)
            }
            _ => {
                let (monitor, segment) = uri.strip_prefix("raw/video/m")?.split_once("/seg_")?;
                let segment = segment.strip_suffix(".mp4")?.parse().ok()?;
                ArtifactKind::VideoSegment { monitor: MonitorId(monitor.parse().ok()?), segment: SegmentId(segment) }
            }
        };
        let artifact = Self { attempt, kind };
        (artifact.relative_uri() == uri).then_some(artifact)
    }

    /// Stable attempt-relative URI. No input path is used to form the URI.
    pub fn relative_uri(&self) -> String {
        match self.kind {
            ArtifactKind::Manifest => "manifest.json".into(),
            ArtifactKind::Journal(StreamId::Input) => "raw/input.jsonl".into(),
            ArtifactKind::Journal(StreamId::Context) => "raw/context.jsonl".into(),
            ArtifactKind::Journal(StreamId::Frames) => "raw/frames.jsonl".into(),
            ArtifactKind::Journal(StreamId::Stills) => "raw/stills.jsonl".into(),
            ArtifactKind::VideoSegment { monitor, segment } => {
                format!("raw/video/m{}/seg_{:06}.mp4", monitor.0, segment.0)
            }
            ArtifactKind::StillImage(digest) => format!("raw/images/{digest}.png"),
            ArtifactKind::CursorImage(digest) => format!("raw/cursors/{digest}.png"),
        }
    }
}
