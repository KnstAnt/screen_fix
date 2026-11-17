mod busy;
mod cursor;
mod events;
mod elevated;
mod frame_policy;
pub(super) use frame_policy::{ApprovalReadError, ELEVATED_FRAME_WAIT_US, FrameApproval, FramePolicy};
mod hung;
mod ime;
pub(super) use ime::State as ImeState;
pub(super) mod identity;
mod queries;
pub(super) use queries::PointerLink;
mod uia;
pub(super) use uia::FocusGuard;
mod uia_commits;
mod settings;
mod version;
mod windows;

use std::{collections::{BTreeMap, VecDeque}, time::{Duration, Instant}};

use serde_json::{Value, json};
use screen_fix_protocol::ipc::{FocusAtReceipt, elevated::{ReadBinding, VisitBinding}};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_FOCUS, EVENT_OBJECT_HIDE, EVENT_OBJECT_IME_CHANGE,
    EVENT_OBJECT_IME_HIDE, EVENT_OBJECT_IME_SHOW, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_SWITCHEND, EVENT_SYSTEM_SWITCHSTART, GA_ROOT, GetAncestor, GetForegroundWindow, OBJID_CARET, OBJID_CURSOR, OBJID_WINDOW,
};

use crate::{
    capture::{context::{BusyLevel, InputSettings, TextTarget}, frame::MonitorGeometry, save::SaveWindowContext},
    config::{ClipboardMode, ResolvedCaptureConfig},
    package::metadata::AttemptLayout,
    package::outcome::{LossEvidence, LossProducer, LossReason},
    protocol::{BoundedList, ClockOrigin, CoarseTime, Evidence, FocusGeneration, GeometryId, PhysicalPoint, PhysicalRect, QpcTick, RequestId, SessionTimeUs, UnavailableReason, WindowId, ipc::{ForegroundAtReceipt, ProcessInstance}},
};

use self::{
    events::{DISPLAY_CHANGE, Notice, Notifications, SETTINGS_CHANGE},
    identity::{ApprovedProcesses, MetadataScrubber, ProcessHandle, WindowProcess},
    queries::{Classification, PointerBackend, Queries},
    windows::{FocusSnapshot, WindowMetadata},
};
use crate::session::windows::Receipt;

use super::{ProbeError, clipboard, clock, display, elevated::{InputCertificate, Selector}, unavailable};

const WINDOW_REFRESH: Duration = Duration::from_millis(100);
const DISPLAY_REFRESH: Duration = Duration::from_secs(1);
const WINDOW_RATE_CACHE: usize = 64;
const FUTURE_NOTICE_LIMIT: usize = 1024;

/// File metadata is sampled by the startup coordinator before input capture.
/// This describes the approved file at startup, not a loaded process image.
pub(super) struct ExecutableVersions(Vec<(crate::config::ExecutableIdentity, Option<version::ExecutableVersion>)>);

impl ExecutableVersions {
    pub fn read(config: &ResolvedCaptureConfig) -> Self {
        let scrubber = MetadataScrubber::new();
        Self(config.profile().approved_identities().iter().map(|identity| {
            (identity.clone(), version::ExecutableVersion::read(identity.executable(), &scrubber))
        }).collect())
    }
}

#[derive(Clone, Copy)]
pub(super) struct Visit {
    pub generation: FocusGeneration,
    pub started_at: QpcTick,
    pub foreground: WindowProcess,
    pub native: Option<FocusSnapshot>,
    uia_blocked: Option<uia::ReadUnavailable>,
    elevated: Option<(VisitBinding, FocusAtReceipt)>,
}

impl Visit {
    pub fn is_elevated(self) -> bool { self.elevated.is_some() }

    pub fn elevated_binding(self) -> Option<VisitBinding> { self.elevated.map(|(binding, _)| binding) }

    pub fn receipt_proof(self) -> ForegroundAtReceipt {
        ForegroundAtReceipt {
            window: self.foreground.window,
            process: self.foreground.process,
            focus: self.elevated.map(|(_, focus)| focus).or_else(|| self.native.map(|native| native.focus)),
        }
    }

    pub fn clipboard_focus_is_current(self) -> bool {
        self.elevated.is_none() && self.uia_blocked.is_none() && WindowProcess::foreground() == Some(self.foreground)
            && FocusSnapshot::read(self.foreground).is_some_and(|native| native.focus.password_style != Some(true)
                && !native.hung && !native.active_fallback && self.native.is_some_and(|expected| native.focus == expected.focus))
    }

    pub fn elevated_clipboard_focus_is_current(self) -> bool {
        let Some((binding, focus)) = self.elevated else { return false; };
        focus.password_style == Some(false)
            && screen_fix_elevated::normal_capture_desktop(binding.foreground.thread)
            && WindowProcess::foreground() == Some(self.foreground)
            && FocusSnapshot::read(self.foreground).is_some_and(|native| native.matches_identity(focus)
                && native.focus.password_style != Some(true) && !native.hung && !native.active_fallback)
    }

}

#[derive(Clone, Copy)]
pub(super) enum ReceiptFocus {
    Pending(Visit),
    Classified(Visit, TextTarget, Option<QpcTick>),
    Historical(Visit),
    Unknown,
}

impl ReceiptFocus {
    pub fn target(self, origin: ClockOrigin, deadline: SessionTimeUs) -> TextTarget {
        match self {
            Self::Classified(_, TextTarget::Password, _) => TextTarget::Password,
            Self::Classified(_, target, Some(at)) if origin.time(at).is_ok_and(|at| at <= deadline) => target,
            _ => TextTarget::PossiblySensitive,
        }
    }

    pub fn generation(self) -> Option<FocusGeneration> {
        self.visit().map(|visit| visit.generation)
    }

    pub fn visit(self) -> Option<Visit> {
        match self {
            Self::Pending(visit) | Self::Classified(visit, _, _) | Self::Historical(visit) => Some(visit),
            Self::Unknown => None,
        }
    }
}

/// The visit timeline's attribution of a hook-only receipt (remediation §6.4).
#[derive(Clone, Copy)]
pub(super) enum HookReceipt {
    /// One visit was verified throughout the receipt's uncertainty interval.
    Covered(Visit),
    /// The focus changed inside the interval while the foreground stayed the
    /// same approved window; `before` is the focus generation that ended.
    FocusUncertain { before: FocusGeneration, after: Visit },
    /// No sweep has verified the timeline through the receipt yet.
    Pending,
    /// The foreground at the receipt is unknown or was not approved.
    Unknown,
}

pub(super) struct ConfirmedText {
    pub visit: Visit,
    pub revision: u64,
    pub source_read: Option<ReadBinding>,
    pub callback_at: QpcTick,
    pub observed_at: QpcTick,
    pub text: String,
    /// False for invalid UTF-16, written with U+FFFD as `status:"invalid"` (I7).
    pub valid: bool,
}

/// A context health condition for the context stream. An activity loss is
/// also an L-auxiliary fault report (§6.2 rows 14–15).
pub(super) struct ContextHealth {
    pub kind: crate::package::outcome::HealthKind,
    pub failure: crate::package::diagnostics::Failure,
    pub at: QpcTick,
    pub count: Option<u64>,
    pub activity_lost: bool,
}

/// An interval with an unapproved or unknown foreground (C5, D-3 A). Its input
/// is suppressed and counted; no identity is recorded.
pub(super) struct Suppression {
    pub start: SessionTimeUs,
    pub end: Option<SessionTimeUs>,
    pub reason: &'static str,
    pub input: u64,
}

/// Open and unwritten suppression intervals; a further one extends the last.
const SUPPRESSION_LIMIT: usize = 32;

/// Context losses waiting for the context journal, at most one per reason (C3).
#[derive(Default)]
struct Losses(Vec<(LossReason, u64, QpcTick, QpcTick)>);

impl Losses {
    fn note(&mut self, reason: LossReason, count: u64, first: QpcTick, last: QpcTick) {
        if count == 0 { return; }
        match self.0.iter_mut().find(|entry| entry.0 == reason) {
            Some(entry) => {
                entry.1 = entry.1.saturating_add(count);
                entry.2 = entry.2.min(first);
                entry.3 = entry.3.max(last);
            }
            None => self.0.push((reason, count, first, last)),
        }
    }
}

