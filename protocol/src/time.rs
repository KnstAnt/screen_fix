use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QpcTick(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QpcFrequency(pub NonZeroU64);

/// Integer microseconds from the one session origin. Pre-origin samples can be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionTimeUs(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UncertaintyUs(pub u64);

/// The wrapping millisecond counter carried by a Windows message or hook record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageTickMs(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoarseTime {
    pub tick_ms: MessageTickMs,
    pub uncertainty_us: UncertaintyUs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordTime {
    pub recv_t_us: SessionTimeUs,
    pub source_t_us: Option<SessionTimeUs>,
    pub source_uncertainty_us: Option<UncertaintyUs>,
    pub coarse: Option<CoarseTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockOrigin {
    pub origin_qpc: QpcTick,
    pub qpc_frequency: QpcFrequency,
}

/// A real UTC sample in Windows FILETIME units (100 ns since 1601-01-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtcAnchor {
    pub qpc: QpcTick,
    pub utc_filetime_100ns: u64,
    pub qpc_bracket_ticks: u64,
}

/// The GetTickCount64 value read inside a QPC bracket. Coarse Windows message
/// ticks are evidence for lateness and boundary checks, never provider time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTickAnchor {
    pub qpc: QpcTick,
    pub tick64_ms: u64,
    pub qpc_bracket_ticks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockRangeError {
    ReversedSample,
    OutOfRange,
}

impl std::fmt::Display for ClockRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReversedSample => "The QPC sample moved backward.",
            Self::OutOfRange => "The timestamp exceeds the supported time range.",
        })
    }
}

impl std::error::Error for ClockRangeError {}

impl QpcTick {
    /// Midpoint and width of a bounded native sample, without adding two i64s.
    pub fn bracket(self, after: Self) -> Result<(Self, u64), ClockRangeError> {
        let delta = i128::from(after.0) - i128::from(self.0);
        let width = u64::try_from(delta).map_err(|_| ClockRangeError::ReversedSample)?;
        let midpoint = i128::from(self.0) + delta / 2;
        Ok((Self(checked_i64(midpoint)?), width))
    }
}

impl ClockOrigin {
    pub fn time(self, qpc: QpcTick) -> Result<SessionTimeUs, ClockRangeError> {
        self.scale_delta(self.origin_qpc, qpc, 1_000_000).map(SessionTimeUs)
    }

    /// Compatibility precision for the existing video muxer and export reader.
    pub fn offset_100ns(self, qpc: QpcTick) -> Result<i64, ClockRangeError> {
        self.delta_100ns(self.origin_qpc, qpc)
    }

    /// A duration/segment offset on the same QPC clock.
    pub fn delta_100ns(self, from: QpcTick, to: QpcTick) -> Result<i64, ClockRangeError> {
        self.scale_delta(from, to, 10_000_000)
    }

    /// WGC SystemRelativeTime is a QPC-based absolute count in 100 ns units.
    pub fn wgc_offset_100ns(self, system_relative_100ns: i64) -> Result<i64, ClockRangeError> {
        checked_i64(self.wgc_offset(system_relative_100ns))
    }

    pub fn wgc_time(self, system_relative_100ns: i64) -> Result<SessionTimeUs, ClockRangeError> {
        checked_i64(self.wgc_offset(system_relative_100ns) / 10).map(SessionTimeUs)
    }

    fn wgc_offset(self, system_relative_100ns: i64) -> i128 {
        let origin = i128::from(self.origin_qpc.0) * 10_000_000
            / i128::from(self.qpc_frequency.0.get());
        i128::from(system_relative_100ns) - origin
    }

    fn scale_delta(self, from: QpcTick, to: QpcTick, units_per_second: i128) -> Result<i64, ClockRangeError> {
        let delta = i128::from(to.0) - i128::from(from.0);
        checked_i64(delta * units_per_second / i128::from(self.qpc_frequency.0.get()))
    }
}

impl UtcAnchor {
    /// Interpolates between adjacent anchors in one continuous interval.
    /// The caller selects the interval so wall-clock estimates never cross
    /// a recorded sleep or other discontinuity.
    pub fn interpolate_filetime(
        self,
        next: Self,
        at: QpcTick,
    ) -> Result<u64, ClockRangeError> {
        if next.qpc <= self.qpc {
            return Err(ClockRangeError::ReversedSample);
        }
        if at < self.qpc || at > next.qpc {
            return Err(ClockRangeError::OutOfRange);
        }
        let width = i128::from(next.qpc.0) - i128::from(self.qpc.0);
        let offset = i128::from(at.0) - i128::from(self.qpc.0);
        let start = i128::from(self.utc_filetime_100ns);
        let delta = i128::from(next.utc_filetime_100ns) - start;
        let scaled = delta.checked_mul(offset).ok_or(ClockRangeError::OutOfRange)?;
        u64::try_from(start + scaled / width).map_err(|_| ClockRangeError::OutOfRange)
    }
}

impl CoarseTime {
    pub const fn windows_message(tick_ms: MessageTickMs) -> Self {
        Self { tick_ms, uncertainty_us: UncertaintyUs(16_000) }
    }
}

impl MessageTickAnchor {
    /// Nearest wrap around a recent sampled tick. Exactly half a wrap is ambiguous.
    pub fn coarse_estimate(
        self,
        clock: ClockOrigin,
        tick: MessageTickMs,
    ) -> Result<SessionTimeUs, ClockRangeError> {
        let delta = tick.0.wrapping_sub(self.tick64_ms as u32) as i32;
        if delta == i32::MIN {
            return Err(ClockRangeError::OutOfRange);
        }
        let anchor = clock.time(self.qpc)?;
        checked_i64(i128::from(anchor.0) + i128::from(delta) * 1_000).map(SessionTimeUs)
    }
}

fn checked_i64(value: i128) -> Result<i64, ClockRangeError> {
    i64::try_from(value).map_err(|_| ClockRangeError::OutOfRange)
}
