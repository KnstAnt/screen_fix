use std::{
    sync::{Condvar, Mutex, PoisonError},
    time::Instant,
};

#[derive(Default)]
struct Boundaries {
    start: Option<Instant>,
    stop: Option<Instant>,
}

/// Orders Start, observation admission and Stop. Work runs outside the lock.
#[derive(Default)]
pub(crate) struct SessionGate {
    boundaries: Mutex<Boundaries>,
    changed: Condvar,
}

impl SessionGate {
    pub(crate) fn stop(&self) {
        let mut state = self.boundaries.lock().unwrap_or_else(PoisonError::into_inner);
        state.stop.get_or_insert_with(Instant::now);
        self.changed.notify_all();
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn open(&self, start: Instant) -> bool {
        let mut state = self.boundaries.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stop.is_some() || state.start.is_some() {
            return false;
        }
        state.start = Some(start);
        self.changed.notify_all();
        true
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn wait_for_start(&self) -> Option<Instant> {
        let state = self.boundaries.lock().unwrap_or_else(PoisonError::into_inner);
        let state = self.changed.wait_while(state, |state| {
            state.start.is_none() && state.stop.is_none()
        }).unwrap_or_else(PoisonError::into_inner);
        state.start
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn wait_until(&self, deadline: Instant) -> Option<Instant> {
        let mut state = self.boundaries.lock().unwrap_or_else(PoisonError::into_inner);
        while state.stop.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            (state, _) = self.changed.wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.stop
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn admit(&self) -> Result<Instant, Instant> {
        let state = self.boundaries.lock().unwrap_or_else(PoisonError::into_inner);
        match (state.start, state.stop) {
            (_, Some(stopped_at)) => Err(stopped_at),
            (Some(_), None) => Ok(Instant::now()),
            (None, None) => Err(Instant::now()),
        }
    }
}
