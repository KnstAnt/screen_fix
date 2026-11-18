//! Shared Windows native observation and input collection mechanics.
#![cfg(target_os = "windows")]

pub mod focus;
pub mod input;

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

use std::{num::NonZeroU32, sync::atomic::{AtomicI64, AtomicU64, Ordering::{Acquire, Relaxed, Release}}};

use screen_fix_protocol::ipc::ProcessInstance;
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

/// Lock-free loss evidence written by a callback or worker and drained by
/// one owner: a count and the first and last receipt QPC of the lost items.
#[derive(Default)]
pub struct LossCounter {
    count: AtomicU64,
    first: AtomicI64,
    last: AtomicI64,
    /// The upper bound of the previous drain; only the draining owner uses it.
    drained: AtomicI64,
}

impl LossCounter {
    pub fn note(&self, count: u64, first: i64, last: i64) {
        if count == 0 { return; }
        // The bounds are written before the count is published (C3). A drain
        // that observes this count therefore observes these bounds, unless an
        // earlier drain that raced this note already took them.
        let _ = self.first.compare_exchange(0, first, Relaxed, Relaxed);
        self.last.fetch_max(last, Relaxed);
        let _ = self.count.fetch_update(Release, Relaxed, |value| Some(value.saturating_add(count)));
    }

    /// The count and receipt bounds since the last drain. The bounds are
    /// never zero and never inverted.
    pub fn take(&self) -> Option<(u64, screen_fix_protocol::QpcTick, screen_fix_protocol::QpcTick)> {
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
        Some((count, screen_fix_protocol::QpcTick(first), screen_fix_protocol::QpcTick(last)))
    }
}
