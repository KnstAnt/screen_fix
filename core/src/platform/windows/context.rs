mod busy;
mod clipboard_staging;
mod cursor;
mod events;
mod elevated;
mod frame_policy;
pub(super) use frame_policy::{ApprovalReadError, ELEVATED_FRAME_WAIT_US, FrameApproval, FramePolicy};
mod hung;
mod ime;
pub(super) use ime::State as ImeState;
pub(super) mod identity;
mod location;
mod queries;
pub(super) use queries::PointerLink;
mod routes;
mod timeline;
mod uia;
pub(super) use uia::FocusGuard;
mod uia_commits;
mod settings;
mod version;
mod windows;

use std::{collections::VecDeque, time::{Duration, Instant}};

use serde_json::{Value, json};
use screen_fix_native_observation::LossSpan;
use screen_fix_protocol::ipc::{FocusAtReceipt, elevated::{ReadBinding, VisitBinding}};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_FOCUS, EVENT_OBJECT_HIDE, EVENT_OBJECT_IME_CHANGE,
    EVENT_OBJECT_IME_HIDE, EVENT_OBJECT_IME_SHOW, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_SWITCHEND, EVENT_SYSTEM_SWITCHSTART, GetForegroundWindow, OBJID_CARET, OBJID_CURSOR, OBJID_WINDOW,
};

use crate::{
    capture::{context::{BusyLevel, InputSettings, TextTarget}, save::SaveWindowContext},
    config::ResolvedCaptureConfig,
    package::metadata::AttemptLayout,
    package::outcome::{LossEvidence, LossProducer, LossReason},
    package::row::MessageTick,
    protocol::{BoundedList, ClockOrigin, CoarseTime, Evidence, FocusGeneration, GeometryId, MonitorId, PhysicalPoint, QpcTick, RequestId, SessionTimeUs, WindowId, ipc::{ForegroundAtReceipt, ProcessInstance}},
};

use self::{
    clipboard_staging::ClipboardStaging,
    events::{DISPLAY_CHANGE, Notice, Notifications, SETTINGS_CHANGE},
    identity::{ApprovedProcesses, MetadataScrubber, ProcessHandle, WindowProcess},
    location::LocationLimiter,
    queries::{Classification, PointerBackend, Queries},
    routes::DisplayRoutes,
    timeline::VisitTimeline,
    windows::{FocusSnapshot, WindowMetadata},
};
use crate::platform::windows::controller::Receipt;

use super::{ProbeError, clock, display, unavailable};

const WINDOW_REFRESH: Duration = Duration::from_millis(100);
const FUTURE_NOTICE_LIMIT: usize = 1024;
/// How long a classified foreground access stays cached. The selector
/// re-classifies the foreground at the same period.
pub(super) const ACCESS_RECHECK: Duration = Duration::from_millis(250);
const ACCESS_CACHE_LIMIT: usize = 64;
const CURSOR_STAGING_LIMIT: usize = 16;
const FUTURE_COMMIT_LIMIT: usize = 4;

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

/// What ended an open elevated visit, from its earliest possible stamp.
#[derive(Clone, Copy)]
pub(super) enum Invalidation {
    /// The foreground, the desktop or the displays may have changed, or a
    /// notice loss may have hidden such a change: the visit closes.
    Foreground(QpcTick),
    /// Only the focus changed: the visit's input is suspended.
    Focus(QpcTick),
}

impl Invalidation {
    fn new(before: QpcTick, foreground: bool) -> Self {
        if foreground { Self::Foreground(before) } else { Self::Focus(before) }
    }

    fn before(self) -> QpcTick {
        match self {
            Self::Foreground(before) | Self::Focus(before) => before,
        }
    }

    /// The earlier stamp of both; a foreground change outweighs a focus change.
    fn merge(self, other: Self) -> Self {
        let foreground = matches!(self, Self::Foreground(_)) || matches!(other, Self::Foreground(_));
        Self::new(self.before().min(other.before()), foreground)
    }
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
    pub reason: SuppressionReason,
    pub input: u64,
}

/// Why the input of a suppression interval was not recorded (C5).
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SuppressionReason {
    /// An approved foreground whose token the source cannot support (D-8 A).
    IneligibleToken,
    /// An approved elevated foreground without a collector (E4).
    ElevatedSourceUnavailable,
    NonApprovedApp,
    UnknownForeground,
}

/// Why the Recorder's access to a foreground process is not known.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AccessUnknown {
    /// The token rules out every source for this release: another user,
    /// session or logon, a protected or SYSTEM process, an unsupported
    /// integrity or split-token pair (D-8 A).
    Ineligible,
    /// The process cannot be classified: it exited, or its image or token
    /// could not be read.
    Unclassified,
}

/// Open and unwritten suppression intervals; a further one extends the last.
const SUPPRESSION_LIMIT: usize = 32;

/// Context losses waiting for the context journal, at most one per reason (C3).
#[derive(Default)]
struct Losses(Vec<(LossReason, LossSpan)>);

impl Losses {
    fn note(&mut self, reason: LossReason, span: LossSpan) {
        if span.count == 0 { return; }
        match self.0.iter_mut().find(|(noted, _)| *noted == reason) {
            Some((_, noted)) => noted.merge(span),
            None => self.0.push((reason, span)),
        }
    }
}

/// The foreground read at the start of a context poll.
struct ForegroundRead {
    window: WindowId,
    /// The foreground window's identity, when it matches `window`.
    observed: Option<WindowProcess>,
    /// The hung window a verified Ghost replaced (IMG-8).
    replaced: Option<WindowProcess>,
    /// The approved, ordinary foreground, if any.
    approved: Option<WindowProcess>,
}

/// What one context poll found before its window sweep.
struct PollFacts {
    foreground: ForegroundRead,
    invalidated: bool,
    foreground_changed: bool,
    display_changed: bool,
}

