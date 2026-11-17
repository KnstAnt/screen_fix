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

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

/// Lock-free loss evidence written by a callback or worker and drained by
/// one owner: a count and the first and last receipt QPC of the lost items.
#[derive(Default)]
pub struct LossCounter {
    count: AtomicU64,
    first: AtomicI64,
    last: AtomicI64,
}

impl LossCounter {
    pub fn note(&self, count: u64, first: i64, last: i64) {
        if count == 0 { return; }
        let _ = self.count.fetch_update(Relaxed, Relaxed, |value| Some(value.saturating_add(count)));
        let _ = self.first.compare_exchange(0, first, Relaxed, Relaxed);
        self.last.fetch_max(last, Relaxed);
    }

    /// The count and receipt bounds since the last drain.
    pub fn take(&self) -> Option<(u64, screen_fix_protocol::QpcTick, screen_fix_protocol::QpcTick)> {
        let count = self.count.swap(0, Relaxed);
        let last = self.last.swap(0, Relaxed);
        let first = self.first.swap(0, Relaxed);
        // A concurrent note can leave only the upper bound for this drain.
        (count != 0).then(|| (count, screen_fix_protocol::QpcTick(if first == 0 { last } else { first }),
            screen_fix_protocol::QpcTick(last)))
    }
}
