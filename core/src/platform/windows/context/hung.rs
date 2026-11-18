//! IMG-8 observation uses HWND ownership and native hung signals without
//! calling the application's UI thread.

use std::time::{Duration, Instant};

use screen_fix_native_observation::{focus::window_owner, hung::{self, CandidateEvidence}};
use serde_json::{Value, json};
use windows_sys::Win32::Foundation::HWND;

pub(super) use hung::is_ghost;

use crate::{
    capture::context::BusyReasons,
    protocol::{WindowId, ipc::ProcessInstance},
    util::push_bounded,
};

use super::identity::{ApprovedProcesses, WindowProcess};

const REFRESH: Duration = Duration::from_secs(1);
const ACTIVE_LIMIT: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Observation {
    window: WindowProcess,
    direct: bool,
    ghost: bool,
}

pub(super) struct Hung {
    active: Vec<Observation>,
    /// Ghost windows verifiably replacing an active approved hung window (IMG-8).
    ghosts: Vec<(WindowId, WindowProcess)>,
    next_scan: Instant,
    coverage_limited: bool,
    ghost_unknown: bool,
    probe: hung::Probe,
}

impl Hung {
    pub fn new(now: Instant) -> Self {
        Self {
            active: Vec::with_capacity(ACTIVE_LIMIT), ghosts: Vec::new(), next_scan: now, coverage_limited: false,
            ghost_unknown: false, probe: hung::Probe::new(),
        }
    }

    pub fn contains(&self, process: ProcessInstance) -> bool {
        self.active.iter().any(|observation| observation.window.process == process)
    }

    /// The hung approved window a Ghost replaces, from the latest scan.
    pub fn ghost_original(&self, ghost: WindowId) -> Option<WindowProcess> {
        self.ghosts.iter().find(|(window, _)| *window == ghost).map(|(_, original)| *original)
    }

    pub fn coverage_limited(&self) -> bool {
        self.coverage_limited
    }

