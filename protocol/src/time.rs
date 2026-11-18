use std::{num::NonZeroU64, time::Duration};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QpcTick(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QpcFrequency(pub NonZeroU64);

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const MICROS_PER_SECOND: i128 = 1_000_000;

/// The receipt uncertainty of a Windows message or hook tick, whose
/// counter advances in steps of about one timer interrupt.
pub const MESSAGE_TICK_UNCERTAINTY_MS: u64 = 16;

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

impl QpcFrequency {
    /// The whole ticks in `duration`, rounded down; `None` beyond `i64`.
    pub fn ticks(self, duration: Duration) -> Option<i64> {
        let ticks = duration.as_nanos().checked_mul(u128::from(self.0.get()))? / NANOS_PER_SECOND;
        i64::try_from(ticks).ok()
    }

    /// The ticks covering `duration`, rounded up; `None` beyond `i64`.
    pub fn ticks_ceil(self, duration: Duration) -> Option<i64> {
        let ticks = duration.as_nanos().checked_mul(u128::from(self.0.get()))?.div_ceil(NANOS_PER_SECOND);
        i64::try_from(ticks).ok()
    }

    /// The time `ticks` span, rounded down to a nanosecond.
    pub fn duration(self, ticks: u64) -> Duration {
        let frequency = self.0.get();
        let nanos = u128::from(ticks % frequency) * NANOS_PER_SECOND / u128::from(frequency);
        // The remainder is below one second, so `nanos` is below 10^9.
        Duration::new(ticks / frequency, u32::try_from(nanos).unwrap_or(u32::MAX))
    }

    /// The whole microseconds covering `ticks`, rounded up.
    pub fn ticks_to_us_ceil(self, ticks: u64) -> i128 {
        let frequency = i128::from(self.0.get());
        (i128::from(ticks) * MICROS_PER_SECOND + frequency - 1) / frequency
    }
}

impl QpcTick {
    /// `self` plus the whole ticks in `duration`, rounded down; `None` on overflow.
    pub fn checked_after(self, frequency: QpcFrequency, duration: Duration) -> Option<Self> {
        self.0.checked_add(frequency.ticks(duration)?).map(Self)
    }

    /// The signed ticks from `earlier` to `self`, without overflow.
    pub fn ticks_since(self, earlier: Self) -> i128 {
        i128::from(self.0) - i128::from(earlier.0)
    }

    /// Midpoint and width of a bounded native sample, without adding two i64s.
    pub fn bracket(self, after: Self) -> Result<(Self, u64), ClockRangeError> {
        let delta = after.ticks_since(self);
        let width = u64::try_from(delta).map_err(|_| ClockRangeError::ReversedSample)?;
        let midpoint = i128::from(self.0) + delta / 2;
        Ok((Self(checked_i64(midpoint)?), width))
    }
}

impl ClockOrigin {
    pub fn time(self, qpc: QpcTick) -> Result<SessionTimeUs, ClockRangeError> {
        self.scale_delta(self.origin_qpc, qpc, 1_000_000).map(SessionTimeUs)
    }

    /// An offset from session origin in 100 ns units.
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
        checked_i64(self.wgc_offset(system_relative_100ns).div_euclid(10)).map(SessionTimeUs)
    }

    /// Rounded once from the exact QPC-scale difference, like `time`, so a
    /// source stamped at or before a receipt's QPC never maps after it.
    fn wgc_offset(self, system_relative_100ns: i64) -> i128 {
        let frequency = i128::from(self.qpc_frequency.0.get());
        (i128::from(system_relative_100ns) * frequency - i128::from(self.origin_qpc.0) * 10_000_000)
            .div_euclid(frequency)
    }

    /// Floors toward negative infinity, like `wgc_offset`, so pre-origin
    /// samples round the same way on both paths.
    fn scale_delta(self, from: QpcTick, to: QpcTick, units_per_second: i128) -> Result<i64, ClockRangeError> {
        let delta = to.ticks_since(from);
        checked_i64((delta * units_per_second).div_euclid(i128::from(self.qpc_frequency.0.get())))
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
        Self { tick_ms, uncertainty_us: UncertaintyUs(MESSAGE_TICK_UNCERTAINTY_MS * 1_000) }
    }
}

impl MessageTickMs {
    /// The milliseconds from the `GetTickCount64` value `anchor_ms` to this
    /// tick at the nearest wrap of the 32-bit message tick. Exactly half a
    /// wrap is ambiguous: `None`.
    pub fn delta_from(self, anchor_ms: u64) -> Option<i32> {
        // The message tick is the low 32 bits of the 64-bit count, and the
        // wrapped difference is read as signed: both casts are intended.
        let delta = self.0.wrapping_sub(anchor_ms as u32) as i32;
        (delta != i32::MIN).then_some(delta)
    }
}

impl MessageTickAnchor {
    /// The milliseconds from the anchor to `tick` (`MessageTickMs::delta_from`).
    pub fn tick_delta_ms(self, tick: MessageTickMs) -> Option<i32> {
        tick.delta_from(self.tick64_ms)
    }

    /// Nearest wrap around a recent sampled tick (`tick_delta_ms`).
    pub fn coarse_estimate(
        self,
        clock: ClockOrigin,
        tick: MessageTickMs,
    ) -> Result<SessionTimeUs, ClockRangeError> {
        let delta = self.tick_delta_ms(tick).ok_or(ClockRangeError::OutOfRange)?;
        let anchor = clock.time(self.qpc)?;
        checked_i64(i128::from(anchor.0) + i128::from(delta) * 1_000).map(SessionTimeUs)
    }
}

fn checked_i64(value: i128) -> Result<i64, ClockRangeError> {
    i64::try_from(value).map_err(|_| ClockRangeError::OutOfRange)
}