pub(super) struct Context<'a> {
    notifications: Notifications,
    losses: Losses,
    health: Vec<ContextHealth>,
    uia_lost: bool,
    suppressions: Vec<Suppression>,
    /// Windows whose location notices the rate limit dropped; each is re-read
    /// once at the trailing edge (C4).
    location_dirty: Vec<WindowId>,
    queries: Queries,
    elevated: elevated::Observations,
    ime: ime::Reader,
    last_ime_focus: Option<(FocusGeneration, ImeState)>,
    future_text: Vec<uia_commits::Commit>,
    clipboard: Option<clipboard::Worker>,
    cursor: cursor::Tracker,
    busy: busy::Busy,
    executable_versions: ExecutableVersions,
    visit_bounds: Option<(FocusGeneration, PhysicalRect, QpcTick)>,
    cursor_completions: Vec<cursor::Completion>,
    routes_since: Option<QpcTick>,
    routable: Vec<MonitorGeometry>,
    visit_geometry: Option<(FocusGeneration, GeometryId, QpcTick)>,
    clipboard_notices: VecDeque<clipboard::Notice>,
    future_clipboard: Vec<clipboard::Read>,
    approved_activity: VecDeque<(FocusGeneration, QpcTick)>,
    future_notices: Vec<Notice>,
    config: &'a ResolvedCaptureConfig,
    topology: &'a display::topology::Topology,
    topology_seen: Option<(QpcTick, u64)>,
    unavailable_routes: BTreeMap<crate::protocol::MonitorId, GeometryId>,
    reconfiguring_routes: BTreeMap<crate::protocol::MonitorId, GeometryId>,
    approvals: ApprovedProcesses<'a>,
    /// Foreground access per process; `Err(true)` is an ineligible token (D-8 A).
    ordinary_access: VecDeque<(ProcessInstance, Instant, Result<screen_fix_elevated::ForegroundAccess, bool>)>,
    scrubber: MetadataScrubber,
    generation: u64,
    current: Option<Visit>,
    /// Ended visits: each with the last sweep that verified it unchanged and
    /// the stamp at which its replacement was established.
    previous_visits: VecDeque<(Visit, QpcTick, QpcTick)>,
    /// The notice horizon of the latest window sweep. Every foreground or
    /// focus change before it has replaced the current visit.
    swept: Option<QpcTick>,
    focus_boundary: Option<QpcTick>,
    hung: hung::Hung,
    observed_foreground_window: WindowId,
    observed_foreground: Option<WindowProcess>,
    next_sweep: Instant,
    next_displays: Instant,
    display_rows: Option<Vec<Value>>,
    displays_unavailable: bool,
    window_rate: VecDeque<(WindowId, Instant)>,
    started: bool,
}

impl<'a> Context<'a> {
    pub fn start(layout: &AttemptLayout, config: &'a ResolvedCaptureConfig, topology: &'a display::topology::Topology,
        executable_versions: ExecutableVersions) -> Result<Self, ProbeError> {
        let now = Instant::now();
        let scrubber = MetadataScrubber::new();
        let queries = Queries::start(scrubber.clone())?;
        let clipboard = (config.profile().clipboard() == ClipboardMode::OptIn)
            .then(clipboard::Worker::start).transpose()?;
        Ok(Self {
            notifications: Notifications::start()?, losses: Losses::default(), health: Vec::new(), uia_lost: false,
            suppressions: Vec::new(), location_dirty: Vec::new(), queries, elevated: elevated::Observations::default(), ime: ime::Reader::spawn()?, last_ime_focus: None, future_text: Vec::with_capacity(4),
            clipboard, cursor: cursor::Tracker::start(layout)?, busy: busy::Busy::default(),
            executable_versions, visit_bounds: None,
            cursor_completions: Vec::with_capacity(16),
            routes_since: None, routable: Vec::new(), visit_geometry: None,
            clipboard_notices: VecDeque::with_capacity(8), future_clipboard: Vec::with_capacity(8),
            approved_activity: VecDeque::with_capacity(64),
            future_notices: Vec::with_capacity(FUTURE_NOTICE_LIMIT),
            config, topology, topology_seen: None, unavailable_routes: BTreeMap::new(), reconfiguring_routes: BTreeMap::new(),
            approvals: ApprovedProcesses::new(config.profile())?, ordinary_access: VecDeque::with_capacity(64), scrubber,
            generation: 0, current: None, previous_visits: VecDeque::with_capacity(32), swept: None, focus_boundary: None,
            hung: hung::Hung::new(now), observed_foreground_window: WindowId(0),
            observed_foreground: None, next_sweep: now, next_displays: now,
            display_rows: None, displays_unavailable: false, window_rate: VecDeque::with_capacity(WINDOW_RATE_CACHE),
            started: false,
        })
    }

    pub fn pause(&mut self, at: QpcTick) -> Result<(), ProbeError> {
        self.generation = self.generation.checked_add(1)
            .ok_or_else(|| unavailable("The focus generation counter overflowed"))?;
        self.focus_boundary = Some(at);
        self.current = None;
        self.previous_visits.clear();
        self.swept = None;
        // The final drain before the pause wrote every interval up to the cutoff.
        self.suppressions.clear();
        self.location_dirty.clear();
        self.ordinary_access.clear();
        self.queries.focus(FocusGeneration(self.generation), None, Instant::now())?;
        self.elevated = elevated::Observations::default();
        self.ime.focus_changed(None);
        self.last_ime_focus = None;
        self.future_text.clear();
        self.clipboard_notices.clear();
        self.future_clipboard.clear();
        self.approved_activity.clear();
        self.future_notices.clear();
        self.cursor_completions.clear();
        self.cursor.reset_observation();
        self.visit_geometry = None;
        self.visit_bounds = None;
        self.routable.clear();
        self.routes_since = None;
        self.display_rows = None;
        self.topology_seen = None;
        self.observed_foreground_window = WindowId(0);
        self.observed_foreground = None;
        self.busy = busy::Busy::default();
        self.started = false;
        Ok(())
    }

    pub fn close_hung_observation(&mut self) -> Vec<Value> {
        self.queries.stop_reads();
        self.hung.close()
    }

    pub fn discard_paused_notifications(&mut self) -> u64 {
        let notifications = self.notifications.drain().count() as u64;
        let lost = self.notifications.take_loss().map_or(0, |(count, _, _)| count);
        let _ = self.cursor.results();
        let clipboard = self.clipboard.as_ref().map_or(0, |worker| worker.notices().count() as u64);
        notifications.saturating_add(lost).saturating_add(clipboard)
    }

    pub fn resume(&mut self) {
        let now = Instant::now();
        self.next_sweep = now;
        self.next_displays = now;
    }

    pub fn visit(&self) -> Option<Visit> {
        self.current
    }

    pub fn activate_elevated(&mut self, binding: VisitBinding, focus: FocusAtReceipt, at: QpcTick) -> Result<Visit, ProbeError> {
        self.generation = self.generation.checked_add(1)
            .ok_or_else(|| unavailable("The focus generation counter overflowed"))?;
        let foreground = WindowProcess { window: binding.foreground.window,
            process: binding.foreground.process, thread: binding.foreground.thread };
        let visit = Visit { generation: FocusGeneration(self.generation), started_at: at, foreground,
            native: None, uia_blocked: None, elevated: Some((binding, focus)) };
        self.current = Some(visit);
        self.elevated = elevated::Observations::default();
        self.focus_boundary = Some(at);
        self.queries.focus(visit.generation, Some(visit), Instant::now())?;
        Ok(visit)
    }

    pub fn elevated_invalidation(
        &mut self, binding: VisitBinding, focus: FocusAtReceipt, after: QpcTick, receipt: Receipt,
    ) -> Result<Option<(QpcTick, bool)>, screen_fix_elevated::Failure> {
        if !self.notifications.running() || self.notifications.take_loss().is_some() {
            return Err(screen_fix_elevated::Failure::Witness);
        }
        let mut invalidation: Option<(QpcTick, bool)> = None;
        for notice in self.notifications.drain() {
            if notice.receipt < after {
                continue;
            }
            let target = notice.window == binding.foreground.window;
            let foreground = matches!(notice.event, EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_DESKTOPSWITCH | DISPLAY_CHANGE)
                || (target && notice.object == OBJID_WINDOW && matches!(notice.event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE));
            let focus_change = notice.event == EVENT_OBJECT_FOCUS && (notice.thread == 0
                || notice.thread == focus.thread.get() || notice.thread == binding.foreground.thread.get());
            if !foreground && !focus_change {
                if self.future_notices.len() < FUTURE_NOTICE_LIMIT && !target {
                    self.future_notices.push(notice);
                }
                continue;
            }
            let interval = receipt.tick_anchor.zip(notice.source_tick).and_then(|(anchor, tick)|
                clock::message_tick_interval(anchor, receipt.clock.qpc_frequency, CoarseTime::windows_message(tick)));
            if interval.is_some_and(|(_, latest)| latest < after) { continue; }
            let before = interval.map_or(after, |(lower, _)| lower.min(notice.receipt));
            invalidation = Some(invalidation.map_or((before, foreground), |(old, close)| (old.min(before), close || foreground)));
        }
        Ok(invalidation)
    }

