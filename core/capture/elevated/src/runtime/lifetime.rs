use std::{
    sync::{Arc, OnceLock, atomic::{AtomicI64, Ordering::{AcqRel, Acquire, Release}}},
    time::{Duration, Instant},
};

use screen_fix_protocol::{QpcFrequency, QpcTick, ipc::elevated::runtime::Lease};
use windows_sys::Win32::System::Performance::QueryPerformanceCounter;

use crate::win::{Failure, Result};

pub const HEARTBEAT: Duration = Duration::from_millis(250);
pub(crate) const LEASE: Duration = Duration::from_secs(1);
const NANOS_PER_SECOND: u128 = 1_000_000_000;

pub(crate) fn qpc() -> Result<QpcTick> {
    let mut tick = 0;
    if unsafe { QueryPerformanceCounter(&mut tick) } == 0 || tick <= 0 {
        return Err(Failure::Supervisor);
    }
    Ok(QpcTick(tick))
}

/// The watchdog and control owner arbitrate expiry with a single atomic
/// deadline. Zero is a terminal latch, including on a clock fault.
pub(crate) struct Lifetime {
    expiry: AtomicI64,
    ticks_per_second: i64,
    pub(super) witness: OnceLock<Arc<AtomicI64>>,
    cleanup: AtomicI64,
    owner: AtomicI64,
    providers: [AtomicI64; ProviderKind::COUNT],
}

impl Lifetime {
    pub(crate) fn new(frequency: QpcFrequency, startup: Instant) -> Result<Self> {
        let ticks_per_second = i64::try_from(frequency.0.get()).map_err(|_| Failure::Supervisor)?;
        let remaining = startup.saturating_duration_since(Instant::now());
        let ticks = (remaining.as_nanos() * ticks_per_second as u128) / NANOS_PER_SECOND;
        let ticks = i64::try_from(ticks).map_err(|_| Failure::Supervisor)?;
        let expiry = qpc()?.0.checked_add(ticks).ok_or(Failure::Supervisor)?;
        Ok(Self {
            expiry: AtomicI64::new(expiry), ticks_per_second, witness: OnceLock::new(), cleanup: AtomicI64::new(0), owner: AtomicI64::new(0),
            providers: [const { AtomicI64::new(0) }; ProviderKind::COUNT],
        })
    }

    pub(crate) fn check(&self, now: QpcTick) -> Result<()> {
        let mut expiry = self.expiry.load(Acquire);
        loop {
            if expiry == 0 {
                return Err(Failure::LeaseExpired);
            }
            if now.0 < expiry {
                return Ok(());
            }
            match self.expiry.compare_exchange(expiry, 0, AcqRel, Acquire) {
                Ok(_) => return Err(Failure::LeaseExpired),
                Err(updated) => expiry = updated,
            }
        }
    }

    pub(super) fn lease_current(&self, now: QpcTick) -> bool {
        now.0 > 0 && now.0 < self.expiry.load(Acquire)
    }

    pub(crate) fn renew(&self, old: Option<Lease>, lease: Lease, now: QpcTick) -> Result<()> {
        let lease_ticks = self.ticks(LEASE)?;
        let oldest = now.0.saturating_sub(lease_ticks);
        if lease.controller_qpc.0 <= oldest || lease.controller_qpc > now
            || lease.pump_qpc.0 <= oldest || lease.pump_qpc > now
            || old.map_or(lease.sequence.get() != 1, |old| {
                old.sequence.get().checked_add(1) != Some(lease.sequence.get())
                    || old.controller_qpc >= lease.controller_qpc || old.pump_qpc >= lease.pump_qpc
            })
        {
            return Err(Failure::LeaseExpired);
        }
        let valid_until = lease.controller_qpc.min(lease.pump_qpc).0
            .checked_add(lease_ticks).ok_or(Failure::Supervisor)?;
        let mut expiry = self.expiry.load(Acquire);
        loop {
            if expiry == 0 || now.0 >= expiry {
                self.check(now)?;
                expiry = self.expiry.load(Acquire);
                continue;
            }
            match self.expiry.compare_exchange(expiry, valid_until, AcqRel, Acquire) {
                Ok(_) => return Ok(()),
                Err(updated) => expiry = updated,
            }
        }
    }

    pub(crate) fn witness_healthy(&self, now: QpcTick) -> bool {
        self.witness.get().is_none_or(|progress| {
            self.ticks(HEARTBEAT).is_ok_and(|max_age| super::witness::fresh(progress, now, max_age))
        })
    }

    pub(super) fn ticks(&self, duration: Duration) -> Result<i64> {
        let ticks = duration.as_nanos().checked_mul(self.ticks_per_second as u128)
            .ok_or(Failure::Supervisor)? / NANOS_PER_SECOND;
        i64::try_from(ticks).map_err(|_| Failure::Supervisor)
    }

    pub(super) fn enter_cleanup(&self, now: QpcTick) -> Result<()> {
        self.begin_cleanup(now, None)
    }

    pub(super) fn begin_cleanup(&self, now: QpcTick, limit: Option<QpcTick>) -> Result<()> {
        let deadline = now.0.checked_add(self.ticks(crate::deadline::CLEANUP)?).ok_or(Failure::Supervisor)?;
        let deadline = limit.map_or(deadline, |limit| limit.0.min(deadline));
        self.cleanup.fetch_update(AcqRel, Acquire, |active| Some(if active == 0 { deadline } else { active.min(deadline) }))
            .map_err(|_| Failure::Supervisor)?;
        Ok(())
    }

    pub(super) fn end_cleanup(&self) {
        self.cleanup.store(0, Release);
    }

    pub(super) fn provider(&self, kind: ProviderKind, deadline: QpcTick) -> Option<Provider<'_>> {
        let slot = &self.providers[kind as usize];
        if deadline <= qpc().ok()? { return None; }
        slot.compare_exchange(0, deadline.0, AcqRel, Acquire).ok()?;
        Some(Provider { slot })
    }

    pub(crate) fn provider_expired(&self, now: QpcTick) -> bool {
        self.providers.iter().any(|slot| {
            let until = slot.load(Acquire);
            until != 0 && now.0 >= until
        })
    }

    pub(super) fn owner_progress(&self, now: QpcTick) {
        self.owner.store(now.0, Release);
    }

    pub(crate) fn owner_healthy(&self, now: QpcTick) -> bool {
        let last = self.owner.load(Acquire);
        last == 0 || self.cleanup.load(Acquire) != 0
            || now.0.saturating_sub(last) <= self.ticks(crate::deadline::CLEANUP).unwrap_or(0)
    }

    pub(crate) fn cleanup_expired(&self, now: QpcTick) -> bool {
        let deadline = self.cleanup.load(Acquire);
        deadline != 0 && now.0 >= deadline
    }

    pub(crate) fn stop(&self) {
        self.expiry.store(0, std::sync::atomic::Ordering::Release);
    }
}


pub(super) enum ProviderKind {
    Read,
    Callback,
    Ime,
    Egress,
    Teardown,
}

impl ProviderKind {
    const COUNT: usize = Self::Teardown as usize + 1;
}

pub(super) struct Provider<'a> {
    slot: &'a AtomicI64,
}

impl Drop for Provider<'_> {
    fn drop(&mut self) { self.slot.store(0, Release); }
}