/// What the new notices of a context poll require.
#[derive(Default)]
struct NoticeScan {
    sweep: bool,
    invalidated: bool,
    foreground: bool,
    display: bool,
}

pub(super) struct Context<'a> {
    notifications: Notifications,
    losses: Losses,
    /// Elevated TextEdit losses waiting to become typed input losses (I7).
    text_losses: Losses,
    health: Vec<ContextHealth>,
    uia_lost: bool,
    suppressions: Vec<Suppression>,
    location: LocationLimiter,
    queries: Queries,
    elevated: elevated::Observations,
    ime: ime::Reader,
    last_ime_focus: Option<(FocusGeneration, ImeState)>,
    future_text: Vec<uia_commits::Commit>,
    clipboard: ClipboardStaging,
    cursor: cursor::Tracker,
    busy: busy::Busy,
    executable_versions: ExecutableVersions,
    cursor_completions: Vec<cursor::Completion>,
    routes: DisplayRoutes<'a>,
    future_notices: Vec<Notice>,
    approvals: ApprovedProcesses<'a>,
    /// Foreground access per process, until its expiry.
    ordinary_access: VecDeque<(ProcessInstance, Instant, Result<screen_fix_elevated::ForegroundAccess, AccessUnknown>)>,
    scrubber: MetadataScrubber,
    timeline: VisitTimeline,
    hung: hung::Hung,
    observed_foreground_window: WindowId,
    observed_foreground: Option<WindowProcess>,
    next_sweep: Instant,
    started: bool,
}

impl<'a> Context<'a> {
    pub fn start(layout: &AttemptLayout, config: &'a ResolvedCaptureConfig, topology: &'a display::topology::Topology,
        executable_versions: ExecutableVersions) -> Result<Self, ProbeError> {
        let now = Instant::now();
        let scrubber = MetadataScrubber::new();
        let queries = Queries::start(scrubber.clone())?;
        let clipboard = ClipboardStaging::start(config)?;
        Ok(Self {
            notifications: Notifications::start()?, losses: Losses::default(), text_losses: Losses::default(),
            health: Vec::new(), uia_lost: false,
            suppressions: Vec::new(), location: LocationLimiter::new(), queries, elevated: elevated::Observations::default(), ime: ime::Reader::spawn()?, last_ime_focus: None,
            future_text: Vec::with_capacity(FUTURE_COMMIT_LIMIT),
            clipboard, cursor: cursor::Tracker::start(layout)?, busy: busy::Busy::default(),
            executable_versions,
            cursor_completions: Vec::with_capacity(CURSOR_STAGING_LIMIT),
            routes: DisplayRoutes::new(config, topology, now),
            future_notices: Vec::with_capacity(FUTURE_NOTICE_LIMIT),
            approvals: ApprovedProcesses::new(config.profile())?, ordinary_access: VecDeque::with_capacity(ACCESS_CACHE_LIMIT), scrubber,
            timeline: VisitTimeline::new(),
            hung: hung::Hung::new(now), observed_foreground_window: WindowId(0),
            observed_foreground: None, next_sweep: now,
            started: false,
        })
    }

    pub fn pause(&mut self, at: QpcTick) -> Result<(), ProbeError> {
        let generation = self.timeline.reset(at)?;
        // The final drain before the pause wrote every interval up to the cutoff.
        self.suppressions.clear();
        self.location.reset();
        self.ordinary_access.clear();
        self.queries.focus(generation, None, Instant::now())?;
        self.elevated = elevated::Observations::default();
        self.ime.focus_changed(None);
        self.last_ime_focus = None;
        self.future_text.clear();
        self.clipboard.reset();
        self.future_notices.clear();
        self.cursor_completions.clear();
        self.cursor.reset_observation();
        self.routes.reset();
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
        let lost = self.notifications.take_loss().map_or(0, |span| span.count);
        let _ = self.cursor.results();
        let clipboard = self.clipboard.discard_notices();
        notifications.saturating_add(lost).saturating_add(clipboard)
    }

    /// Drain the notice ring while an elevated epoch has no open visit, so
    /// that a stale overflow does not reach the next visit (D1). No visit
    /// observes these notices; a ring loss is still counted (C3).
    pub fn discard_unbound_notifications(&mut self) {
        self.notifications.drain().for_each(|_| ());
        if let Some(span) = self.notifications.take_loss() {
            self.losses.note(LossReason::NotificationOverflow, span);
        }
    }

    pub fn resume(&mut self) {
        let now = Instant::now();
        self.next_sweep = now;
        self.routes.resume(now);
    }

    pub fn visit(&self) -> Option<Visit> {
        self.timeline.current()
    }

    pub fn activate_elevated(&mut self, binding: VisitBinding, focus: FocusAtReceipt, at: QpcTick) -> Result<Visit, ProbeError> {
        let generation = self.timeline.next_generation()?;
        let foreground = binding.foreground.into();
        let visit = Visit { generation, started_at: at, foreground,
            native: None, uia_blocked: None, elevated: Some((binding, focus)) };
        self.timeline.activate(visit);
        self.elevated = elevated::Observations::default();
        self.queries.focus(visit.generation, Some(visit), Instant::now())?;
        Ok(visit)
    }