    /// Identity resolution, UI Automation and clipboard health (§6.2 rows 14, 15, 17, 18).
    /// None of these conditions fails the context loop (C3).
    fn poll_health(&mut self, at: QpcTick) {
        use crate::package::{diagnostics::{Failure, FailureComponent}, outcome::HealthKind};
        let (unresolved, resolver_lost) = self.approvals.poll();
        if unresolved != 0 {
            self.health.push(ContextHealth { kind: HealthKind::IdentityUnresolved,
                failure: Failure::new(FailureComponent::Context, "Executable path canonicalization", None),
                at, count: Some(unresolved), activity_lost: false });
        }
        let activity_lost = |operation| ContextHealth { kind: HealthKind::AuxiliaryActivityLoss,
            failure: Failure::new(FailureComponent::Context, operation, None), at, count: None, activity_lost: true };
        if resolver_lost {
            self.health.push(activity_lost("Executable identity resolution"));
        }
        if !self.uia_lost && let Some(operation) = self.queries.uia_failure() {
            // One report: no replacement worker is started (ALG §7.1).
            self.uia_lost = true;
            self.health.push(activity_lost(operation));
        }
        if self.clipboard.as_ref().is_some_and(clipboard::Worker::exited) {
            // Row 17: reported once; no replacement worker, so no later
            // clipboard events are produced.
            self.clipboard = None;
            self.clipboard_notices.clear();
            self.health.push(activity_lost("Observe the opt-in clipboard"));
        }
        if self.cursor.take_owner_lost() {
            self.health.push(activity_lost("Publish cursor assets"));
        }
    }

