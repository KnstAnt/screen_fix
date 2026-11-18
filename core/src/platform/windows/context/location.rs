use std::{collections::VecDeque, time::Instant};

use crate::protocol::WindowId;

use super::WINDOW_REFRESH;

const WINDOW_RATE_CACHE: usize = 64;

/// The per-window rate limit of location rows (C4): one read per window per
/// `WINDOW_REFRESH`, and one trailing re-read of a window whose notices it dropped.
pub(super) struct LocationLimiter {
    /// The last location read per window.
    reads: VecDeque<(WindowId, Instant)>,
    /// Windows whose location notices the limit dropped.
    dirty: Vec<WindowId>,
}

impl LocationLimiter {
    pub fn new() -> Self {
        Self { reads: VecDeque::with_capacity(WINDOW_RATE_CACHE), dirty: Vec::new() }
    }

    /// Drop the pending trailing re-reads at a pause. The read history is
    /// kept, so it still limits the notices after the resume.
    pub fn reset(&mut self) {
        self.dirty.clear();
    }

    fn limited(&self, window: WindowId, now: Instant) -> bool {
        self.reads.iter().any(|(id, last)| *id == window && now.saturating_duration_since(*last) < WINDOW_REFRESH)
    }

    /// Whether a location notice of `window` at `now` is dropped by the limit;
    /// a dropped notice leaves the window due a trailing re-read.
    pub fn defer(&mut self, window: WindowId, now: Instant) -> bool {
        if !self.limited(window, now) {
            return false;
        }
        self.mark_dirty(window);
        true
    }

    /// Remember a window whose location notice the limit dropped.
    pub fn mark_dirty(&mut self, window: WindowId) {
        if !self.dirty.contains(&window) && self.dirty.len() < WINDOW_RATE_CACHE {
            self.dirty.push(window);
        }
    }

    /// Record a location read of `window`. The read also serves any pending
    /// trailing re-read of the window.
    pub fn rate(&mut self, window: WindowId, now: Instant) {
        self.dirty.retain(|dirty| *dirty != window);
        self.reads.retain(|(id, _)| *id != window);
        if self.reads.len() == WINDOW_RATE_CACHE {
            self.reads.pop_front();
        }
        self.reads.push_back((window, now));
    }

    /// Take the windows whose dropped location notices are now due a re-read.
    pub fn due(&mut self, now: Instant) -> Vec<WindowId> {
        let due: Vec<_> = self.dirty.iter().copied().filter(|dirty| !self.limited(*dirty, now)).collect();
        self.dirty.retain(|window| !due.contains(window));
        due
    }

    /// Forget a destroyed window.
    pub fn forget(&mut self, window: WindowId) {
        self.reads.retain(|(id, _)| *id != window);
    }
}
