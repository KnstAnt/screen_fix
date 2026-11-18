//! Shared Windows native observation and input collection mechanics.

#[cfg(not(target_os = "windows"))]
compile_error!("Screen Fix supports only Windows.");

pub mod channel;
pub mod focus;
pub mod input;
pub mod owner;
pub mod wake;

#[cfg(feature = "context")]
pub mod com;
#[cfg(feature = "context")]
pub mod events;
#[cfg(feature = "context")]
pub mod held;
#[cfg(feature = "context")]
pub mod hung;
#[cfg(feature = "context")]
pub mod identity;
#[cfg(feature = "context")]
pub mod ime;
#[cfg(feature = "context")]
pub mod uia;
#[cfg(feature = "context")]
pub mod uia_commits;
#[cfg(feature = "context")]
pub mod windows;

use std::{
    num::NonZeroU32,
    sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::{AcqRel, Acquire, Relaxed, Release}},
};

use screen_fix_protocol::{QpcTick, ipc::ProcessInstance};
use windows_sys::Win32::{Foundation::{FILETIME, HANDLE}, System::Threading::GetProcessTimes};

/// The instance of process `pid`, identified by its creation FILETIME, read
/// from a handle with query access to it.
pub fn process_instance(process: HANDLE, pid: NonZeroU32) -> Option<ProcessInstance> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let creation_filetime = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some(ProcessInstance { pid, creation_filetime })
}

/// The claim on a process-wide single-flight latch, released on drop.
#[must_use]
pub struct StaticLease(&'static AtomicBool);

impl StaticLease {
    /// Claim `latch`; `None` while another owner holds it.
    pub fn claim(latch: &'static AtomicBool) -> Option<Self> {
        latch.compare_exchange(false, true, AcqRel, Acquire).ok().map(|_| Self(latch))
    }
}

impl Drop for StaticLease {
    fn drop(&mut self) {
        self.0.store(false, Release);
    }
}

/// A run of lost items: their count and the first and last receipt QPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LossSpan {
    pub count: u64,
    pub first: QpcTick,
    pub last: QpcTick,
}

impl LossSpan {
    /// One item lost at `at`.
    pub const fn at(at: QpcTick) -> Self {
        Self { count: 1, first: at, last: at }
    }

    /// Counts one more item lost at `at`.
    pub fn note(&mut self, at: QpcTick) {
        self.merge(Self::at(at));
    }

    /// Adds the items of `other`, widening the bounds to cover both.
    pub fn merge(&mut self, other: Self) {
        self.count = self.count.saturating_add(other.count);
        self.first = self.first.min(other.first);
        self.last = self.last.max(other.last);
    }
}

/// Lock-free loss evidence written by a callback or worker and drained by
/// one owner: a count and the first and last receipt QPC of the lost items.
/// A receipt of zero is a missing bound.
#[derive(Default)]
pub struct LossCounter {
    count: AtomicU64,
    first: AtomicI64,
    last: AtomicI64,
    /// The upper bound of the previous drain; only the draining owner uses it.
    drained: AtomicI64,
}

impl LossCounter {
    pub fn note(&self, span: LossSpan) {
        if span.count == 0 { return; }
        // The bounds are written before the count is published (C3). A drain
        // that observes this count therefore observes these bounds, unless an
        // earlier drain that raced this note already took them.
        let _ = self.first.compare_exchange(0, span.first.0, Relaxed, Relaxed);
        self.last.fetch_max(span.last.0, Relaxed);
        let _ = self.count.fetch_update(Release, Relaxed, |value| Some(value.saturating_add(span.count)));
    }

    /// The count and receipt bounds since the last drain. The bounds are
    /// never zero and never inverted.
    pub fn take(&self) -> Option<LossSpan> {
        let count = self.count.swap(0, Acquire);
        if count == 0 { return None; }
        let first = self.first.swap(0, Relaxed);
        let last = self.last.swap(0, Relaxed);
        // A note that raced the previous drain may have left all or part of
        // its bounds to that drain. Its items then lie at or before the
        // previous upper bound, so a missing bound takes the other bound,
        // or the previous upper bound when both are missing.
        let (first, last) = match (first, last) {
            (0, 0) => {
                let previous = self.drained.load(Relaxed);
                (previous, previous)
            }
            (0, bound) | (bound, 0) => (bound, bound),
            (first, last) => (first.min(last), first.max(last)),
        };
        self.drained.store(last, Relaxed);
        Some(LossSpan { count, first: QpcTick(first), last: QpcTick(last) })
    }
}