    pub fn poll(&mut self, receipt: Receipt, at: QpcTick, frame_policy: &FramePolicy) -> Result<Vec<Value>, ProbeError> {
        self.poll_health(at);
        let start = receipt.boundary.qpc;
        let origin = receipt.clock;
        if !self.notifications.running() {
            return Err(unavailable("A context notification owner exited unexpectedly"));
        }
        let mut notices = Vec::new();
        let ring_loss = self.notifications.take_loss();
        let mut lost = ring_loss.map_or(0, |(count, _, _)| count);
        if let Some((count, first, last)) = ring_loss {
            self.losses.note(LossReason::NotificationOverflow, count, first, last);
        }
        let mut future = Vec::with_capacity(FUTURE_NOTICE_LIMIT);
        for notice in self.future_notices.drain(..).chain(self.notifications.drain()) {
            if notice.receipt < start {
                continue;
            }
            if notice.receipt <= at {
                notices.push(notice);
            } else if future.len() < FUTURE_NOTICE_LIMIT {
                future.push(notice);
            } else {
                lost = lost.saturating_add(1);
                self.losses.note(LossReason::NotificationOverflow, 1, notice.receipt, notice.receipt);
            }
        }
        self.future_notices = future;
        let mut output = Vec::new();
        let interrupted = lost != 0 || notices.iter().any(|notice| matches!(notice.event,
            EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_DESKTOPSWITCH));
        let foreground = unsafe { GetForegroundWindow() };
        let foreground_window = WindowId(foreground as usize as u64);
        let observed = WindowProcess::foreground().filter(|proof| proof.window == foreground_window);
        // IMG-8: a Ghost that verifiably replaced a hung window counts as that
        // window; an unresolved Ghost stays unapproved (C4).
        let replaced = observed.filter(|proof| hung::is_ghost(proof.hwnd().0))
            .and_then(|ghost| self.hung.ghost_original(ghost.window));
        let approved = replaced.or(observed).filter(|proof| !hung::is_ghost(proof.hwnd().0)
            && self.approvals.get(proof.process).is_some() && self.ordinary_process(proof.process));
        let observed_at = origin.time(clock::qpc()?).map_err(super::clock_error)?;
        frame_policy.publish(approved, observed_at, receipt.boundary, interrupted);
        let now = Instant::now();
        let mut invalidated = lost != 0 || self.queries.changed();
        let mut sweep = !self.started || invalidated || now >= self.next_sweep;
        let mut foreground_notice = false;
        let mut display_changed = now >= self.next_displays;
        for notice in &notices {
            match notice.event {
                DISPLAY_CHANGE => display_changed = true,
                SETTINGS_CHANGE => sweep = true,
                EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_DESKTOPSWITCH => {
                    sweep = true;
                    invalidated = true;
                    foreground_notice = true;
                }
                EVENT_OBJECT_FOCUS => {
                    // Ignore an unrelated process's focus event only if the
                    // native owner thread is known to differ.
                    if self.current.is_none_or(|visit| notice.thread == 0
                        || Some(notice.thread) == visit.native.map(|native| native.focus.thread.get())
                        || notice.thread == visit.foreground.thread.get())
                    {
                        sweep = true;
                        invalidated = true;
                    }
                }
                EVENT_OBJECT_IME_CHANGE | EVENT_OBJECT_IME_HIDE | EVENT_OBJECT_IME_SHOW => {
                    sweep = true;
                }
                _ => {}
            }
        }
        let approval_changed = self.current.map(|visit| visit.foreground) != approved;
        let foreground_changed = !self.started || approval_changed || foreground_window != self.observed_foreground_window
            || observed != self.observed_foreground || foreground_notice;
        self.observed_foreground_window = foreground_window;
        self.observed_foreground = observed;
        let hung_sampled = self.hung.poll(now, foreground_changed, foreground_window, &mut self.approvals, &mut output);
        if hung_sampled {
            self.queries.update_hung(&self.hung);
        }
        if display_changed {
            self.next_displays = now + DISPLAY_REFRESH;
        }
        self.refresh_displays(at, &mut output, display_changed);
        if sweep || foreground_changed || hung_sampled {
            self.next_sweep = now + WINDOW_REFRESH;
            // The desktop can change while the top-level sweep is running.
            // Recheck before the snapshot establishes a new ordinary visit.
            let current = WindowProcess::foreground();
            let verified = approved.filter(|proof| (current == Some(*proof) || replaced == Some(*proof) && current == observed)
                && !hung::is_ghost(proof.hwnd().0));
            if approved != verified {
                self.observed_foreground_window = WindowId(0);
            }
            let approved = verified;
            let native = approved.and_then(FocusSnapshot::read);
            let uia_blocked = approved.and_then(|proof| self.queries.blocked(proof.process));
            let old = self.current.map(|visit| (visit.foreground, visit.native.map(|native| (native.focus, native.hung)), visit.uia_blocked));
            let new = approved.map(|proof| (proof, native.map(|native| (native.focus, native.hung)), uia_blocked));
            let mut ended = None;
            if invalidated || old != new {
                self.generation = self.generation.checked_add(1)
                    .ok_or_else(|| unavailable("The focus generation counter overflowed"))?;
                let stamp = clock::qpc()?;
                self.focus_boundary = Some(stamp);
                ended = self.current.map(|known| self.verified(known));
                if let Some(known) = self.current {
                    if self.previous_visits.len() == 32 { self.previous_visits.pop_front(); }
                    self.previous_visits.push_back((known, self.verified(known), stamp));
                }
                self.current = approved.map(|foreground| Visit {
                    generation: FocusGeneration(self.generation), started_at: stamp, foreground, native, uia_blocked, elevated: None,
                });
                self.queries.focus(FocusGeneration(self.generation), self.current, now)?;
                self.ime.focus_changed(self.current);
                self.visit_geometry = None;
                self.visit_bounds = None;
                if let Some(visit) = self.current {
                    output.push(json!({
                        "kind": "keyboard_layout", "generation": visit.generation,
                        "layout": visit.native.map(|value| value.focus.keyboard_layout),
                    }));
                }
            } else if let Some(visit) = &mut self.current {
                visit.native = native;
            }
            if let Some(visit) = self.current && (foreground_changed || self.visit_geometry.is_none() || display_changed) {
                self.observe_visit_geometry(visit, at)?;
            }
            // C5: an unapproved or unknown foreground opens a suppression
            // interval at the last verification of the visit before it.
            let open = self.suppressions.last().is_some_and(|interval| interval.end.is_none());
            match self.current {
                None if !open => {
                    let start = origin.time(ended.unwrap_or(self.swept.unwrap_or(at))).map_err(super::clock_error)?;
                    let reason = match observed {
                        // D-8 A: an approved foreground whose token the source cannot support.
                        Some(proof) if self.approvals.get(proof.process).is_some()
                            && self.access(proof.process) == Err(true) => "ineligible_token",
                        // An approved elevated foreground without a collector (E4).
                        Some(proof) if self.approvals.get(proof.process).is_some()
                            && self.access(proof.process) == Ok(screen_fix_elevated::ForegroundAccess::Elevated) =>
                            "elevated_source_unavailable",
                        Some(_) => "non_approved_app",
                        None => "unknown_foreground",
                    };
                    if self.suppressions.len() == SUPPRESSION_LIMIT {
                        if let Some(last) = self.suppressions.last_mut() { last.end = None; }
                    } else {
                        self.suppressions.push(Suppression { start, end: None, reason, input: 0 });
                    }
                }
                Some(visit) if open => if let Some(last) = self.suppressions.last_mut() {
                    last.end = Some(origin.time(visit.started_at).map_err(super::clock_error)?.max(last.start));
                },
                _ => {}
            }
            // Notices through `at` were processed and the reads above followed it.
            self.swept = Some(at);
            if foreground_changed {
                let mut row = approved.and_then(|proof| self.window_row(proof, "foreground"))
                    .unwrap_or_else(|| json!({ "kind": "foreground", "approved": false }));
                if replaced.is_some() && replaced == approved {
                    row["ghost_replacement"] = json!(true);
                }
                // The foreground snapshot is obtained during this pass. A
                // missing or mismatched notice cannot supply its tick.
                if lost == 0
                    && let Some(proof) = observed
                    && let Some(notice) = notices.iter()
                        .filter(|notice| notice.event == EVENT_SYSTEM_FOREGROUND)
                        .max_by_key(|notice| notice.receipt)
                    && notice.window == proof.window
                    && (notice.thread == 0 || notice.thread == proof.thread.get())
                {
                    add_win_event_timing(&mut row, *notice, receipt)?;
                }
                output.push(row);
            }
        }
        let target = match self.queries.classification() {
            Classification::Resolved(target, _) => Some(target),
            Classification::Pending => None,
        };
        let process_hung = self.current.is_some_and(|visit| self.hung.contains(visit.foreground.process));
        if let Some(change) = self.busy.refresh(self.current, process_hung, target, &notices, false, at, origin) {
            output.push(json!({ "kind": "app_busy", "approved": change.is_some(), "hint": change }));
        }
        let cursor_notified = notices.iter().any(|notice| notice.event == EVENT_OBJECT_NAMECHANGE
            && notice.object == OBJID_CURSOR as i32);
        if let Some(visit) = self.current
            && matches!(self.queries.classification(), Classification::Resolved(TextTarget::Editable, Some(_)))
            && let Some(focus) = visit.native
            && let Some(notice) = notices.iter().filter(|notice| {
                notice.object == OBJID_CARET as i32 && notice.receipt >= visit.started_at
                    && notice.thread == focus.focus.thread.get()
                    && (Some(notice.window) == focus.caret_window || notice.window == focus.focus.window)
                    && matches!(notice.event, EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE | EVENT_OBJECT_LOCATIONCHANGE)
            }).max_by_key(|notice| notice.receipt)
            && self.confirm_focus(visit)
            && let Some(native) = FocusSnapshot::read(visit.foreground)
        {
            let visible = notice.event != EVENT_OBJECT_HIDE && native.caret.is_some();
            let change = match notice.event {
                EVENT_OBJECT_SHOW => "show",
                EVENT_OBJECT_HIDE => "hide",
                _ => "location",
            };
            let mut row = json!({ "kind": "caret", "generation": visit.generation, "change": change,
                "visible": visible, "bounds": visible.then_some(native.caret).flatten() });
            add_win_event_timing(&mut row, *notice, receipt)?;
            output.push(row);
        }
        for notice in notices {
            let hint = match notice.event {
                EVENT_SYSTEM_MINIMIZESTART => Some("minimize_start"),
                EVENT_SYSTEM_MINIMIZEEND => Some("minimize_end"),
                EVENT_SYSTEM_SWITCHSTART => Some("switch_start"),
                EVENT_SYSTEM_SWITCHEND => Some("switch_end"),
                EVENT_OBJECT_IME_SHOW => Some("ime_show"),
                EVENT_OBJECT_IME_HIDE => Some("ime_hide"),
                EVENT_OBJECT_IME_CHANGE => Some("ime_change"),
                _ => None,
            };
            if let Some(hint) = hint {
                // Typed hint records name a window only when it is approved (C4).
                let approved = read_window(notice.window).filter(|proof| self.approvals.get(proof.process).is_some());
                let mut row = json!({ "kind": "window_hint", "hint": hint, "approved": approved.is_some(),
                    "window": approved.map(|proof| proof.window) });
                add_win_event_timing(&mut row, notice, receipt)?;
                output.push(row);
                continue;
            }
            if notice.object != OBJID_WINDOW as i32 || notice.window.0 == 0 {
                continue;
            }
            let kind = match notice.event {
                EVENT_OBJECT_NAMECHANGE => "window_title",
                EVENT_OBJECT_LOCATIONCHANGE => "window_location",
                EVENT_OBJECT_SHOW => "window_shown",
                EVENT_OBJECT_HIDE => "window_hidden",
                EVENT_OBJECT_DESTROY => {
                    self.window_rate.retain(|(window, _)| *window != notice.window);
                    continue;
                }
                _ => continue,
            };
            if kind == "window_location" && self.location_limited(notice.window, now) {
                if !self.location_dirty.contains(&notice.window) && self.location_dirty.len() < WINDOW_RATE_CACHE {
                    self.location_dirty.push(notice.window);
                }
                continue;
            }
            // The notification callback keeps only top-level windows (CTX-1).
            let Some(proof) = read_window(notice.window) else { continue };
            if notice.thread != 0 && proof.thread.get() != notice.thread {
                continue;
            }
            if let Some(mut row) = self.window_row(proof, kind) {
                if kind == "window_location" && let Some(visit) = self.current.filter(|visit| visit.foreground == proof) {
                    self.observe_visit_geometry(visit, at)?;
                }
                add_win_event_timing(&mut row, notice, receipt)?;
                row["source_event_tick_ms"] = json!(notice.source_tick);
                if kind == "window_location" {
                    self.rate_location(notice.window, now);
                    self.location_dirty.retain(|window| *window != notice.window);
                }
                output.push(row);
            }
        }
        // Trailing edge of the location rate limit: one re-read per dropped burst.
        let due: Vec<_> = self.location_dirty.iter().copied().filter(|dirty| !self.location_limited(*dirty, now)).collect();
        self.location_dirty.retain(|window| !due.contains(window));
        for window in due {
            let Some(proof) = read_window(window) else { continue };
            if let Some(mut row) = self.window_row(proof, "window_location") {
                if let Some(visit) = self.current.filter(|visit| visit.foreground == proof) {
                    self.observe_visit_geometry(visit, at)?;
                }
                row["trailing"] = json!(true);
                self.rate_location(window, now);
                output.push(row);
            }
        }
        if let Some(point) = self.queries.pointer_rest(now) && let Some(geometry) = self.point_geometry(point) {
            self.pointer_query(point, geometry, origin, at, None)?;
        }
        self.sample_cursor(cursor_notified);
        self.ime.poll();
        let extras = queries::FocusExtras {
            cursor: self.cursor.current_hint(),
            ime: self.current.and_then(|visit| self.ime.focus_state(visit.generation)),
        };
        self.queries.poll(now, at, origin, Some(&extras), &mut output);
        let mut changes = std::mem::take(&mut self.cursor_completions);
        changes.extend(self.cursor.results());
        for change in changes {
            if change.received() <= at {
                output.push(change.row(origin));
            } else if self.cursor_completions.len() < 16 {
                self.cursor_completions.push(change);
            } else {
                self.losses.note(LossReason::StagingPressure, 1, change.received(), change.received());
            }
        }
        if let Some(visit) = self.current && let Some(state) = self.ime.focus_state(visit.generation)
            && self.last_ime_focus != Some((visit.generation, state))
        {
            self.last_ime_focus = Some((visit.generation, state));
            output.push(json!({ "kind": "ime_state", "generation": visit.generation, "ime": state }));
        }
        self.started = true;
        Ok(output)
    }

