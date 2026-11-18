//! The only Windows clock reads used by the recording lifecycle.

use std::num::NonZeroU64;

use windows::Win32::System::{
    Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    SystemInformation::{GetSystemTimePreciseAsFileTime, GetTickCount64},
};

use crate::protocol::{ClockOrigin, CoarseTime, MessageTickAnchor, QpcFrequency, QpcTick, SessionTimeUs, UtcAnchor};

use super::{ProbeError, stage, unavailable};

pub(crate) fn frequency() -> Result<QpcFrequency, ProbeError> {
    let mut value = 0;
    stage(unsafe { QueryPerformanceFrequency(&mut value) }, "Read the QPC frequency")?;
    u64::try_from(value).ok().and_then(NonZeroU64::new).map(QpcFrequency)
        .ok_or_else(|| unavailable("Invalid QPC frequency"))
}

pub(crate) fn qpc() -> Result<QpcTick, ProbeError> {
    let mut value = 0;
    stage(unsafe { QueryPerformanceCounter(&mut value) }, "Read the QPC counter")?;
    Ok(QpcTick(value))
}

pub(crate) fn utc_anchor() -> Result<UtcAnchor, ProbeError> {
    let before = qpc()?;
    let utc = unsafe { GetSystemTimePreciseAsFileTime() };
    let (qpc, qpc_bracket_ticks) = before.bracket(qpc()?)
        .map_err(|_| unavailable("QPC moved backward across a UTC sample"))?;
    let utc_filetime_100ns = (u64::from(utc.dwHighDateTime) << 32) | u64::from(utc.dwLowDateTime);
    Ok(UtcAnchor { qpc, utc_filetime_100ns, qpc_bracket_ticks })
}

pub(super) fn uptime_ms() -> u64 {
    unsafe { GetTickCount64() }
}

pub(crate) fn message_tick_anchor() -> Result<MessageTickAnchor, ProbeError> {
    let before = qpc()?;
    let tick64_ms = uptime_ms();
    let (qpc, qpc_bracket_ticks) = before.bracket(qpc()?)
        .map_err(|_| unavailable("QPC moved backward across a message tick sample"))?;
    Ok(MessageTickAnchor { qpc, tick64_ms, qpc_bracket_ticks })
}

pub(super) fn message_tick_interval(
    anchor: MessageTickAnchor, frequency: QpcFrequency, coarse: CoarseTime,
) -> Option<(QpcTick, QpcTick)> {
    let delta = anchor.tick_delta_ms(coarse.tick_ms)?;
    let frequency = i128::from(frequency.0.get());
    let center = i128::from(anchor.qpc.0) + (i128::from(delta) * frequency).div_euclid(1000);
    let uncertainty = (i128::from(coarse.uncertainty_us.0) * frequency + 999_999) / 1_000_000;
    let radius = uncertainty + i128::from(anchor.qpc_bracket_ticks) + 1;
    Some((QpcTick(i64::try_from(center - radius).ok()?), QpcTick(i64::try_from(center + radius).ok()?)))
}

pub(super) struct MessageTickEstimate {
    center_us: i128,
    uncertainty_us: i128,
    bracket_us: i128,
}

impl MessageTickEstimate {
    pub(super) fn predates(&self, boundary: SessionTimeUs) -> bool {
        self.center_us + self.uncertainty_us + self.bracket_us < i128::from(boundary.0)
    }

    pub(super) fn is_late(&self, received: SessionTimeUs) -> bool {
        i128::from(received.0) - self.center_us > 2 * self.uncertainty_us + self.bracket_us
    }
}

pub(super) fn message_tick_estimate(
    origin: ClockOrigin, anchor: Option<MessageTickAnchor>, coarse: CoarseTime,
) -> Option<MessageTickEstimate> {
    let anchor = anchor?;
    let center_us = i128::from(anchor.coarse_estimate(origin, coarse.tick_ms).ok()?.0);
    let frequency = i128::from(origin.qpc_frequency.0.get());
    let bracket_us = (i128::from(anchor.qpc_bracket_ticks) * 1_000_000 + frequency - 1) / frequency;
    Some(MessageTickEstimate { center_us, uncertainty_us: i128::from(coarse.uncertainty_us.0), bracket_us })
}
