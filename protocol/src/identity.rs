use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

macro_rules! numeric_id {
    ($($name:ident: $repr:ty),+ $(,)?) => {
        $(
            #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
            #[serde(transparent)]
            pub struct $name(pub $repr);
        )+
    };
}

numeric_id! {
    MonitorId: u32,
    GeometryEpoch: u32,
    CaptureEpoch: u32,
    AdmissionEpoch: u64,
    FrameSequence: u64,
    RequestId: u64,
    ObservationId: u64,
    SaveId: u64,
    SegmentId: u32,
    SourceSequence: u64,
    StreamSequence: u64,
    DeviceId: u64,
    WindowId: u64,
    FocusGeneration: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamId {
    Input,
    Context,
    Frames,
    Stills,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventRef {
    pub stream: StreamId,
    pub seq: StreamSequence,
}

/// Identifies an actual captured image, independently of encoded slots and repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameId {
    pub monitor: MonitorId,
    pub epoch: CaptureEpoch,
    pub sequence: FrameSequence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GeometryId {
    pub monitor: MonitorId,
    pub epoch: GeometryEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRecordingId;

impl fmt::Display for InvalidRecordingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Expected 1 to 64 ASCII letters, digits, underscores or hyphens.")
    }
}

impl std::error::Error for InvalidRecordingId {}

macro_rules! recording_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(Box<str>);

        impl $name {
            pub fn new(value: &str) -> Result<Self, InvalidRecordingId> {
                if value.is_empty() || value.len() > 64
                    || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return Err(InvalidRecordingId);
                }
                Ok(Self(value.into()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::new(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

recording_id!(SessionId);
recording_id!(AttemptId);