    fn location_limited(&self, window: WindowId, now: Instant) -> bool {
        self.window_rate.iter().any(|(id, last)| *id == window && now.saturating_duration_since(*last) < WINDOW_REFRESH)
    }

    /// Record a location read of `window` for the rate limit.
    fn rate_location(&mut self, window: WindowId, now: Instant) {
        self.window_rate.retain(|(id, _)| *id != window);
        if self.window_rate.len() == WINDOW_RATE_CACHE {
            self.window_rate.pop_front();
        }
        self.window_rate.push_back((window, now));
    }

    /// A context loss found outside this owner, such as elevated evidence the
    /// Recorder could not stage; it is admitted with the owner's own losses.
    pub fn note_loss(&mut self, reason: LossReason, count: u64, first: QpcTick, last: QpcTick) {
        self.losses.note(reason, count, first, last);
    }

    /// Context losses since the last call, clipped to the admission through `at` (C3).
    pub fn take_losses(&mut self, receipt: Receipt, at: QpcTick) -> Vec<LossEvidence> {
        std::mem::take(&mut self.losses.0).into_iter().filter_map(|(reason, count, first, last)| {
            let last = last.min(at);
            if last < receipt.boundary.qpc { return None; }
            let first = receipt.clock.time(first.max(receipt.boundary.qpc).min(last)).ok()?;
            let last = receipt.clock.time(last).ok()?;
            Some(LossEvidence::ordinary(reason, None, std::num::NonZeroU64::new(count)?, first, last))
        }).collect()
    }

    /// Count one suppressed input record at `at` into its suppression interval (C5).
    pub fn count_suppressed(&mut self, at: SessionTimeUs) {
        if let Some(interval) = self.suppressions.iter_mut().rev()
            .find(|interval| interval.start <= at && interval.end.is_none_or(|end| at < end))
        {
            interval.input = interval.input.saturating_add(1);
        }
    }

    /// Suppression intervals that ended at or before `ready`. A closing
    /// boundary at `close` ends the open interval and takes every interval.
    pub fn take_suppressions(
        &mut self, ready: Option<SessionTimeUs>, close: Option<SessionTimeUs>,
    ) -> Vec<Suppression> {
        if let Some(close) = close && let Some(last) = self.suppressions.last_mut() && last.end.is_none() {
            last.end = Some(close.max(last.start));
        }
        let due = self.suppressions.iter().take_while(|interval| interval.end
            .is_some_and(|end| close.is_some() || ready.is_some_and(|ready| end <= ready))).count();
        self.suppressions.drain(..due).collect()
    }

    /// Context health conditions since the last call (C3).
    pub fn take_health(&mut self) -> Vec<ContextHealth> {
        std::mem::take(&mut self.health)
    }

    pub fn note_approved_input(&mut self, generation: FocusGeneration, at: QpcTick) {
        self.busy.activity(generation, at);
        if self.clipboard.is_none() {
            return;
        }
        if self.approved_activity.len() == 64 {
            self.approved_activity.pop_front();
        }
        self.approved_activity.push_back((generation, at));
    }

    pub fn input_settings(&self) -> InputSettings {
        settings::read()
    }

    pub fn note_held_buttons(&mut self, held: bool) {
        self.busy.buttons(held);
    }

    pub fn fixture_save_proof(&mut self, executable: &crate::config::ExecutableIdentity, at: QpcTick)
        -> Option<(ForegroundAtReceipt, Option<VisitBinding>)>
    {
        let visit = self.current.filter(|visit| visit.started_at <= at)?;
        if self.queries.changed() || WindowProcess::foreground() != Some(visit.foreground)
            || visit.is_elevated() && (!self.has_certified_elevated_input(visit, at) || !self.confirm_focus(visit))
            || self.approvals.get_at(visit.foreground.process, at)? != executable
        {
            return None;
        }
        Some((visit.receipt_proof(), visit.elevated.map(|(binding, _)| binding)))
    }

    pub fn save_geometry(&self, monitor: crate::protocol::MonitorId, at: QpcTick) -> Option<GeometryId> {
        if !self.routes_since.is_some_and(|since| since <= at) { return None; }
        self.routable.iter().find(|geometry| geometry.id.monitor == monitor).map(|geometry| geometry.id)
    }

    pub fn checkpoint_context(&self, at: QpcTick, source: Option<VisitBinding>) -> Option<SaveWindowContext> {
        let visit = self.current.filter(|visit| visit.started_at <= at)?;
        if visit.elevated.map(|(binding, _)| binding) != source
            || self.queries.changed() || WindowProcess::foreground() != Some(visit.foreground)
            || visit.is_elevated() && (!self.has_certified_elevated_input(visit, at) || !self.confirm_focus(visit))
        {
            return None;
        }
        let geometry = self.visit_bounds.filter(|(generation, _, observed)| {
            *generation == visit.generation && *observed <= at && self.routes_since.is_some_and(|since| since <= at)
        });
        let geometries = geometry.map_or_else(Vec::new, |(_, bounds, _)| {
            self.routable.iter().filter(|monitor| monitor.bounds.intersects(bounds)).map(|monitor| monitor.id).collect()
        });
        let busy = self.busy.current().filter(|hint| hint.generation == visit.generation)
            .map_or(BusyLevel::Clear, |hint| hint.level);
        Some(SaveWindowContext { window: visit.foreground.window, focus: visit.generation,
            geometries: BoundedList::new(geometries).ok()?, busy })
    }

    pub fn poll_clipboard(&mut self, receipt: Receipt, now: QpcTick) -> Vec<Value> {
        let Some(worker) = &self.clipboard else { return Vec::new() };
        take_clipboard_notices(worker, &mut self.clipboard_notices, &mut self.losses);
        let safe_visit = self.current.filter(|visit| self.queries.clipboard_safe() && visit.clipboard_focus_is_current());
        if let Some(visit) = safe_visit {
            let mut pending = VecDeque::with_capacity(8);
            for notice in self.clipboard_notices.drain(..) {
                if notice.observed.elapsed() >= clipboard::MAX_AGE || notice.foreground != visit.foreground
                    || notice.received < visit.started_at || notice.received < receipt.boundary.qpc
                {
                    continue;
                }
                if notice.received > now {
                    pending.push_back(notice);
                    continue;
                }
                if !clipboard_change_after_visit(notice, visit, receipt) {
                    continue;
                }
                if notice.owner.is_none() {
                    if !clipboard_activity(&self.approved_activity, notice, visit, receipt.clock) {
                        pending.push_back(notice);
                        continue;
                    }
                }
                if !worker.grant(notice, visit, self.queries.clipboard_guard()) {
                    self.losses.note(LossReason::ClipboardQueue, 1, notice.received, notice.received);
                }
            }
            self.clipboard_notices = pending;
        } else {
            self.clipboard_notices.retain(|notice| notice.observed.elapsed() < clipboard::MAX_AGE
                && self.current.is_some_and(|visit| visit.foreground == notice.foreground
                    && notice.received >= visit.started_at));
        }
        let mut results = std::mem::take(&mut self.future_clipboard);
        results.extend(worker.results());
        let mut output = Vec::new();
        for read in results {
            if read.completed > now {
                if self.future_clipboard.len() < 8 {
                    self.future_clipboard.push(read);
                } else {
                    self.losses.note(LossReason::ClipboardQueue, 1, read.completed, read.completed);
                }
                continue;
            }
            if safe_visit.is_some_and(|visit| visit.generation == read.visit.generation
                && visit.foreground == read.notice.foreground)
            {
                output.push(clipboard_row(read, receipt.clock));
            }
        }
        output
    }

