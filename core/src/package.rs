//! References into one attempt's M1 artifact inventory.

use std::fmt;

pub mod metadata;
pub mod diagnostics;
mod error;
pub(crate) mod finalize;
pub(crate) mod journal;
mod manifest;
pub(crate) mod outcome;
mod owner;
mod reader;
pub(crate) mod row;
pub(crate) mod source;
#[cfg(test)]
mod tests;
mod verify;
pub use journal::StreamSummary;
pub use reader::SourceSummary;
pub use verify::{SegmentDecode, Verification, verify_attempt};

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

/// The attempt directory of the journals. Still rows name their images
/// relative to it (D-1).
const RAW: &str = "raw";
pub(crate) const RAW_VIDEO: &str = "raw/video";
pub(crate) const RAW_IMAGES: &str = "raw/images";
pub(crate) const RAW_CURSORS: &str = "raw/cursors";
const MANIFEST: &str = "manifest.json";

/// The attempt-relative path of the journal of `stream`.
fn journal_file(stream: StreamId) -> &'static str {
    match stream {
        StreamId::Input => "raw/input.jsonl",
        StreamId::Context => "raw/context.jsonl",
        StreamId::Frames => "raw/frames.jsonl",
        StreamId::Stills => "raw/stills.jsonl",
    }
}

/// `uri` relative to `raw/`, where the journals live (D-1); `None` outside it.
pub(crate) fn raw_relative(uri: &str) -> Option<&str> {
    uri.strip_prefix(RAW)?.strip_prefix('/')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Manifest,
    Journal(StreamId),
    VideoSegment { monitor: MonitorId, segment: SegmentId },
    StillImage(ImageDigest),
    CursorImage(ImageDigest),
}

impl ArtifactKind {
    /// The attempt-relative directory of the video segments of `monitor`.
    pub(crate) fn video_dir(monitor: MonitorId) -> String {
        format!("{RAW_VIDEO}/m{}", monitor.0)
    }

    /// The artifact a journal row names by `file`, relative to `raw/` (D-1).
    pub(crate) fn from_journal_file(file: &str) -> Option<Self> {
        Self::from_relative_uri(&format!("{RAW}/{file}"))
    }

    /// Resolve only the canonical, attempt-relative paths emitted by the owners.
    fn from_relative_uri(uri: &str) -> Option<Self> {
        let image = |directory: &str| {
            ImageDigest::parse(uri.strip_prefix(directory)?.strip_prefix('/')?.strip_suffix(".png")?)
        };
        let kind = if uri == MANIFEST {
            Self::Manifest
        } else if let Some(stream) = reader::STREAMS.into_iter().find(|&stream| journal_file(stream) == uri) {
            Self::Journal(stream)
        } else if let Some(digest) = image(RAW_IMAGES) {
            Self::StillImage(digest)
        } else if let Some(digest) = image(RAW_CURSORS) {
            Self::CursorImage(digest)
        } else {
            let (monitor, segment) = uri.strip_prefix(RAW_VIDEO)?.strip_prefix("/m")?.split_once("/seg_")?;
            let segment = segment.strip_suffix(".mp4")?.parse().ok()?;
            Self::VideoSegment { monitor: MonitorId(monitor.parse().ok()?), segment: SegmentId(segment) }
        };
        (kind.relative_uri() == uri).then_some(kind)
    }

    /// Stable attempt-relative URI. No input path is used to form the URI.
    fn relative_uri(self) -> String {
        match self {
            Self::Manifest => MANIFEST.into(),
            Self::Journal(stream) => journal_file(stream).into(),
            Self::VideoSegment { monitor, segment } => {
                format!("{}/seg_{:06}.mp4", Self::video_dir(monitor), segment.0)
            }
            Self::StillImage(digest) => format!("{RAW_IMAGES}/{digest}.png"),
            Self::CursorImage(digest) => format!("{RAW_CURSORS}/{digest}.png"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactRef {
    pub attempt: AttemptId,
    pub kind: ArtifactKind,
}

impl ArtifactRef {
    /// Resolve only the canonical, attempt-relative paths emitted by the owners.
    pub fn from_relative_uri(attempt: AttemptId, uri: &str) -> Option<Self> {
        ArtifactKind::from_relative_uri(uri).map(|kind| Self { attempt, kind })
    }

    /// Stable attempt-relative URI. No input path is used to form the URI.
    pub fn relative_uri(&self) -> String {
        self.kind.relative_uri()
    }

    /// The name a journal row gives this artifact: its path relative to
    /// `raw/` (D-1). `None` for an artifact outside `raw/`.
    pub(crate) fn journal_file(&self) -> Option<String> {
        raw_relative(&self.relative_uri()).map(str::to_owned)
    }
}
