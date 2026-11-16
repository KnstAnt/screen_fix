//! IMG-8 observation uses HWND ownership and native hung signals without
//! calling the application's UI thread.

use std::{panic::{AssertUnwindSafe, catch_unwind}, time::{Duration, Instant}};

use serde_json::{Value, json};
use windows::{
    Win32::{Foundation::HWND as NativeWindow, System::LibraryLoader::{GetModuleHandleW, GetProcAddress}},
    core::{PCSTR, w},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM},
    UI::WindowsAndMessaging::{
        EnumWindows, GA_ROOT, GetAncestor, GetClassNameW, GetWindowThreadProcessId, IsHungAppWindow,
    },
};

use crate::{
    capture::context::BusyReasons,
    protocol::{WindowId, ipc::ProcessInstance},
};

use super::identity::{ApprovedProcesses, WindowProcess};

const REFRESH: Duration = Duration::from_secs(1);
const ACTIVE_LIMIT: usize = 64;
const CANDIDATE_LIMIT: usize = 512;
type GhostLookup = unsafe extern "system" fn(HWND) -> HWND;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Observation {
    window: WindowProcess,
    direct: bool,
    ghost: bool,
}

pub(super) struct Hung {
    active: Vec<Observation>,
    next_scan: Instant,
    coverage_limited: bool,
    ghost_unknown: bool,
    ghost_to_original: Option<GhostLookup>,
    original_to_ghost: Option<GhostLookup>,
}

impl Hung {
    pub fn new(now: Instant) -> Self {
        // These optional user32 exports are private. They resolve only from
        // the already loaded system DLL. Neither is needed for ordinary HWND
        // checks and class evidence alone never attributes a Ghost.
        let user32 = unsafe { GetModuleHandleW(w!("user32.dll")) }.ok();
        let lookup = |name: &'static [u8]| {
            unsafe { GetProcAddress(user32?, PCSTR(name.as_ptr())) }
                .map(|address| unsafe { std::mem::transmute::<_, GhostLookup>(address) })
        };
        Self {
            active: Vec::with_capacity(ACTIVE_LIMIT), next_scan: now, coverage_limited: false,
            ghost_unknown: false, ghost_to_original: lookup(b"HungWindowFromGhostWindow\0"),
            original_to_ghost: lookup(b"GhostWindowFromHungWindow\0"),
        }
    }

    pub fn contains(&self, process: ProcessInstance) -> bool {
        self.active.iter().any(|observation| observation.window.process == process)
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
        let mut candidates = Vec::with_capacity(CANDIDATE_LIMIT);
        // The callback is synchronous, does not retain the reference and
        // collects only bounded native evidence. Approval happens below.
        let complete = unsafe { EnumWindows(Some(read_window), (&mut candidates as *mut Vec<Candidate>) as LPARAM) } != 0;
        let mut scan = Scan {
            approvals, tracker: self,
            observations: Vec::with_capacity(ACTIVE_LIMIT), foreground,
            foreground_ghost_known: false, ghost_unknown: false, limited: !complete,
        };
        for candidate in candidates {
            scan.observe_candidate(candidate);
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
            let owner = native_owner(hwnd.0);
            let proof = WindowProcess::read(hwnd);
            let gone = owner != Some((window.process.pid.get(), window.thread.get()))
                || proof.is_some_and(|proof| proof != window);
            if !gone && (!complete || proof.is_none() || unsafe { IsHungAppWindow(hwnd.0) } != 0) {
                // Prefer an existing interval over an untracked new one if
                // the sample is truncated. Do not report a false recovery.
                if next.len() == ACTIVE_LIMIT {
                    limited = true;
                    let replace = next.iter().rposition(|new| !self.active.iter().any(|entry| entry.window == new.window));
                    if let Some(index) = replace { next.swap_remove(index); }
                }
                if next.len() < ACTIVE_LIMIT { next.push(*old); }
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
        self.busy_changes(&next, rows);
        self.active = next;
        self.coverage_limited = limited;
        self.ghost_unknown = ghost_unknown;
        true
    }

    pub fn close(&mut self) -> Vec<Value> {
        let mut rows = self.active.iter().map(|old| hung_row(*old, false, "observation_ended")).collect();
        self.busy_changes(&[], &mut rows);
        self.active.clear();
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

    fn ghost_original(&self, ghost: HWND) -> Option<WindowProcess> {
        let reverse = self.ghost_to_original?;
        let owner = native_owner(ghost)?;
        let original = unsafe { reverse(ghost) };
        let proof = WindowProcess::read(NativeWindow(original))?;
        let pair_matches = |forward: GhostLookup| unsafe { forward(original) == ghost };
        if original == ghost || !top_level(original) || is_ghost(original)
            || self.original_to_ghost.is_some_and(|forward| !pair_matches(forward))
            || unsafe { reverse(ghost) } != original || native_owner(ghost) != Some(owner)
            || !is_ghost(ghost) || !top_level(ghost) || unsafe { IsHungAppWindow(ghost) } == 0
            || WindowProcess::read(NativeWindow(original)) != Some(proof)
        {
            return None;
        }
        Some(proof)
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
    fn observe_candidate(&mut self, candidate: Candidate) {
        let hwnd = candidate.window;
        if native_owner(hwnd) != Some(candidate.owner) || !top_level(hwnd) || unsafe { IsHungAppWindow(hwnd) } == 0 {
            return;
        }
        if is_ghost(hwnd) {
            if let Some(window) = self.tracker.ghost_original(hwnd) {
                self.foreground_ghost_known |= self.foreground.0 == hwnd as usize as u64;
                self.observe(window, true);
            } else {
                self.ghost_unknown = true;
            }
        } else if let Some(window) = WindowProcess::read(NativeWindow(hwnd))
            && candidate.owner == (window.process.pid.get(), window.thread.get())
            && !is_ghost(hwnd) && unsafe { IsHungAppWindow(hwnd) } != 0
        {
            self.observe(window, false);
        }
    }

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

struct Candidate {
    window: HWND,
    owner: (u32, u32),
}

unsafe extern "system" fn read_window(hwnd: HWND, data: LPARAM) -> i32 {
    let candidates = unsafe { &mut *(data as *mut Vec<Candidate>) };
    let keep_going = catch_unwind(AssertUnwindSafe(|| {
        if !top_level(hwnd) || unsafe { IsHungAppWindow(hwnd) } == 0 {
            return true;
        }
        if candidates.len() == CANDIDATE_LIMIT {
            return false;
        }
        if let Some(owner) = native_owner(hwnd) {
            candidates.push(Candidate { window: hwnd, owner });
        }
        true
    })).unwrap_or(false);
    if keep_going { 1 } else { 0 }
}

pub(super) fn is_ghost(window: HWND) -> bool {
    let mut class = [0_u16; 7];
    let length = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    length == 5 && class[..5] == [b'G' as u16, b'h' as u16, b'o' as u16, b's' as u16, b't' as u16]
}

fn top_level(window: HWND) -> bool {
    !window.is_null() && unsafe { GetAncestor(window, GA_ROOT) } == window
}

fn native_owner(window: HWND) -> Option<(u32, u32)> {
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    (thread != 0 && pid != 0).then_some((pid, thread))
}

fn hung_row(observed: Observation, active: bool, reason: &str) -> Value {
    let window = observed.window;
    json!({ "kind": "app_hung", "window": window.window, "process": window.process,
        "thread_id": window.thread, "active": active, "reason": reason,
        "native_hung": observed.direct, "ghost_replacement": observed.ghost })
}