    pub fn approve_receipt(
        &mut self, proof: ForegroundAtReceipt, native_receipt: QpcTick, first: QpcTick,
    ) -> Option<ReceiptFocus> {
        let original = native_receipt.min(first);
        if let Some(visit) = self.current.filter(|visit| visit.elevated.is_some()) {
            if visit.receipt_proof() != proof || original < visit.started_at
                || !self.has_certified_elevated_input(visit, native_receipt.max(first))
            {
                return None;
            }
            if proof.focus.is_some_and(|focus| focus.password_style == Some(true)) {
                return Some(ReceiptFocus::Classified(visit, TextTarget::Password, None));
            }
            return Some(match self.queries.classification() {
                Classification::Pending => ReceiptFocus::Pending(visit),
                Classification::Resolved(target, completed) => ReceiptFocus::Classified(visit, target, completed),
            });
        }
        self.approvals.get_at(proof.process, original)?;
        if !self.ordinary_process(proof.process) { return None; }
        let Some(visit) = self.current.filter(|visit| visit.receipt_proof() == proof && visit.started_at <= original
            && !self.queries.changed()) else {
            let historical = self.previous_visits.iter().rev().find(|(visit, _, ended)| {
                visit.receipt_proof() == proof && visit.started_at <= original && native_receipt.max(first) < *ended
            });
            return Some(historical.map_or(ReceiptFocus::Unknown, |(visit, _, _)| ReceiptFocus::Historical(*visit)));
        };
        if proof.focus.is_some_and(|focus| focus.password_style == Some(true)) {
            return Some(ReceiptFocus::Classified(visit, TextTarget::Password, None));
        }
        Some(match self.queries.classification() {
            Classification::Pending => ReceiptFocus::Pending(visit),
            Classification::Resolved(target, completed) => ReceiptFocus::Classified(visit, target, completed),
        })
    }

    /// Confirmed TextEdit commits, and typed input losses for every commit
    /// that could not be confirmed inside the admission (I7). Commits come
    /// only from editable, non-password fields, so the losses are ordinary.
    pub fn take_confirmed_text(&mut self, receipt: Receipt, now: QpcTick) -> (Vec<ConfirmedText>, Vec<LossEvidence>) {
        use screen_fix_native_observation::uia_commits::CommitLossReason;
        let visit = self.current;
        if visit.is_some_and(|visit| visit.elevated.is_some()) {
            return (self.take_elevated_text(now), Vec::new());
        }
        let mut lost = Vec::new();
        let mut lose = |reason, count, first: QpcTick, last: QpcTick| {
            if last < receipt.boundary.qpc { return; }
            if let (Ok(first), Ok(last), Some(count)) = (receipt.clock.time(first.max(receipt.boundary.qpc)),
                receipt.clock.time(last), std::num::NonZeroU64::new(count))
            {
                lost.push(LossEvidence::ordinary(reason, Some(LossProducer::TextEdit), count, first, last));
            }
        };
        for loss in self.queries.commit_losses() {
            let reason = match loss.reason {
                CommitLossReason::Capacity => LossReason::CommitCapacity,
                CommitLossReason::TooLong => LossReason::CommitTooLong,
                CommitLossReason::Parts => LossReason::CommitParts,
                CommitLossReason::Rejected => LossReason::CommitRejected,
                CommitLossReason::Stale => LossReason::LateCommit,
            };
            lose(reason, loss.count, loss.first, loss.last);
        }
        let mut commits = std::mem::take(&mut self.future_text);
        commits.extend(self.queries.commits());
        let mut confirmed = Vec::new();
        for commit in commits {
            let Some(visit) = visit.filter(|visit| visit.generation == commit.generation) else {
                lose(LossReason::LateCommit, 1, commit.receipt, commit.receipt);
                continue;
            };
            if commit.receipt > now {
                if self.future_text.len() < 4 {
                    self.future_text.push(commit);
                } else {
                    lose(LossReason::CommitCapacity, 1, commit.receipt, commit.receipt);
                }
                continue;
            }
            if commit.receipt < visit.started_at || !self.queries.accepts_commit(commit.generation, commit.revision)
                || !self.confirm_focus(visit)
            {
                lose(LossReason::LateCommit, 1, commit.receipt, commit.receipt);
                continue;
            }
            confirmed.push(ConfirmedText {
                visit, revision: commit.revision, source_read: None, callback_at: commit.receipt, observed_at: now,
                text: commit.text, valid: commit.valid,
            });
        }
        (confirmed, lost)
    }

    pub fn confirm_text(&self, text: &ConfirmedText) -> bool {
        self.current.is_some_and(|visit| visit.generation == text.visit.generation)
            && text.source_read.map_or_else(
                || self.queries.accepts_commit(text.visit.generation, text.revision),
                |read| self.queries.accepts_remote_commit(read, text.callback_at),
            ) && self.confirm_focus(text.visit)
    }

    pub fn ime_for_key(
        &mut self, visit: Visit, key: RequestId, original_qpc: QpcTick, max_wait: Duration,
    ) -> Option<ImeState> {
        if !self.confirm_focus(visit) {
            return Some(ImeState::Unknown);
        }
        if visit.elevated.is_some() {
            self.elevated_ime_for_key(key, original_qpc, max_wait)
        } else {
            self.ime.key_state(visit, key)
        }
    }

    pub fn confirm_focus(&self, visit: Visit) -> bool {
        if let Some((binding, focus)) = visit.elevated {
            return self.current.is_some_and(|current| current.elevated == Some((binding, focus)))
                && WindowProcess::foreground() == Some(visit.foreground)
                && FocusSnapshot::read(visit.foreground).is_some_and(|native| native.matches_identity(focus)
                    && native.focus.password_style.is_none_or(|local| focus.password_style == Some(local)));
        }
        !self.queries.changed() && self.queries.blocked(visit.foreground.process).is_none()
            && WindowProcess::foreground() == Some(visit.foreground)
            && FocusSnapshot::read(visit.foreground).is_some_and(|native|
                !native.hung && visit.native.is_some_and(|expected| native.focus == expected.focus))
            && !self.queries.changed()
    }

    pub fn focus_end(&self, generation: FocusGeneration, clock: ClockOrigin) -> Option<SessionTimeUs> {
        let (_, _, ended) = self.previous_visits.iter().find(|(visit, _, _)| visit.generation == generation)?;
        clock.time(*ended).ok()
    }

    /// The last stamp at which a sweep confirmed `visit` unchanged; no
    /// earlier than its start. An elevated visit is never swept.
    fn verified(&self, visit: Visit) -> QpcTick {
        self.swept.filter(|_| !visit.is_elevated()).map_or(visit.started_at, |swept| swept.max(visit.started_at))
    }

    /// Attribute a hook-only receipt `at` (remediation §6.4). A visit covers
    /// it when the visit started before the receipt's uncertainty interval
    /// `[at − 16 ms, at]` and a sweep verified it unchanged through `at`.
    pub fn hook_receipt(&self, at: QpcTick, clock: ClockOrigin) -> HookReceipt {
        let uncertainty = i64::try_from(clock.qpc_frequency.0.get() / 1_000 * 16).unwrap_or(i64::MAX);
        let lower = QpcTick(at.0.saturating_sub(uncertainty));
        if let Some(visit) = self.current.filter(|visit| visit.is_elevated()) {
            // Elevated records reach the matcher only inside the certified visit.
            return if visit.started_at <= lower { HookReceipt::Covered(visit) } else { HookReceipt::Unknown };
        }
        if self.swept.is_none_or(|swept| swept < at) {
            return HookReceipt::Pending;
        }
        let current = self.current.map(|visit| (visit, self.verified(visit), None));
        let mut before = None;
        let mut after = None;
        for (visit, verified, ended) in self.previous_visits.iter()
            .map(|(visit, verified, ended)| (*visit, *verified, Some(*ended))).chain(current)
        {
            if visit.started_at <= lower {
                if at <= verified {
                    return HookReceipt::Covered(visit);
                }
                before = Some((visit, ended));
            } else if after.is_none() {
                after = Some(visit);
            }
        }
        match (before, after) {
            // The replacement was established in the same sweep, so no other
            // foreground came between the two visits.
            (Some((before, Some(ended))), Some(after)) if ended == after.started_at && before.foreground == after.foreground =>
                HookReceipt::FocusUncertain { before: before.generation, after },
            _ => HookReceipt::Unknown,
        }
    }