    pub fn processes(&self) -> impl Iterator<Item = ProcessInstance> + '_ {
        self.active.iter().map(|observation| observation.window.process)
    }

    pub fn poll(
        &mut self, now: Instant, foreground_changed: bool, foreground: WindowId,
        approvals: &mut ApprovedProcesses<'_>, rows: &mut Vec<Value>,
    ) -> bool {
        if !foreground_changed && now < self.next_scan {
            return false;
        }
        // Frequent foreground changes may trigger extra passes but do not
        // postpone the periodic pass.
        if now >= self.next_scan {
            self.next_scan = now + REFRESH;
        }
        let (candidates, complete) = hung::candidates();
        let mut scan = Scan {
            approvals, tracker: self,
            observations: Vec::with_capacity(ACTIVE_LIMIT), foreground,
            foreground_ghost_known: false, ghost_unknown: false, limited: !complete,
        };
        let mut ghosts = Vec::new();
        for candidate in candidates {
            match self.probe.observe_candidate(candidate) {
                CandidateEvidence::Window { window, ghost } => {
                    scan.foreground_ghost_known |= ghost && scan.foreground.0 == candidate.window as usize as u64;
                    scan.observe(window, ghost);
                    if ghost && ghosts.len() < ACTIVE_LIMIT {
                        ghosts.push((WindowId(candidate.window as usize as u64), window));
                    }
                }
                CandidateEvidence::UnknownGhost => scan.ghost_unknown = true,
                CandidateEvidence::Changed => {}
            }
        }
        let mut next = scan.observations;
        let mut limited = scan.limited;
        let ghost_unknown = scan.ghost_unknown || (!scan.foreground_ghost_known
            && is_ghost(foreground.0 as usize as HWND)) || (!complete && self.ghost_unknown);
        if ghost_unknown != self.ghost_unknown {
            rows.push(json!({
                "kind": "app_hung_ghost", "active": ghost_unknown,
                "association": if ghost_unknown { Some("unknown") } else { None },
            }));
        }
        for old in &self.active {
            let window = old.window;
            if next.iter().any(|new| new.window == window) {
                continue;
            }
            let hwnd = window.hwnd();
            let owner = window_owner(hwnd.0);
            let proof = WindowProcess::read(hwnd);
            let gone = owner != Some(window.owner())
                || proof.is_some_and(|proof| proof != window);
            if !gone && (!complete || proof.is_none() || hung::is_hung(hwnd.0)) {
                // Prefer an existing interval over an untracked new one if
                // the sample is truncated. Do not report a false recovery.
                if next.len() == ACTIVE_LIMIT {
                    limited = true;
                    let replace = next.iter().rposition(|new| !self.active.iter().any(|entry| entry.window == new.window));
                    if let Some(index) = replace { next.swap_remove(index); }
                }
                push_bounded(&mut next, *old, ACTIVE_LIMIT);
            } else {
                let reason = if gone { "window_gone" }
                    else if ghost_unknown { "ghost_association_unknown" }
                    else { "responsive" };
                rows.push(hung_row(*old, false, reason));
            }
        }
        for new in &next {
            let previous = self.active.iter().find(|old| old.window == new.window);
            if previous.is_none() {
                rows.push(hung_row(*new, true, "detected"));
            } else if new.ghost && previous.is_some_and(|old| !old.ghost) {
                rows.push(json!({ "kind": "app_hung_ghost", "active": true, "association": "confirmed",
                    "window": new.window.window, "process": new.window.process }));
            }
        }
        if limited != self.coverage_limited {
            rows.push(json!({ "kind": "app_hung_coverage", "complete": !limited }));
        }
        ghosts.retain(|(_, original)| next.iter().any(|observed| observed.window == *original && observed.ghost));
        self.ghosts = ghosts;
        self.busy_changes(&next, rows);
        self.active = next;
        self.coverage_limited = limited;
        self.ghost_unknown = ghost_unknown;
        true
    }

    pub fn observe_scoped(&mut self, window: WindowProcess, direct: bool, ghost: bool, rows: &mut Vec<Value>) {
        let previous = self.active.iter().find(|item| item.window == window).copied();
        let observed = (direct || ghost).then_some(Observation { window, direct, ghost });
        if previous == observed { return; }
        let mut next = self.active.clone();
        next.retain(|item| item.window != window);
        if let Some(observed) = observed {
            if next.len() == ACTIVE_LIMIT {
                rows.push(json!({ "kind": "app_hung_coverage", "complete": false, "source": "elevated" }));
                return;
            }
            next.push(observed);
            if previous.is_none() { rows.push(hung_row(observed, true, "detected")); }
            if ghost && previous.is_none_or(|previous| !previous.ghost) {
                rows.push(json!({ "kind": "app_hung_ghost", "active": true, "association": "confirmed",
                    "window": window.window, "process": window.process }));
            }
        } else if let Some(previous) = previous {
            rows.push(hung_row(previous, false, "responsive"));
        }
        self.busy_changes(&next, rows);
        self.active = next;
    }

    pub fn close(&mut self) -> Vec<Value> {
        let mut rows = self.active.iter().map(|old| hung_row(*old, false, "observation_ended")).collect();
        self.busy_changes(&[], &mut rows);
        self.active.clear();
        self.ghosts.clear();
        if std::mem::take(&mut self.ghost_unknown) {
            rows.push(json!({ "kind": "app_hung_ghost", "active": false, "association": null }));
        }
        self.coverage_limited = false;
        self.next_scan = Instant::now();
        rows
    }

    fn busy_changes(&self, next: &[Observation], rows: &mut Vec<Value>) {
        for (before, after, active) in [(&self.active[..], next, true), (next, &self.active[..], false)] {
            for (index, observed) in after.iter().enumerate() {
                let process = observed.window.process;
                if before.iter().chain(&after[..index]).any(|other| other.window.process == process) {
                    continue;
                }
                let reasons = BusyReasons { hung: active, ..BusyReasons::default() };
                rows.push(json!({ "kind": "app_busy", "approved": true, "scope": "process_hung",
                    "process": process, "hint": { "window": observed.window.window, "level": reasons.level(), "reasons": reasons } }));
            }
        }
    }
}

struct Scan<'tracker, 'approvals, 'profile> {
    approvals: &'approvals mut ApprovedProcesses<'profile>,
    tracker: &'tracker Hung,
    observations: Vec<Observation>,
    foreground: WindowId,
    foreground_ghost_known: bool,
    ghost_unknown: bool,
    limited: bool,
}

impl Scan<'_, '_, '_> {
    fn observe(&mut self, window: WindowProcess, ghost: bool) {
        if self.approvals.get(window.process).is_none() || WindowProcess::read(window.hwnd()) != Some(window) {
            return;
        }
        if let Some(existing) = self.observations.iter_mut().find(|entry| entry.window == window) {
            existing.direct |= !ghost;
            existing.ghost |= ghost;
        } else if self.observations.len() == ACTIVE_LIMIT {
            self.limited = true;
            if self.tracker.active.iter().any(|old| old.window == window)
                && let Some(index) = self.observations.iter().rposition(|new| {
                    !self.tracker.active.iter().any(|old| old.window == new.window)
                })
            {
                self.observations[index] = Observation { window, direct: !ghost, ghost };
            }
        } else {
            self.observations.push(Observation { window, direct: !ghost, ghost });
        }
    }
}

fn hung_row(observed: Observation, active: bool, reason: &str) -> Value {
    let window = observed.window;
    json!({ "kind": "app_hung", "window": window.window, "process": window.process,
        "thread_id": window.thread, "active": active, "reason": reason,
        "native_hung": observed.direct, "ghost_replacement": observed.ghost })
}