    pub fn elevated_invalidation(
        &mut self, binding: VisitBinding, focus: FocusAtReceipt, after: QpcTick, receipt: Receipt,
    ) -> Result<Option<Invalidation>, screen_fix_elevated::Failure> {
        if !self.notifications.running() {
            return Err(screen_fix_elevated::Failure::Witness);
        }
        let mut invalidation: Option<Invalidation> = None;
        if let Some(span) = self.notifications.take_loss() {
            self.losses.note(LossReason::NotificationOverflow, span);
            // A loss that ended before the visit opened hid none of its
            // notices. A later loss may have hidden a foreground change, so
            // the visit is revoked from its first possible notice (D1).
            if span.last >= after {
                invalidation = Some(Invalidation::Foreground(span.first.max(after)));
            }
        }
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
                // The collector reports the target's own notices.
                if !target {
                    defer_notice(&mut self.future_notices, &mut self.losses, notice);
                }
                continue;
            }
            let interval = notice_interval(notice, receipt);
            if interval.is_some_and(|(_, latest)| latest < after) { continue; }
            let before = interval.map_or(after, |(lower, _)| lower.min(notice.receipt));
            let found = Invalidation::new(before, foreground);
            invalidation = Some(invalidation.map_or(found, |known| known.merge(found)));
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
        if self.clipboard.take_exited() {
            self.health.push(activity_lost("Observe the opt-in clipboard"));
        }
        if self.cursor.take_owner_lost() {
            self.health.push(activity_lost("Publish cursor assets"));
        }
    }

    pub fn poll(&mut self, receipt: Receipt, at: QpcTick, frame_policy: &FramePolicy) -> Result<Vec<Value>, ProbeError> {
        self.poll_health(at);
        let origin = receipt.clock;
        if !self.notifications.running() {
            return Err(unavailable("A context notification owner exited unexpectedly"));
        }
        let (notices, lost) = self.take_notices(receipt.boundary.qpc, at);
        let mut output = Vec::new();
        let interrupted = lost != 0 || notices.iter().any(|notice| matches!(notice.event,
            EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_DESKTOPSWITCH));
        let foreground = self.read_foreground();
        let observed_at = origin.time(clock::qpc()?)?;
        frame_policy.publish(foreground.approved, observed_at, receipt.boundary, interrupted);
        let now = Instant::now();
        let scan = self.scan_notices(&notices);
        let invalidated = lost != 0 || self.queries.changed() || scan.invalidated;
        let sweep = !self.started || invalidated || now >= self.next_sweep || scan.sweep;
        let display_changed = self.routes.due(now, scan.display);
        let approval_changed = self.timeline.current().map(|visit| visit.foreground) != foreground.approved;
        let foreground_changed = !self.started || approval_changed || foreground.window != self.observed_foreground_window
            || foreground.observed != self.observed_foreground || scan.foreground;
        self.observed_foreground_window = foreground.window;
        self.observed_foreground = foreground.observed;
        let hung_sampled = self.hung.poll(now, foreground_changed, foreground.window, &mut self.approvals, &mut output);
        if hung_sampled {
            self.queries.update_hung(&self.hung);
        }
        self.routes.refresh(at, &mut output, display_changed);
        if sweep || foreground_changed || hung_sampled {
            let facts = PollFacts { foreground, invalidated, foreground_changed, display_changed };
            let approved = self.sweep_foreground(&facts, now, at, origin, &mut output)?;
            if foreground_changed {
                let row = self.foreground_row(approved, &facts.foreground, &notices, lost, receipt)?;
                output.push(row);
            }
        }
        let process_hung = self.timeline.current().is_some_and(|visit| self.hung.contains(visit.foreground.process));
        output.extend(self.busy_row(process_hung, &notices, false, at, origin));
        output.extend(self.caret_change(&notices, receipt)?);
        let cursor_notified = cursor_notified(&notices);
        self.notice_rows(notices, now, at, receipt, &mut output)?;
        self.trailing_locations(now, at, &mut output)?;
        if let Some(point) = self.queries.pointer_rest(now) && let Some(geometry) = self.routes.point_geometry(point) {
            self.pointer_query(point, geometry, origin, at, None)?;
        }
        self.sample_cursor(cursor_notified);
        self.ime.poll();
        let extras = queries::FocusExtras {
            cursor: self.cursor.current_hint(),
            ime: self.timeline.current().and_then(|visit| self.ime.focus_state(visit.generation)),
        };
        self.queries.poll(now, at, origin, Some(&extras), &mut output);
        self.cursor_rows(at, origin, &mut output);
        if let Some(visit) = self.timeline.current() && let Some(state) = self.ime.focus_state(visit.generation)
            && self.last_ime_focus != Some((visit.generation, state))
        {
            self.last_ime_focus = Some((visit.generation, state));
            output.push(json!({ "kind": "ime_state", "generation": visit.generation, "ime": state }));
        }
        self.started = true;
        Ok(output)
    }

    /// The deferred and new notices from the admission start through `at`,
    /// and the number lost. A later notice is deferred (C3).
    fn take_notices(&mut self, start: QpcTick, at: QpcTick) -> (Vec<Notice>, u64) {
        let mut notices = Vec::new();
        let ring_loss = self.notifications.take_loss();
        let mut lost = ring_loss.map_or(0, |span| span.count);
        if let Some(span) = ring_loss {
            self.losses.note(LossReason::NotificationOverflow, span);
        }
        let mut future = Vec::with_capacity(FUTURE_NOTICE_LIMIT);
        for notice in self.future_notices.drain(..).chain(self.notifications.drain()) {
            if notice.receipt < start {
                continue;
            }
            if notice.receipt <= at {
                notices.push(notice);
            } else if !defer_notice(&mut future, &mut self.losses, notice) {
                lost = lost.saturating_add(1);
            }
        }
        self.future_notices = future;
        (notices, lost)
    }

    fn read_foreground(&mut self) -> ForegroundRead {
        let foreground = unsafe { GetForegroundWindow() };
        let window = WindowId(foreground as usize as u64);
        let observed = WindowProcess::foreground().filter(|proof| proof.window == window);
        // IMG-8: a Ghost that verifiably replaced a hung window counts as that
        // window; an unresolved Ghost stays unapproved (C4).
        let replaced = observed.filter(|proof| hung::is_ghost(proof.hwnd().0))
            .and_then(|ghost| self.hung.ghost_original(ghost.window));
        let approved = replaced.or(observed).filter(|proof| !hung::is_ghost(proof.hwnd().0)
            && self.approvals.get(proof.process).is_some() && self.ordinary_process(proof.process));
        ForegroundRead { window, observed, replaced, approved }
    }

    fn scan_notices(&self, notices: &[Notice]) -> NoticeScan {
        let mut scan = NoticeScan::default();
        for notice in notices {
            match notice.event {
                DISPLAY_CHANGE => scan.display = true,
                SETTINGS_CHANGE => scan.sweep = true,
                EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_DESKTOPSWITCH => {
                    scan.sweep = true;
                    scan.invalidated = true;
                    scan.foreground = true;
                }
                EVENT_OBJECT_FOCUS => {
                    // Ignore an unrelated process's focus event only if the
                    // native owner thread is known to differ.
                    if self.timeline.current().is_none_or(|visit| notice.thread == 0
                        || Some(notice.thread) == visit.native.map(|native| native.focus.thread.get())
                        || notice.thread == visit.foreground.thread.get())
                    {
                        scan.sweep = true;
                        scan.invalidated = true;
                    }
                }
                EVENT_OBJECT_IME_CHANGE | EVENT_OBJECT_IME_HIDE | EVENT_OBJECT_IME_SHOW => {
                    scan.sweep = true;
                }
                _ => {}
            }
        }
        scan
    }

    /// Verify the foreground, replace the visit when it changed and update
    /// the suppression intervals. Returns the verified approved foreground.
    fn sweep_foreground(
        &mut self, facts: &PollFacts, now: Instant, at: QpcTick, origin: ClockOrigin, output: &mut Vec<Value>,
    ) -> Result<Option<WindowProcess>, ProbeError> {
        let ForegroundRead { observed, replaced, approved, .. } = facts.foreground;
        self.next_sweep = now + WINDOW_REFRESH;
        // The desktop can change while the top-level sweep is running.
        // Recheck before the snapshot establishes a new ordinary visit.
        let current = WindowProcess::foreground();
        let verified = approved.filter(|proof| (current == Some(*proof) || replaced == Some(*proof) && current == observed)
            && !hung::is_ghost(proof.hwnd().0));
        if approved != verified {
            self.observed_foreground_window = WindowId(0);
        }
        let ended = self.replace_visit(verified, facts.invalidated, now, output)?;
        if let Some(visit) = self.timeline.current()
            && (facts.foreground_changed || !self.routes.has_visit_geometry() || facts.display_changed)
        {
            self.observe_visit_geometry(visit, at)?;
        }
        self.update_suppression(observed, ended, at, origin)?;
        // Notices through `at` were processed and the reads above followed it.
        self.timeline.mark_swept(at);
        Ok(verified)
    }

    /// Start a new visit when the verified foreground, its focus or its
    /// readability changed. Returns the last verification of the ended visit.
    fn replace_visit(
        &mut self, approved: Option<WindowProcess>, invalidated: bool, now: Instant, output: &mut Vec<Value>,
    ) -> Result<Option<QpcTick>, ProbeError> {
        let native = approved.and_then(FocusSnapshot::read);
        let uia_blocked = approved.and_then(|proof| self.queries.blocked(proof.process));
        let old = self.timeline.current()
            .map(|visit| (visit.foreground, visit.native.map(|native| (native.focus, native.hung)), visit.uia_blocked));
        let new = approved.map(|proof| (proof, native.map(|native| (native.focus, native.hung)), uia_blocked));
        if !invalidated && old == new {
            if let Some(visit) = self.timeline.current_mut() {
                visit.native = native;
            }
            return Ok(None);
        }
        let generation = self.timeline.next_generation()?;
        let stamp = clock::qpc()?;
        let ended = self.timeline.replace(approved.map(|foreground| Visit {
            generation, started_at: stamp, foreground, native, uia_blocked, elevated: None,
        }), stamp);
        let current = self.timeline.current();
        self.queries.focus(generation, current, now)?;
        self.ime.focus_changed(current);
        self.routes.forget_visit();
        if let Some(visit) = current {
            output.push(json!({
                "kind": "keyboard_layout", "generation": visit.generation,
                "layout": visit.native.map(|value| value.focus.keyboard_layout),
            }));
        }
        Ok(ended)
    }

    /// C5: an unapproved or unknown foreground opens a suppression interval
    /// at the last verification of the visit before it; a new visit ends it.
    fn update_suppression(
        &mut self, observed: Option<WindowProcess>, ended: Option<QpcTick>, at: QpcTick, origin: ClockOrigin,
    ) -> Result<(), ProbeError> {
        let open = self.suppressions.last().is_some_and(|interval| interval.end.is_none());
        match self.timeline.current() {
            None if !open => {
                let start = origin.time(ended.unwrap_or(self.timeline.swept().unwrap_or(at)))?;
                let reason = self.suppression_reason(observed);
                if self.suppressions.len() == SUPPRESSION_LIMIT {
                    if let Some(last) = self.suppressions.last_mut() { last.end = None; }
                } else {
                    self.suppressions.push(Suppression { start, end: None, reason, input: 0 });
                }
            }
            Some(visit) if open => if let Some(last) = self.suppressions.last_mut() {
                last.end = Some(origin.time(visit.started_at)?.max(last.start));
            },
            _ => {}
        }
        Ok(())
    }

    fn suppression_reason(&mut self, observed: Option<WindowProcess>) -> SuppressionReason {
        let Some(proof) = observed else { return SuppressionReason::UnknownForeground; };
        if self.approvals.get(proof.process).is_none() {
            return SuppressionReason::NonApprovedApp;
        }
        match self.access(proof.process) {
            Err(AccessUnknown::Ineligible) => SuppressionReason::IneligibleToken,
            Ok(screen_fix_elevated::ForegroundAccess::Elevated) => SuppressionReason::ElevatedSourceUnavailable,
            _ => SuppressionReason::NonApprovedApp,
        }
    }

    fn foreground_row(
        &mut self, approved: Option<WindowProcess>, foreground: &ForegroundRead, notices: &[Notice], lost: u64,
        receipt: Receipt,
    ) -> Result<Value, ProbeError> {
        let mut row = approved.and_then(|proof| self.window_row(proof, WindowRowKind::Foreground))
            .unwrap_or_else(|| json!({ "kind": WindowRowKind::Foreground, "approved": false }));
        if foreground.replaced.is_some() && foreground.replaced == approved {
            row["ghost_replacement"] = json!(true);
        }
        // The foreground snapshot is obtained during this pass. A
        // missing or mismatched notice cannot supply its tick.
        if lost == 0
            && let Some(proof) = foreground.observed
            && let Some(notice) = notices.iter()
                .filter(|notice| notice.event == EVENT_SYSTEM_FOREGROUND)
                .max_by_key(|notice| notice.receipt)
            && notice.window == proof.window
            && (notice.thread == 0 || notice.thread == proof.thread.get())
        {
            add_win_event_timing(&mut row, *notice, receipt)?;
        }
        Ok(row)
    }

    /// The current classification target; `None` while it is pending.
    fn classified_target(&self) -> Option<TextTarget> {
        match self.queries.classification() {
            Classification::Resolved(target, _) => Some(target),
            Classification::Pending => None,
        }
    }

    /// An `app_busy` row when the busy hint of the current visit changed.
    fn busy_row(
        &mut self, process_hung: bool, notices: &[Notice], scoped: bool, at: QpcTick, origin: ClockOrigin,
    ) -> Option<Value> {
        let target = self.classified_target();
        self.busy.refresh(self.timeline.current(), process_hung, target, notices, scoped, at, origin)
            .map(|change| json!({ "kind": "app_busy", "approved": change.is_some(), "hint": change }))
    }

    /// A `caret` row for the latest caret notice of the current editable focus.
    fn caret_change(&self, notices: &[Notice], receipt: Receipt) -> Result<Option<Value>, ProbeError> {
        if let Some(visit) = self.timeline.current()
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
            return caret_row(visit, *notice, native, receipt).map(Some);
        }
        Ok(None)
    }

    /// Window hint and window rows for the notices of this poll.
    fn notice_rows(
        &mut self, notices: Vec<Notice>, now: Instant, at: QpcTick, receipt: Receipt, output: &mut Vec<Value>,
    ) -> Result<(), ProbeError> {
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
            if notice.object != OBJID_WINDOW || notice.window.0 == 0 {
                continue;
            }
            if notice.event == EVENT_OBJECT_DESTROY {
                self.location.forget(notice.window);
                continue;
            }
            let Some(kind) = window_event_kind(notice.event) else { continue };
            if kind == WindowRowKind::WindowLocation && self.location.defer(notice.window, now) {
                continue;
            }
            // The notification callback keeps only top-level windows (CTX-1).
            let Some(proof) = read_window(notice.window) else { continue };
            if notice.thread != 0 && proof.thread.get() != notice.thread {
                continue;
            }
            if let Some(mut row) = self.window_row(proof, kind) {
                if kind == WindowRowKind::WindowLocation
                    && let Some(visit) = self.timeline.current().filter(|visit| visit.foreground == proof)
                {
                    self.observe_visit_geometry(visit, at)?;
                }
                add_win_event_timing(&mut row, notice, receipt)?;
                row["source_event_tick_ms"] = json!(notice.source_tick);
                if kind == WindowRowKind::WindowLocation {
                    self.location.rate(notice.window, now);
                }
                output.push(row);
            }
        }
        Ok(())
    }

    /// Trailing edge of the location rate limit: one re-read per dropped burst (C4).
    fn trailing_locations(&mut self, now: Instant, at: QpcTick, output: &mut Vec<Value>) -> Result<(), ProbeError> {
        for window in self.location.due(now) {
            let Some(proof) = read_window(window) else { continue };
            if let Some(mut row) = self.window_row(proof, WindowRowKind::WindowLocation) {
                if let Some(visit) = self.timeline.current().filter(|visit| visit.foreground == proof) {
                    self.observe_visit_geometry(visit, at)?;
                }
                row["trailing"] = json!(true);
                self.location.rate(window, now);
                output.push(row);
            }
        }
        Ok(())
    }

    /// Cursor changes received through `at` as rows; a later one stays staged.
    fn cursor_rows(&mut self, at: QpcTick, origin: ClockOrigin, rows: &mut Vec<Value>) {
        let mut changes = std::mem::take(&mut self.cursor_completions);
        changes.extend(self.cursor.results());
        for change in changes {
            if change.received() <= at {
                rows.push(change.row(origin));
            } else {
                self.stage_cursor(change);
            }
        }
    }

    /// Stage a cursor change; one beyond the bound is a counted loss (C3).
    fn stage_cursor(&mut self, change: cursor::Completion) {
        if self.cursor_completions.len() < CURSOR_STAGING_LIMIT {
            self.cursor_completions.push(change);
        } else {
            self.losses.note(LossReason::StagingPressure, LossSpan::at(change.received()));
        }
    }

    /// A context loss found outside this owner, such as elevated evidence the
    /// Recorder could not stage; it is admitted with the owner's own losses.
    pub fn note_loss(&mut self, reason: LossReason, span: LossSpan) {
        self.losses.note(reason, span);
    }

    /// Context losses since the last call, clipped to the admission through `at` (C3).
    pub fn take_losses(&mut self, receipt: Receipt, at: QpcTick) -> Vec<LossEvidence> {
        std::mem::take(&mut self.losses.0).into_iter().filter_map(|(reason, span)| {
            let last = span.last.min(at);
            if last < receipt.boundary.qpc { return None; }
            let first = receipt.clock.time(span.first.max(receipt.boundary.qpc).min(last)).ok()?;
            let last = receipt.clock.time(last).ok()?;
            Some(LossEvidence::ordinary(reason, None, std::num::NonZeroU64::new(span.count)?, first, last))
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
        self.clipboard.note_activity(generation, at);
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
        let visit = self.confirmed_visit(at)?;
        if self.approvals.get_at(visit.foreground.process, at)? != executable {
            return None;
        }
        Some((visit.receipt_proof(), visit.elevated_binding()))
    }

    /// The current visit, if it started by `at` and is still confirmed: no
    /// pending focus change, the same foreground and, for an elevated visit,
    /// input certified past `at` and the same focus.
    fn confirmed_visit(&self, at: QpcTick) -> Option<Visit> {
        let visit = self.timeline.current().filter(|visit| visit.started_at <= at)?;
        let confirmed = !self.queries.changed() && WindowProcess::foreground() == Some(visit.foreground)
            && (!visit.is_elevated() || self.has_certified_elevated_input(visit, at) && self.confirm_focus(visit));
        confirmed.then_some(visit)
    }

    pub fn save_geometry(&self, monitor: MonitorId, at: QpcTick) -> Option<GeometryId> {
        self.routes.save_geometry(monitor, at)
    }

    pub fn checkpoint_context(&self, at: QpcTick, source: Option<VisitBinding>) -> Option<SaveWindowContext> {
        let visit = self.confirmed_visit(at).filter(|visit| visit.elevated_binding() == source)?;
        let geometries = self.routes.visit_geometries(visit.generation, at);
        let busy = self.busy.current().filter(|hint| hint.generation == visit.generation)
            .map_or(BusyLevel::Clear, |hint| hint.level);
        Some(SaveWindowContext { window: visit.foreground.window, focus: visit.generation,
            geometries: BoundedList::new(geometries).ok()?, busy })
    }

    pub fn poll_clipboard(&mut self, receipt: Receipt, now: QpcTick) -> Vec<Value> {
        if !self.clipboard.stage_notices(&mut self.losses) { return Vec::new(); }
        let current = self.timeline.current();
        let safe = current.filter(|visit| self.queries.clipboard_safe() && visit.clipboard_focus_is_current());
        self.clipboard.poll(safe, current, || self.queries.clipboard_guard(), receipt, now, &mut self.losses)
    }

    pub fn approve_receipt(
        &mut self, proof: ForegroundAtReceipt, native_receipt: QpcTick, first: QpcTick,
    ) -> Option<ReceiptFocus> {
        let original = native_receipt.min(first);
        let current = self.timeline.current();
        if let Some(visit) = current.filter(|visit| visit.elevated.is_some()) {
            if visit.receipt_proof() != proof || original < visit.started_at
                || !self.has_certified_elevated_input(visit, native_receipt.max(first))
            {
                return None;
            }
            return Some(self.classify_receipt(visit, proof));
        }
        self.approvals.get_at(proof.process, original)?;
        if !self.ordinary_process(proof.process) { return None; }
        let Some(visit) = current.filter(|visit| visit.receipt_proof() == proof && visit.started_at <= original
            && !self.queries.changed()) else {
            return Some(self.timeline.historical(proof, original, native_receipt.max(first)));
        };
        Some(self.classify_receipt(visit, proof))
    }

    /// A receipt in the current `visit`: a password focus at the receipt, or
    /// the visit's classification.
    fn classify_receipt(&self, visit: Visit, proof: ForegroundAtReceipt) -> ReceiptFocus {
        if proof.focus.is_some_and(|focus| focus.password_style == Some(true)) {
            return ReceiptFocus::Classified(visit, TextTarget::Password, None);
        }
        match self.queries.classification() {
            Classification::Pending => ReceiptFocus::Pending(visit),
            Classification::Resolved(target, completed) => ReceiptFocus::Classified(visit, target, completed),
        }
    }

    /// Confirmed TextEdit commits, and typed input losses for every commit
    /// that could not be confirmed inside the admission (I7), including the
    /// commit losses of an elevated visit and its collector's TextEdit gaps.
    /// Commits come only from editable, non-password fields, so the losses
    /// are ordinary.
    pub fn take_confirmed_text(&mut self, receipt: Receipt, now: QpcTick) -> (Vec<ConfirmedText>, Vec<LossEvidence>) {
        use screen_fix_native_observation::uia_commits::CommitLossReason;
        let visit = self.timeline.current();
        let elevated = visit.is_some_and(Visit::is_elevated).then(|| self.take_elevated_text(now));
        let mut lost: Vec<_> = std::mem::take(&mut self.text_losses.0).into_iter()
            .filter_map(|(reason, span)| text_loss(receipt, reason, span)).collect();
        if let Some(confirmed) = elevated {
            return (confirmed, lost);
        }
        let mut lose = |reason, span| lost.extend(text_loss(receipt, reason, span));
        for loss in self.queries.commit_losses() {
            let reason = match loss.reason {
                CommitLossReason::Capacity => LossReason::CommitCapacity,
                CommitLossReason::TooLong => LossReason::CommitTooLong,
                CommitLossReason::Parts => LossReason::CommitParts,
                CommitLossReason::Rejected => LossReason::CommitRejected,
                CommitLossReason::Stale => LossReason::LateCommit,
            };
            lose(reason, loss.span);
        }
        let mut commits = std::mem::take(&mut self.future_text);
        commits.extend(self.queries.commits());
        let mut confirmed = Vec::new();
        for commit in commits {
            let Some(visit) = visit.filter(|visit| visit.generation == commit.generation) else {
                lose(LossReason::LateCommit, LossSpan::at(commit.receipt));
                continue;
            };
            if commit.receipt > now {
                if self.future_text.len() < FUTURE_COMMIT_LIMIT {
                    self.future_text.push(commit);
                } else {
                    lose(LossReason::CommitCapacity, LossSpan::at(commit.receipt));
                }
                continue;
            }
            if commit.receipt < visit.started_at || !self.queries.accepts_commit(commit.generation, commit.revision)
                || !self.confirm_focus(visit)
            {
                lose(LossReason::LateCommit, LossSpan::at(commit.receipt));
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
        self.timeline.current().is_some_and(|visit| visit.generation == text.visit.generation)
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
            return self.timeline.current().is_some_and(|current| current.elevated == Some((binding, focus)))
                && WindowProcess::foreground() == Some(visit.foreground)
                && elevated_focus_matches(visit.foreground, focus);
        }
        !self.queries.changed() && self.queries.blocked(visit.foreground.process).is_none()
            && WindowProcess::foreground() == Some(visit.foreground)
            && FocusSnapshot::read(visit.foreground).is_some_and(|native|
                !native.hung && visit.native.is_some_and(|expected| native.focus == expected.focus))
            && !self.queries.changed()
    }

    pub fn focus_end(&self, generation: FocusGeneration, clock: ClockOrigin) -> Option<SessionTimeUs> {
        self.timeline.focus_end(generation, clock)
    }

    /// Attribute a hook-only receipt `at` (remediation §6.4).
    pub fn hook_receipt(&self, at: QpcTick, clock: ClockOrigin) -> HookReceipt {
        self.timeline.hook_receipt(at, clock)
    }

    /// Drop ended visits that can no longer attribute an unpublished receipt.
    pub fn prune_visits(&mut self, published: SessionTimeUs, clock: ClockOrigin, keep: Option<FocusGeneration>) {
        self.timeline.prune(published, clock, keep);
    }

    pub fn focus_boundary(&self) -> Option<QpcTick> {
        self.timeline.focus_boundary()
    }

    pub fn is_current(&self, proof: ForegroundAtReceipt, generation: Option<FocusGeneration>) -> bool {
        self.timeline.current().is_some_and(|visit| visit.receipt_proof() == proof && generation == Some(visit.generation))
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
        let visit = self.timeline.current();
        let selected = self.routes.routable();
        let approvals = &mut self.approvals;
        let change = self.cursor.sample(force, |point| {
            visit.is_some() && selected.iter().any(|geometry| geometry.bounds.contains(point))
                && uia::window_at(point).is_some_and(|window| approvals.get(window.process).is_some())
        });
        if let Some(change) = change {
            self.stage_cursor(change);
        }
    }

    pub fn pointer_geometry_at(&self, point: PhysicalPoint, at: QpcTick) -> Evidence<GeometryId> {
        self.routes.pointer_geometry_at(point, at)
    }

    pub fn window_geometry_at(&self, proof: ForegroundAtReceipt, at: QpcTick) -> Evidence<GeometryId> {
        self.routes.window_geometry_at(self.timeline.current(), proof, at)
    }

    fn observe_visit_geometry(&mut self, visit: Visit, lower_bound: QpcTick) -> Result<(), ProbeError> {
        let bounds = WindowMetadata::read(visit.foreground, &self.scrubber).and_then(|snapshot| snapshot.bounds);
        let measured = clock::qpc()?.max(lower_bound);
        self.routes.observe_bounds(visit, bounds, measured);
        Ok(())
    }

    pub fn pointer_query(
        &mut self, point: PhysicalPoint, geometry: GeometryId, origin: ClockOrigin, at: QpcTick, action_at: Option<QpcTick>,
    ) -> Result<Option<PointerLink>, ProbeError> {
        if self.routes.point_geometry(point) != Some(geometry) { return Ok(None); }
        let Some(visit) = self.timeline.current() else { return Ok(None); };
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

    fn window_row(&mut self, proof: WindowProcess, kind: WindowRowKind) -> Option<Value> {
        if hung::is_ghost(proof.hwnd().0) { return None; }
        let identity = self.approvals.get(proof.process)?;
        if !self.ordinary_process(proof.process) { return None; }
        // An identity change during the read gets one re-read; an approved
        // window whose metadata still cannot be read stays approved (C4).
        let Some(window) = WindowMetadata::read(proof, &self.scrubber).or_else(|| WindowMetadata::read(proof, &self.scrubber)) else {
            return Some(unreadable_window_row(kind, proof));
        };
        Some(self.window_evidence_row(proof, identity, kind, window))
    }

    /// A window row with the PDF foreground fields (D-1): `pid`, `exe`, `rect`,
    /// `monitor`, `product`, `version`, `version_string`, `title` and `class`.
    fn window_evidence_row(
        &self, proof: WindowProcess, identity: &crate::config::ExecutableIdentity,
        kind: WindowRowKind, window: WindowMetadata,
    ) -> Value {
        let version = self.executable_versions.0.iter().find(|(executable, _)| executable == identity)
            .and_then(|(_, version)| version.as_ref());
        let geometry = window.bounds.and_then(|bounds| self.routes.only_geometry(bounds));
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

    /// The Recorder's access to `proof`, cached for `ACCESS_RECHECK`.
    fn access(&mut self, proof: ProcessInstance) -> Result<screen_fix_elevated::ForegroundAccess, AccessUnknown> {
        use screen_fix_elevated::{Failure, foreground_access};
        use crate::protocol::{TokenIdentityCheck as Check, TokenSubject as Subject};
        let now = Instant::now();
        self.ordinary_access.retain(|(_, expires, _)| *expires > now);
        if let Some((_, _, access)) = self.ordinary_access.iter().find(|(process, _, _)| *process == proof) {
            return *access;
        }
        let access = ProcessHandle::open_instance(proof)
            .and_then(|process| process.image_path())
            .ok_or(AccessUnknown::Unclassified)
            .and_then(|image| foreground_access(proof, &image).map_err(|failure| match failure {
                Failure::TokenIdentity(identity) if matches!(identity.subject, Subject::Foreground | Subject::ForegroundLinkedToken)
                    && matches!(identity.check, Check::ProtectedProcess | Check::DifferentSession | Check::DifferentUser
                        | Check::DifferentLogon | Check::SystemAccount | Check::UnsupportedIntegrity
                        | Check::UnsupportedSplitPair | Check::LinkedLogonMismatch) => AccessUnknown::Ineligible,
                _ => AccessUnknown::Unclassified,
            }));
        if self.ordinary_access.len() == ACCESS_CACHE_LIMIT { self.ordinary_access.pop_front(); }
        self.ordinary_access.push_back((proof, now + ACCESS_RECHECK, access));
        access
    }
}

/// The QPC interval in which a notice's source message tick was posted;
/// `None` without a tick anchor or a source tick.
fn notice_interval(notice: Notice, receipt: Receipt) -> Option<(QpcTick, QpcTick)> {
    receipt.tick_anchor.zip(notice.source_tick).and_then(|(anchor, tick)|
        clock::message_tick_interval(anchor, receipt.clock.qpc_frequency, CoarseTime::windows_message(tick)))
}

fn add_win_event_timing(row: &mut Value, notice: Notice, receipt: Receipt) -> Result<(), ProbeError> {
    let Some(tick) = notice.source_tick else { return Ok(()); };
    let received = receipt.clock.time(notice.receipt)?;
    let coarse = CoarseTime::windows_message(tick);
    let late = clock::message_tick_estimate(receipt.clock, receipt.tick_anchor, coarse)
        .map(|estimate| estimate.is_late(received));
    // The observation uses the poll receipt. The callback receipt remains
    // independently available, including when notifications arrive out of order.
    row["win_event_recv_t_us"] = json!(received);
    MessageTick::new(Some(coarse), late).write(row).map_err(|_| unavailable("Encode the WinEvent timing of a context row"))
}

/// The row of an approved window whose metadata cannot be read (C4).
fn unreadable_window_row(kind: WindowRowKind, proof: WindowProcess) -> Value {
    json!({ "kind": kind, "approved": true, "window": proof.window, "pid": proof.process.pid, "metadata": "unavailable" })
}

/// Keep `notice` for a later poll. One beyond `FUTURE_NOTICE_LIMIT` is a
/// counted loss (C3); returns whether it was kept.
fn defer_notice(future: &mut Vec<Notice>, losses: &mut Losses, notice: Notice) -> bool {
    if future.len() < FUTURE_NOTICE_LIMIT {
        future.push(notice);
        true
    } else {
        losses.note(LossReason::NotificationOverflow, LossSpan::at(notice.receipt));
        false
    }
}

/// The `kind` of a window row: the foreground window, or a top-level window event.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum WindowRowKind {
    Foreground,
    WindowTitle,
    /// Rate-limited per window (C4).
    WindowLocation,
    WindowShown,
    WindowHidden,
}

/// The window row kind of a top-level window event.
fn window_event_kind(event: u32) -> Option<WindowRowKind> {
    match event {
        EVENT_OBJECT_NAMECHANGE => Some(WindowRowKind::WindowTitle),
        EVENT_OBJECT_LOCATIONCHANGE => Some(WindowRowKind::WindowLocation),
        EVENT_OBJECT_SHOW => Some(WindowRowKind::WindowShown),
        EVENT_OBJECT_HIDE => Some(WindowRowKind::WindowHidden),
        _ => None,
    }
}

/// A cursor name change asks for an immediate cursor sample.
fn cursor_notified(notices: &[Notice]) -> bool {
    notices.iter().any(|notice| notice.event == EVENT_OBJECT_NAMECHANGE && notice.object == OBJID_CURSOR as i32)
}

/// A `caret` row for `notice` from the focus snapshot read after it.
fn caret_row(visit: Visit, notice: Notice, snapshot: FocusSnapshot, receipt: Receipt) -> Result<Value, ProbeError> {
    let visible = notice.event != EVENT_OBJECT_HIDE && snapshot.caret.is_some();
    let change = match notice.event {
        EVENT_OBJECT_SHOW => "show",
        EVENT_OBJECT_HIDE => "hide",
        _ => "location",
    };
    let mut row = json!({ "kind": "caret", "generation": visit.generation, "change": change,
        "visible": visible, "bounds": visible.then_some(snapshot.caret).flatten() });
    add_win_event_timing(&mut row, notice, receipt)?;
    Ok(row)
}

/// The focus of `foreground` still has an elevated visit's `focus` identity,
/// and no local password marker contradicts the collector's.
pub(super) fn elevated_focus_matches(foreground: WindowProcess, focus: FocusAtReceipt) -> bool {
    FocusSnapshot::read(foreground).is_some_and(|native| native.matches_identity(focus)
        && native.focus.password_style.is_none_or(|local| focus.password_style == Some(local)))
}

/// A typed TextEdit input loss, clipped to the admission; `None` when it
/// ended before the admission (I7).
fn text_loss(receipt: Receipt, reason: LossReason, span: LossSpan) -> Option<LossEvidence> {
    if span.last < receipt.boundary.qpc { return None; }
    let first = receipt.clock.time(span.first.max(receipt.boundary.qpc)).ok()?;
    let last = receipt.clock.time(span.last).ok()?;
    Some(LossEvidence::ordinary(reason, Some(LossProducer::TextEdit), std::num::NonZeroU64::new(span.count)?, first, last))
}

fn read_window(window: WindowId) -> Option<WindowProcess> {
    WindowProcess::read(::windows::Win32::Foundation::HWND(window.0 as usize as *mut _))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    /// The row of an approved window whose metadata cannot be read.
    #[test]
    fn unreadable_window_bytes() -> serde_json::Result<()> {
        let pid = NonZeroU32::MIN.saturating_add(6);
        let proof = WindowProcess { window: WindowId(42), process: ProcessInstance { pid, creation_filetime: 9 }, thread: NonZeroU32::MIN };
        assert_eq!(serde_json::to_string(&unreadable_window_row(WindowRowKind::WindowShown, proof))?,
            r#"{"approved":true,"kind":"window_shown","metadata":"unavailable","pid":7,"window":42}"#);
        Ok(())
    }
}