    /// Drop ended visits that can no longer attribute an unpublished receipt:
    /// they ended an uncertainty interval before the published input prefix.
    /// The visit of the active key focus keeps its end for redaction.
    pub fn prune_visits(&mut self, published: SessionTimeUs, clock: ClockOrigin, keep: Option<FocusGeneration>) {
        self.previous_visits.retain(|(visit, _, ended)| Some(visit.generation) == keep
            || clock.time(*ended).is_ok_and(|ended| ended.0.saturating_add(16_000) >= published.0));
    }

    pub fn focus_boundary(&self) -> Option<QpcTick> {
        self.focus_boundary
    }

    pub fn is_current(&self, proof: ForegroundAtReceipt, generation: Option<FocusGeneration>) -> bool {
        self.current.is_some_and(|visit| visit.receipt_proof() == proof && generation == Some(visit.generation))
    }

    pub fn finish_cursor(&mut self, cutoff: QpcTick, origin: ClockOrigin) -> Vec<Value> {
        let mut completions = std::mem::take(&mut self.cursor_completions);
        completions.extend(self.cursor.close());
        completions.into_iter().filter(|completion| completion.received() <= cutoff)
            .map(|completion| completion.row(origin)).collect()
    }

    /// Stop the cursor asset owner; every asset it published, unless it failed.
    pub fn cursor_inventory(&mut self) -> Option<Vec<crate::package::metadata::ImageFile>> {
        self.cursor.finish()
    }

    pub fn sample_cursor(&mut self, force: bool) {
        let visit = self.current;
        let selected = &self.routable;
        let approvals = &mut self.approvals;
        if let Some(change) = self.cursor.sample(force, |point| {
            visit.is_some() && selected.iter().any(|geometry| geometry.bounds.contains(point))
                && uia::window_at(point).is_some_and(|window| approvals.get(window.process).is_some())
        }) {
            if self.cursor_completions.len() < 16 {
                self.cursor_completions.push(change);
            } else {
                self.losses.note(LossReason::StagingPressure, 1, change.received(), change.received());
            }
        }
    }

    pub fn pointer_geometry_at(&self, point: PhysicalPoint, at: QpcTick) -> Evidence<GeometryId> {
        if !self.routes_since.is_some_and(|since| since <= at) {
            return Evidence::Unavailable(UnavailableReason::NotObserved);
        }
        self.point_geometry(point).map_or(Evidence::Unavailable(UnavailableReason::NotObserved), Evidence::Observed)
    }

    pub fn window_geometry_at(&self, proof: ForegroundAtReceipt, at: QpcTick) -> Evidence<GeometryId> {
        let Some((generation, geometry, observed)) = self.visit_geometry else {
            return Evidence::Unavailable(UnavailableReason::NotObserved);
        };
        if observed <= at && self.current.is_some_and(|visit| visit.generation == generation
            && visit.started_at <= at && visit.receipt_proof() == proof)
        {
            Evidence::Observed(geometry)
        } else {
            Evidence::Unavailable(UnavailableReason::NotObserved)
        }
    }

    fn observe_visit_geometry(&mut self, visit: Visit, lower_bound: QpcTick) -> Result<(), ProbeError> {
        let bounds = WindowMetadata::read(visit.foreground, &self.scrubber).and_then(|snapshot| snapshot.bounds);
        let measured = clock::qpc()?.max(lower_bound);
        self.observe_bounds(visit, bounds, measured);
        Ok(())
    }

    fn observe_bounds(&mut self, visit: Visit, bounds: Option<PhysicalRect>, measured: QpcTick) {
        if !self.visit_bounds.is_some_and(|(generation, known, _)| generation == visit.generation && Some(known) == bounds) {
            self.visit_bounds = bounds.map(|bounds| (visit.generation, bounds, measured));
        }
        if let Some(geometry) = bounds.and_then(|bounds| self.only_geometry(bounds)) {
            if !self.visit_geometry.is_some_and(|(generation, id, _)| generation == visit.generation && id == geometry) {
                self.visit_geometry = Some((visit.generation, geometry, measured));
            }
        } else {
            self.visit_geometry = None;
        }
    }

    pub fn pointer_query(
        &mut self, point: PhysicalPoint, geometry: GeometryId, origin: ClockOrigin, at: QpcTick, action_at: Option<QpcTick>,
    ) -> Result<Option<PointerLink>, ProbeError> {
        if self.point_geometry(point) != Some(geometry) { return Ok(None); }
        let Some(visit) = self.current else { return Ok(None); };
        let Some(window) = uia::window_at(point).filter(|window| self.approvals.get(window.process).is_some()) else {
            self.queries.deny_pointer();
            return Ok(None);
        };
        let Some(root) = uia::window_root(window) else { return Ok(None); };
        let backend = if self.ordinary_process(window.process) {
            PointerBackend::Ordinary
        } else if visit.elevated.is_some() {
            if window == visit.foreground {
                PointerBackend::Elevated(None)
            } else {
                self.secondary_claim(window).map_or(PointerBackend::Unavailable,
                    |claim| PointerBackend::Elevated(Some(claim)))
            }
        } else {
            PointerBackend::Unavailable
        };
        self.queries.pointer(point, geometry, visit, window, root, backend, origin, at, action_at)
    }

    fn point_geometry(&self, point: PhysicalPoint) -> Option<GeometryId> {
        let mut captures = self.routable.iter().filter(|capture| capture.bounds.contains(point));
        let geometry = captures.next()?.id;
        captures.next().is_none().then_some(geometry)
    }

    fn window_row(&mut self, proof: WindowProcess, kind: &'static str) -> Option<Value> {
        if hung::is_ghost(proof.hwnd().0) { return None; }
        let identity = self.approvals.get(proof.process)?;
        if !self.ordinary_process(proof.process) { return None; }
        // An identity change during the read gets one re-read; an approved
        // window whose metadata still cannot be read stays approved (C4).
        let Some(window) = WindowMetadata::read(proof, &self.scrubber).or_else(|| WindowMetadata::read(proof, &self.scrubber)) else {
            return Some(json!({ "kind": kind, "approved": true, "window": proof.window, "pid": proof.process.pid,
                "metadata": "unavailable" }));
        };
        Some(self.window_evidence_row(proof, identity, kind, window))
    }

    /// A window row with the PDF foreground fields (D-1): `pid`, `exe`, `rect`,
    /// `monitor`, `product`, `version`, `version_string`, `title` and `class`.
    fn window_evidence_row(
        &self, proof: WindowProcess, identity: &crate::config::ExecutableIdentity,
        kind: &'static str, window: WindowMetadata,
    ) -> Value {
        let version = self.executable_versions.0.iter().find(|(executable, _)| executable == identity)
            .and_then(|(_, version)| version.as_ref());
        let geometry = window.bounds.and_then(|bounds| self.only_geometry(bounds));
        json!({
            "kind": kind, "approved": true, "window": proof.window, "pid": proof.process.pid,
            "exe": self.scrubber.scrub(identity.executable(), 4096),
            "product": version.and_then(|version| version.product.as_ref()),
            "version": version.and_then(|version| version.version.as_ref()),
            "version_string": version.and_then(|version| version.version_string.as_ref()),
            "version_observation": "approved_file_at_startup",
            "rect": window.bounds, "monitor": geometry.map(|geometry| geometry.monitor), "geometry": geometry,
            "dpi": window.dpi, "dpi_awareness": window.dpi_awareness,
            "class": window.class, "title": window.title,
            "metadata_observed_at_read": true,
        })
    }

    pub fn ordinary_process(&mut self, proof: ProcessInstance) -> bool {
        self.foreground_access(proof) == Some(screen_fix_elevated::ForegroundAccess::Ordinary)
    }

    /// The Recorder's access to a receipt-time foreground process. `None`
    /// when the process cannot be classified (it exited, or its image or
    /// token could not be read).
    pub fn foreground_access(&mut self, proof: ProcessInstance) -> Option<screen_fix_elevated::ForegroundAccess> {
        self.access(proof).ok()
    }

