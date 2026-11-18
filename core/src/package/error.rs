//! The error type of the package publisher and reader.

use std::io;

use serde::{Deserialize, Serialize};

use crate::protocol::StreamId;

use super::ArtifactKind;

pub type PackageResult<T> = Result<T, PackageError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageOperation {
    CreateAttempt,
    CreateLayout,
    CreateManifest,
    EncodeManifest,
    FlushManifest,
    PublishManifest,
    MarkLateManifestIncomplete,
    OpenManifest,
    ReadManifest,
    OpenArtifact,
    InspectArtifact,
    ReadArtifact,
}

impl PackageOperation {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::CreateAttempt => "Create the recording attempt directory",
            Self::CreateLayout => "Create the recording package layout",
            Self::CreateManifest => "Create a temporary manifest",
            Self::EncodeManifest => "Write the recording manifest",
            Self::FlushManifest => "Flush the recording manifest",
            Self::PublishManifest => "Publish the recording manifest",
            Self::MarkLateManifestIncomplete => "Mark a late recording manifest incomplete",
            Self::OpenManifest => "Open the recording manifest",
            Self::ReadManifest => "Read the recording manifest",
            Self::OpenArtifact => "Open a recording artifact",
            Self::InspectArtifact => "Inspect a recording artifact",
            Self::ReadArtifact => "Read a recording artifact",
        }
    }
}

#[derive(Debug)]
pub enum PackageError {
    Io { operation: PackageOperation, source: io::Error },
    InvalidManifest,
    InconsistentInventory,
    WrongAttempt,
    UnsupportedVersion(u16),
    InvalidArtifact(ArtifactKind),
    ArtifactNotInventoried(ArtifactKind),
    InvalidJournal { stream: StreamId, row: u64 },
    /// An M1-10 acceptance gate rule failed at a journal row (row 0: at the end).
    GateViolation { gate: u8, stream: StreamId, row: u64, reason: &'static str },
}

impl PackageError {
    pub(super) fn io(operation: PackageOperation, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "{}: {source}.", operation.message()),
            Self::InvalidManifest => f.write_str("The recording manifest is invalid."),
            Self::InconsistentInventory => f.write_str("The closed producers do not reconcile with the recording inventory."),
            Self::WrongAttempt => f.write_str("The artifact belongs to a different recording attempt."),
            Self::UnsupportedVersion(version) => write!(f, "Manifest version {version} is unsupported."),
            Self::InvalidArtifact(kind) => write!(f, "The recording artifact {kind:?} has an invalid file type or size."),
            Self::ArtifactNotInventoried(kind) => write!(f, "The recording manifest does not list {kind:?}."),
            Self::InvalidJournal { stream, row: 0 } => write!(f, "The {stream:?} journal does not reconcile with its inventory."),
            Self::InvalidJournal { stream, row } => write!(f, "The {stream:?} journal is invalid at row {row}."),
            Self::GateViolation { gate, stream, row, reason } =>
                write!(f, "Gate {gate} failed in the {stream:?} journal at row {row}: {reason}."),
        }
    }
}

impl std::error::Error for PackageError {}