    /// `Err(true)` when the token rules out every source for this release:
    /// another user, session or logon, a protected or SYSTEM process, an
    /// unsupported integrity or split-token pair (D-8 A). `Err(false)` when
    /// the process cannot be classified.
    fn access(&mut self, proof: ProcessInstance) -> Result<screen_fix_elevated::ForegroundAccess, bool> {
        use screen_fix_elevated::{Failure, foreground_access};
        use crate::protocol::{TokenIdentityCheck as Check, TokenSubject as Subject};
        let now = Instant::now();
        self.ordinary_access.retain(|(_, expires, _)| *expires > now);
        if let Some((_, _, access)) = self.ordinary_access.iter().find(|(process, _, _)| *process == proof) {
            return *access;
        }
        let access = ProcessHandle::open(proof.pid).filter(|process| process.instance(proof.pid) == Some(proof))
            .and_then(|process| process.image_path())
            .ok_or(false)
            .and_then(|image| foreground_access(proof, &image).map_err(|failure| matches!(failure,
                Failure::TokenIdentity(identity) if matches!(identity.subject, Subject::Foreground | Subject::ForegroundLinkedToken)
                    && matches!(identity.check, Check::ProtectedProcess | Check::DifferentSession | Check::DifferentUser
                        | Check::DifferentLogon | Check::SystemAccount | Check::UnsupportedIntegrity
                        | Check::UnsupportedSplitPair | Check::LinkedLogonMismatch))));
        if self.ordinary_access.len() == 64 { self.ordinary_access.pop_front(); }
        self.ordinary_access.push_back((proof, now + Duration::from_millis(250), access));
        access
    }

    fn only_geometry(&self, bounds: PhysicalRect) -> Option<GeometryId> {
        let mut candidates = self.routable.iter().filter(|monitor| monitor.bounds.intersects(bounds));
        let geometry = candidates.next()?.id;
        candidates.next().is_none().then_some(geometry)
    }

    fn refresh_displays(&mut self, at: QpcTick, rows: &mut Vec<Value>, forced: bool) {
        let Some(snapshot) = self.topology.snapshot() else { return };
        let revision = self.topology.revision();
        let observed = (snapshot.received, revision);
        if !forced && self.topology_seen == Some(observed) { return; }
        let Some(states) = self.topology.routes() else { return };
        self.topology_seen = Some(observed);
        use display::topology::Route;
        for (&monitor, route) in &states {
            match route {
                Route::Missing(geometry) => {
                    self.reconfiguring_routes.remove(&monitor);
                    if self.unavailable_routes.insert(monitor, *geometry) != Some(*geometry) {
                        rows.push(json!({ "kind": "monitor_missing", "monitor": monitor, "geometry": geometry }));
                    }
                }
                Route::Active(capture) => {
                    self.reconfiguring_routes.remove(&monitor);
                    if self.unavailable_routes.remove(&monitor).is_some() {
                        rows.push(json!({ "kind": "monitor_returned", "monitor": monitor, "geometry": capture.geometry.id }));
                    }
                }
                Route::Reconfiguring(geometry) => {
                    if self.reconfiguring_routes.insert(monitor, *geometry) != Some(*geometry)
                        && !self.unavailable_routes.contains_key(&monitor)
                    {
                        rows.push(json!({ "kind": "display_reconfiguring", "monitor": monitor, "geometry": geometry }));
                    }
                }
            }
        }
        if let Some(monitors) = &snapshot.outputs {
            let mut routes = Vec::new();
            let displays: Vec<_> = monitors.iter().map(|monitor| {
                let selected = self.config.monitors().iter().find(|entry| entry.key == monitor.key);
                let geometry = selected.and_then(|entry| states.get(&entry.id).and_then(Route::capture))
                    .filter(|capture| {
                        capture.geometry.bounds == monitor.bounds && capture.geometry.dpi_x == monitor.dpi_x
                            && capture.geometry.dpi_y == monitor.dpi_y && capture.rotation == monitor.rotation
                            && capture.hdr == monitor.hdr && capture.sdr_white_level == monitor.sdr_white
                            && capture.refresh_numerator == monitor.refresh.map(|refresh| refresh.0)
                            && capture.refresh_denominator == monitor.refresh.map(|refresh| refresh.1)
                    }).map(|capture| capture.geometry);
                routes.extend(geometry);
                json!({
                    "display_key": monitor.key.as_str(), "selected_monitor": selected.map(|entry| entry.id),
                    "geometry": geometry.map(|value| value.id), "bounds": monitor.bounds,
                    "dpi_x": monitor.dpi_x, "dpi_y": monitor.dpi_y, "refresh": monitor.refresh,
                    "rotation": monitor.rotation, "hdr": monitor.hdr, "primary": monitor.primary,
                    "unavailable": &monitor.unavailable,
                })
            }).collect();
            if self.displays_unavailable || self.display_rows.as_ref() != Some(&displays) {
                rows.push(json!({ "kind": "display", "available": true, "monitors": &displays }));
                self.display_rows = Some(displays);
            }
            if self.routable != routes || self.routes_since.is_none() {
                self.routable = routes;
                self.routes_since = clock::qpc().ok().map(|sample| sample.max(at));
                self.visit_geometry = None;
                self.visit_bounds = None;
            }
            self.displays_unavailable = false;
        } else if !self.displays_unavailable {
            self.displays_unavailable = true;
            self.routable.clear();
            self.routes_since = None;
            self.visit_geometry = None;
            self.visit_bounds = None;
            rows.push(json!({ "kind": "display", "available": false }));
        }
    }

}

fn add_win_event_timing(row: &mut Value, notice: Notice, receipt: Receipt) -> Result<(), ProbeError> {
    let Some(tick) = notice.source_tick else { return Ok(()); };
    let received = receipt.clock.time(notice.receipt).map_err(super::clock_error)?;
    let coarse = CoarseTime::windows_message(tick);
    let late = clock::message_tick_estimate(receipt.clock, receipt.tick_anchor, coarse)
        .map(|estimate| estimate.is_late(received));
    // The observation uses the poll receipt. The callback receipt remains
    // independently available, including when notifications arrive out of order.
    row["win_event_recv_t_us"] = json!(received);
    row["tick_ms"] = json!(coarse.tick_ms);
    row["tick_uncertainty_us"] = json!(coarse.uncertainty_us);
    row["late"] = json!(late);
    Ok(())
}

fn clipboard_activity(
    activity: &VecDeque<(FocusGeneration, QpcTick)>, notice: clipboard::Notice, visit: Visit, clock: ClockOrigin,
) -> bool {
    let received = clock.time(notice.received).ok();
    activity.iter().rev().any(|(generation, at)| {
        *generation == visit.generation && *at <= notice.received
            && received.zip(clock.time(*at).ok())
                .is_some_and(|(change, input)| change.0.saturating_sub(input.0) <= 1_000_000)
    })
}

/// Queue the clipboard owner's notices, at most 8; every notice it or this
/// queue could not hold is a `clipboard_queue` loss (C3).
fn take_clipboard_notices(worker: &clipboard::Worker, notices: &mut VecDeque<clipboard::Notice>, losses: &mut Losses) {
    if let Some((count, first, last)) = worker.take_loss() {
        losses.note(LossReason::ClipboardQueue, count, first, last);
    }
    for notice in worker.notices() {
        if notices.len() < 8 {
            notices.push_back(notice);
        } else {
            losses.note(LossReason::ClipboardQueue, 1, notice.received, notice.received);
        }
    }
}

fn read_window(window: WindowId) -> Option<WindowProcess> {
    WindowProcess::read(::windows::Win32::Foundation::HWND(window.0 as usize as *mut _))
}

fn clipboard_row(read: clipboard::Read, clock: ClockOrigin) -> Value {
    json!({
        "kind": "clipboard_change", "approved": true,
        "reason": read.text.is_none().then_some("too_large"), "text": read.text,
        "sequence": read.notice.sequence, "owner_verified": read.notice.owner.is_some(),
        "notification_receipt_t_us": clock.time(read.notice.received).ok(),
        "completed_t_us": clock.time(read.completed).ok(),
    })
}

fn clipboard_change_after_visit(notice: clipboard::Notice, visit: Visit, receipt: Receipt) -> bool {
    let Some(anchor) = receipt.tick_anchor else { return false };
    let Ok(estimate) = anchor.coarse_estimate(receipt.clock, notice.source_tick) else { return false };
    let Ok(started) = receipt.clock.time(visit.started_at) else { return false };
    let coarse = CoarseTime::windows_message(notice.source_tick);
    let frequency = i128::from(receipt.clock.qpc_frequency.0.get());
    let bracket = (i128::from(anchor.qpc_bracket_ticks) * 1_000_000 + frequency - 1) / frequency;
    i128::from(estimate.0) - i128::from(coarse.uncertainty_us.0) - bracket >= i128::from(started.0)
}

