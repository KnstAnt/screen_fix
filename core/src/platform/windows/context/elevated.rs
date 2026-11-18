use screen_fix_elevated::{Failure, Result};
use screen_fix_protocol::ipc::elevated::{
    CommitEvidence, ContextEvidence, Query, ReadBinding, ReadEvidence, ReadOutcome, WindowEvidence, WindowInstance,
    runtime::{ContextGapCause, ContextProducer, IdentityClaim, Observation},
};

use super::*;
use crate::protocol::ipc::elevated::{ImagePath, runtime::ApprovalKey};
use crate::{platform::windows::elevated::{ContextDelivery, add_time}, platform::windows::controller::ActiveSourceProof};

const NATIVE_BUDGET: Duration = Duration::from_millis(250);
const WINDOW_NOTICES: usize = 32;
const CLIPBOARD_READ_BUDGET: Duration = Duration::from_millis(100);
const IME_BUDGET: Duration = Duration::from_millis(50);
const COMMIT_CAPACITY: usize = screen_fix_native_observation::uia_commits::CAPACITY;

#[derive(Default)]
pub(super) struct Observations {
    focus: Option<ReadBinding>,
    focus_observed: Option<QpcTick>,
    window: Option<(ReadBinding, WindowRowKind, WindowCause)>,
    hung: Option<ReadBinding>,
    windows: VecDeque<Notice>,
    caret: Option<Notice>,
    foreground_busy: bool,
    owned_windows: VecDeque<WindowId>,
    foreground: bool,
    input_before: Option<(VisitBinding, QpcTick)>,
    ime_focus: Option<ReadBinding>,
    ime_key: Option<(RequestId, QpcTick, Duration, Option<ReadBinding>)>,
    ime_result: Option<(RequestId, ImeState)>,
    commits: VecDeque<ConfirmedText>,
    clipboard: Option<ClipboardStage>,
}

/// Why a collector window read was requested.
#[derive(Clone, Copy)]
enum WindowCause {
    /// The periodic foreground sweep.
    Sweep,
    /// A window notice; its row carries the notice timing.
    Notice(Notice),
    /// The trailing re-read of a rate-limited location notice (C4).
    Trailing,
}

enum ClipboardStage {
    Pre { notice: clipboard::Notice, fence: ReadBinding },
    Read { notice: clipboard::Notice, pre: QpcTick, permit: clipboard::Permit },
    Post { read: clipboard::Read, fence: ReadBinding },
    Publish { read: clipboard::Read, fence_completed: QpcTick, deadline: QpcTick },
}

impl Context<'_> {
    pub fn certify_elevated_input(&mut self, certificate: InputCertificate) {
        if self.current.and_then(Visit::elevated_binding) == Some(certificate.visit) {
            self.elevated.input_before = Some((certificate.visit, certificate.before));
        }
    }

    pub fn truncate_elevated_input(&mut self, before: QpcTick) {
        if let Some((visit, current)) = self.elevated.input_before {
            self.elevated.input_before = Some((visit, current.min(before)));
        }
    }

    pub(super) fn has_certified_elevated_input(&self, visit: Visit, at: QpcTick) -> bool {
        visit.elevated.is_some_and(|(binding, _)| self.elevated.input_before
            .is_some_and(|(certified, before)| certified == binding && at < before))
    }

    pub fn discard_elevated_clipboard(&mut self) {
        self.elevated.clipboard = None;
    }

    pub fn poll_elevated_queries(
        &mut self, source: &mut Selector<'_>, at: QpcTick, receipt: Receipt, admission: &ActiveSourceProof,
    )
        -> Result<Vec<Value>>
    {
        let Some(visit) = self.current else { return Ok(Vec::new()); };
        let Some((binding, focus)) = visit.elevated else { return Ok(Vec::new()); };
        let mut rows = Vec::new();
        self.poll_health(at);
        let now = Instant::now();
        let display_changed = now >= self.next_displays;
        if display_changed { self.next_displays = now + DISPLAY_REFRESH; }
        self.refresh_displays(at, &mut rows, display_changed);
        self.replay_future_notices(source, visit, at, receipt, &mut rows);
        let mut notices = Vec::new();
        for (arrived, delivery) in source.observations(at) {
            let first_row = rows.len();
            self.queries.remote_focus_result(&delivery, arrived, receipt.clock, &mut rows);
            let routes = &self.routable;
            self.queries.remote_pointer_result(&delivery, arrived, receipt.clock,
                |query| valid_point(query, routes), &mut rows);
            self.elevated_fact(delivery, &mut notices, &mut rows, visit, arrived, at, receipt, admission)?;
            for row in &mut rows[first_row..] {
                row["context_receipt_t_us"] = json!(receipt.clock.time(arrived).ok());
            }
        }
        if now >= self.next_sweep || !self.elevated.foreground {
            self.next_sweep = now + WINDOW_REFRESH;
            self.request_sweep(source, binding, focus, at)?;
        }
        if self.elevated.ime_focus.is_some_and(|read| read.deadline_qpc <= at && !source.deferred_read(read)) {
            self.elevated.ime_focus = None;
            self.elevated_ime_focus(visit, ImeState::Unknown, None, None, receipt.clock, &mut rows);
        }
        self.poll_elevated_key(source, binding, focus, at)?;
        self.poll_elevated_clipboard(source, visit, at, receipt, &mut rows)?;
        self.queries.poll_remote_focus(source, at, receipt.clock, &mut rows)?;
        if let Some(point) = self.queries.pointer_rest(now) && let Some(geometry) = self.point_geometry(point) {
            // The query's only failure is its request counter overflowing.
            self.pointer_query(point, geometry, receipt.clock, at, None).map_err(|_| Failure::Protocol)?;
        }
        let routes = &self.routable;
        self.queries.poll_elevated_pointer(source, visit, at, receipt.clock,
            |query| valid_point(query, routes), &mut rows)?;
        if !self.elevated.foreground {
            self.elevated.foreground = true;
            rows.push(json!({ "kind": "keyboard_layout", "generation": visit.generation, "layout": focus.keyboard_layout }));
        }
        self.expire_reads(source, visit, at, &mut rows);
        if self.elevated.window.is_none() {
            self.elevated_window_notice(source, visit, at, receipt, &mut rows)?;
        }
        self.elevated_trailing_locations(source, visit, at, &mut rows)?;
        let process_hung = self.hung.contains(visit.foreground.process);
        let scoped_busy = self.elevated.foreground_busy || !self.elevated.owned_windows.is_empty();
        rows.extend(self.busy_row(process_hung, &notices, scoped_busy, at, receipt.clock));
        rows.extend(self.elevated_caret(visit, at, receipt)?);
        self.sample_cursor(cursor_notified(&notices));
        self.cursor_rows(at, receipt.clock, &mut rows);
        Ok(rows)
    }

    /// Replay the native notices deferred for this visit once the collector's
    /// context progress covers them. A notice after `at` stays deferred; a
    /// stale window notice of the visit is reported timed out (C3).
    fn replay_future_notices(
        &mut self, source: &Selector<'_>, visit: Visit, at: QpcTick, receipt: Receipt, rows: &mut Vec<Value>,
    ) {
        let context_before = source.context_before();
        for notice in std::mem::take(&mut self.future_notices) {
            if notice.receipt <= at && !recent(receipt.clock, notice.receipt, at, NATIVE_BUDGET) {
                if authorized_notice(notice, visit, receipt, at) && let Some(kind) = window_notice_kind(notice) {
                    rows.push(native_status_row(kind, None, NativeStatus::TimedOut));
                }
                continue;
            }
            if notice.receipt > at || context_before.is_none_or(|before| notice.receipt >= before) {
                self.future_notices.push(notice);
            } else if authorized_notice(notice, visit, receipt, at) {
                if notice.object == OBJID_WINDOW && matches!(notice.event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) {
                    self.elevated.owned_windows.retain(|window| *window != notice.window);
                }
                if window_notice_kind(notice).is_some() { self.queue_window_notice(notice); }
            }
        }
    }

    /// Request the periodic reads of the elevated foreground that are not pending.
    fn request_sweep(&mut self, source: &mut Selector<'_>, binding: VisitBinding, focus: FocusAtReceipt, at: QpcTick)
        -> Result<()>
    {
        if self.elevated.focus.is_none() {
            self.elevated.focus = request(source, binding, binding.foreground, at, Query::NativeFocus, None)?;
        }
        if self.elevated.ime_focus.is_none() {
            let read = ReadBinding { visit: binding, request: RequestId(0), target: binding.foreground,
                requested_qpc: at, deadline_qpc: source.read_deadline(at, IME_BUDGET)? };
            self.elevated.ime_focus = source.query(read, |read| Query::ImeFocus { read, expected: focus }, None)?;
        }
        if self.elevated.hung.is_none() {
            self.elevated.hung = request(source, binding, binding.foreground, at, Query::Hung, None)?;
        }
        if self.elevated.window.is_none() && self.elevated.windows.is_empty() {
            self.elevated.window = request(source, binding, binding.foreground, at, Query::Window, None)?
                .map(|read| (read, WindowRowKind::Foreground, WindowCause::Sweep));
        }
        Ok(())
    }

    /// Drop the focus, hung and window reads whose deadline passed without a
    /// pending result; the hung and window reads are reported incomplete.
    fn expire_reads(&mut self, source: &Selector<'_>, visit: Visit, at: QpcTick, rows: &mut Vec<Value>) {
        let expired = |read: ReadBinding| read.deadline_qpc <= at && !source.deferred_read(read);
        if self.elevated.focus.is_some_and(expired) {
            self.elevated.focus = None;
        }
        if self.elevated.hung.is_some_and(expired) {
            self.elevated.hung = None;
            rows.push(json!({ "kind": "app_hung_coverage", "complete": false, "source": "elevated" }));
        }
        if self.elevated.window.is_some_and(|(read, _, _)| expired(read))
            && let Some((read, kind, _)) = self.elevated.window.take()
        {
            if read.target == visit.foreground.into() { self.observe_bounds(visit, None, at); }
            rows.push(native_status_row(kind, Some(read.target.window), NativeStatus::TimedOut));
        }
    }

    /// A `caret` row for the latest caret notice of the elevated editable
    /// focus, once a later focus read confirmed the focus.
    fn elevated_caret(&mut self, visit: Visit, at: QpcTick, receipt: Receipt) -> Result<Option<Value>> {
        if self.elevated.caret.is_some_and(|notice| !recent(receipt.clock, notice.receipt, at, NATIVE_BUDGET)) {
            self.elevated.caret = None;
        }
        if self.classified_target() == Some(TextTarget::Editable)
            && let Some(notice) = self.elevated.caret
            && let Some(snapshot) = self.current.and_then(|visit| visit.native)
            && self.elevated.focus_observed.is_some_and(|observed| observed >= notice.receipt)
            && notice.thread == snapshot.focus.thread.get()
            && (Some(notice.window) == snapshot.caret_window || notice.window == snapshot.focus.window)
            && self.confirm_focus(visit)
        {
            self.elevated.caret = None;
            return caret_row(visit, notice, snapshot, receipt).map(Some).map_err(|_| TIMESTAMP_FAILURE);
        }
        Ok(None)
    }

    fn elevated_fact(
        &mut self, delivery: ContextDelivery, notices: &mut Vec<Notice>, rows: &mut Vec<Value>,
        visit: Visit, arrived: QpcTick, at: QpcTick, receipt: Receipt, admission: &ActiveSourceProof,
    ) -> Result<()> {
        let Some((binding, focus)) = visit.elevated else { return Ok(()); };
        let evidence = match delivery {
            ContextDelivery::Observation(Observation::Item { evidence, .. }) => evidence,
            ContextDelivery::Observation(Observation::Gap(gap)) if gap.visit == binding => {
                // A collector gap is a typed loss per cause (E3); a TextEdit
                // gap is an input loss of that producer (I7).
                let reason = match (gap.producer, gap.cause) {
                    (ContextProducer::TextEdit, ContextGapCause::Overflow) => LossReason::CommitCapacity,
                    (ContextProducer::Notices, ContextGapCause::Overflow) => LossReason::NotificationOverflow,
                    (_, ContextGapCause::Overflow) => LossReason::QueueOverflow,
                    (_, ContextGapCause::Stale) => LossReason::LateCommit,
                    (_, ContextGapCause::CommitTooLong) => LossReason::CommitTooLong,
                    (_, ContextGapCause::CommitParts) => LossReason::CommitParts,
                    (_, ContextGapCause::CommitRejected) => LossReason::CommitRejected,
                };
                let losses = if gap.producer == ContextProducer::TextEdit { &mut self.text_losses } else { &mut self.losses };
                losses.note(reason, LossSpan { count: gap.count(), first: gap.first_qpc, last: gap.last_qpc });
                return Ok(());
            }
            ContextDelivery::Rejected(read, reason) => ContextEvidence::Rejected(ReadEvidence {
                binding: read, started_qpc: None, completed_qpc: None, outcome: ReadOutcome::Unavailable(reason),
            }),
            _ => return Ok(()),
        };
        match evidence {
            ContextEvidence::Notice(item) if item.visit == binding && item.observed_qpc == item.value.receipt
                && authorized_notice(item.value, visit, receipt, at) =>
            {
                let notice = item.value;
                if notice.object == OBJID_WINDOW && matches!(notice.event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) {
                    self.elevated.owned_windows.retain(|window| *window != notice.window);
                }
                if notice.window == visit.foreground.window && notice.object == OBJID_WINDOW
                    && notice.event == EVENT_OBJECT_LOCATIONCHANGE
                {
                    self.visit_bounds = None;
                    self.visit_geometry = None;
                }
                if window_notice_kind(notice).is_some() {
                    self.queue_window_notice(notice);
                }
                if notice.object == OBJID_CARET as i32
                    && matches!(notice.event, EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE | EVENT_OBJECT_LOCATIONCHANGE)
                {
                    self.elevated.caret = Some(notice);
                }
                notices.push(notice);
            }
            ContextEvidence::NativeFocus(read) if self.elevated.focus == Some(read.binding) => {
                self.elevated.focus = None;
                if let Some((observed, snapshot)) = fresh_read(&read, arrived) && snapshot.focus == focus
                    && let Some(current) = self.current.as_mut()
                {
                    self.elevated.focus_observed = Some(observed);
                    // Collector evidence carries no fallback marker. A fallback focus
                    // has no password evidence, and the collector itself withholds
                    // focus reads and IME state for it.
                    current.native = Some(FocusSnapshot {
                        focus: snapshot.focus, caret: snapshot.caret, caret_window: snapshot.caret_window,
                        gui_flags: snapshot.gui_flags, hung: snapshot.hung, active_fallback: false,
                    });
                }
            }
            ContextEvidence::Window(read) if self.elevated.window.is_some_and(|(pending, _, _)| pending == read.binding) => {
                let (_, kind, cause) = self.elevated.window.take().ok_or(Failure::Supervisor)?;
                let mut row = self.elevated_window_row(visit, kind, &read, arrived);
                match cause {
                    WindowCause::Notice(notice) => {
                        event_timing(&mut row, notice, receipt)?;
                        row["source_event_tick_ms"] = json!(notice.source_tick);
                    }
                    WindowCause::Trailing => row["trailing"] = json!(true),
                    WindowCause::Sweep => {}
                }
                rows.push(row);
            }
            ContextEvidence::HungRead(read) if self.elevated.hung == Some(read.binding) => {
                self.elevated.hung = None;
                if let Some((measured, evidence)) = fresh_read(&read, arrived) && evidence.window == binding.foreground {
                    let first_row = rows.len();
                    self.hung.observe_scoped(visit.foreground, evidence.direct, evidence.ghost, rows);
                    for row in &mut rows[first_row..] { row["observed_qpc"] = json!(measured); }
                }
            }
            ContextEvidence::ImeFocus(read) if self.elevated.ime_focus == Some(read.binding) => {
                self.elevated.ime_focus = None;
                let state = fresh_read(&read, arrived).map_or(ImeState::Unknown, |(_, state)| *state);
                self.elevated_ime_focus(visit, state, read.started_qpc, read.completed_qpc, receipt.clock, rows);
            }
            ContextEvidence::ImeKey { key, key_qpc, read }
                if self.elevated.ime_key.is_some_and(|(id, original, _, pending)|
                    id == key && original == key_qpc && pending == Some(read.binding)) =>
            {
                self.elevated.ime_key = None;
                self.elevated.ime_result = Some((key, fresh_read(&read, arrived).map_or(ImeState::Unknown, |(_, state)| *state)));
            }
            ContextEvidence::Commit(commit) if commit.binding.visit == binding => {
                self.elevated_commit(commit, visit, arrived, at, receipt.clock);
            }
            ContextEvidence::ClipboardFence(read) if read.binding.visit == binding => {
                self.clipboard_fence(read, visit, arrived, at, receipt.clock, admission);
            }
            ContextEvidence::Rejected(read) if read.binding.visit == binding => {
                self.rejected_read(read, visit, at, receipt.clock, rows);
            }
            _ => {}
        }
        Ok(())
    }

    /// Settle every pending read that the collector or the Recorder rejected.
    fn rejected_read(&mut self, read: ReadEvidence<()>, visit: Visit, at: QpcTick, clock: ClockOrigin, rows: &mut Vec<Value>) {
        if self.elevated.clipboard.as_ref().is_some_and(|stage| match stage {
            ClipboardStage::Pre { fence, .. } | ClipboardStage::Post { fence, .. } => *fence == read.binding,
            _ => false,
        }) {
            self.elevated.clipboard = None;
        }
        if self.elevated.ime_focus == Some(read.binding) {
            self.elevated.ime_focus = None;
            self.elevated_ime_focus(visit, ImeState::Unknown, read.started_qpc, read.completed_qpc, clock, rows);
        }
        if let Some((key, _, _, Some(pending))) = self.elevated.ime_key && pending == read.binding {
            self.elevated.ime_key = None;
            self.elevated.ime_result = Some((key, ImeState::Unknown));
        }
        if self.elevated.focus == Some(read.binding) { self.elevated.focus = None; }
        if self.elevated.hung == Some(read.binding) { self.elevated.hung = None; }
        if self.elevated.window.is_some_and(|(pending, _, _)| pending == read.binding)
            && let Some((_, kind, _)) = self.elevated.window.take()
        {
            if read.binding.target == visit.foreground.into() { self.observe_bounds(visit, None, at); }
            rows.push(native_status_row(kind, Some(read.binding.target.window), NativeStatus::Rejected(read.outcome)));
        }
    }

    fn clipboard_fence(
        &mut self, fence: ReadEvidence<()>, visit: Visit, arrived: QpcTick, at: QpcTick,
        clock: ClockOrigin, admission: &ActiveSourceProof,
    ) {
        let Some(worker) = &self.clipboard else { return; };
        let Some(stage) = self.elevated.clipboard.take() else { return; };
        let safe = admission.current() && self.queries.elevated_clipboard_safe(fence.binding.visit)
            && visit.elevated_clipboard_focus_is_current();
        self.elevated.clipboard = match stage {
            ClipboardStage::Pre { notice, fence: expected } if fence.binding == expected => {
                fresh_read(&fence, arrived).filter(|_| safe && clipboard::notice_current(notice, visit))
                    .and_then(|(pre, _)| {
                        let until = add_time(pre, clock.qpc_frequency, CLIPBOARD_READ_BUDGET).ok()?.min(expected.deadline_qpc);
                        if at >= until { return None; }
                        worker.grant_elevated(notice, visit, until, admission.clone())
                            .map(|permit| ClipboardStage::Read { notice, pre, permit })
                    })
            }
            ClipboardStage::Post { read, fence: expected } if fence.binding == expected => {
                if let Some((completed, _)) = fresh_read(&fence, arrived)
                    && fence.started_qpc.is_some_and(|started| started >= read.completed)
                    && safe && clipboard::notice_current(read.notice, visit)
                {
                    add_time(completed, clock.qpc_frequency, NATIVE_BUDGET).ok()
                        .map(|deadline| ClipboardStage::Publish { read, fence_completed: completed, deadline })
                } else {
                    None
                }
            }
            stage => Some(stage),
        };
    }

    fn poll_elevated_clipboard(
        &mut self, source: &mut Selector<'_>, visit: Visit, at: QpcTick, receipt: Receipt, rows: &mut Vec<Value>,
    ) -> Result<()> {
        let Some(worker) = &self.clipboard else { return Ok(()); };
        let Some((binding, _)) = visit.elevated else { return Ok(()); };
        take_clipboard_notices(worker, &mut self.clipboard_notices, &mut self.losses);
        self.clipboard_notices.retain(|notice| notice.observed.elapsed() < clipboard::MAX_AGE
            && notice.foreground == visit.foreground && notice.received >= visit.started_at
            && notice.received >= receipt.boundary.qpc);
        let safe = self.queries.elevated_clipboard_safe(binding) && visit.elevated_clipboard_focus_is_current();
        for read in due_clipboard_reads(&mut self.future_clipboard, worker.results(), at, &mut self.losses) {
            let Some(ClipboardStage::Read { notice, pre, permit }) = self.elevated.clipboard.as_ref() else { continue; };
            if *notice != read.notice || read.visit.generation != visit.generation { continue; }
            let qualified = safe && read.started >= *pre && read.started <= read.completed
                && read.completed < permit.until() && clipboard::notice_current(read.notice, visit);
            self.elevated.clipboard = None;
            if qualified {
                let until = source.read_deadline(read.notice.received, clipboard::MAX_AGE)?;
                if let Some(fence) = clipboard_request(source, binding, at, until)? {
                    self.elevated.clipboard = Some(ClipboardStage::Post { read, fence });
                }
            }
        }
        let phase = self.elevated.clipboard.take();
        self.elevated.clipboard = match phase {
            Some(ClipboardStage::Publish { read, fence_completed, deadline })
                if safe && at < deadline && clipboard::notice_current(read.notice, visit) =>
            {
                if source.context_before().is_some_and(|before| fence_completed < before) {
                    rows.push(clipboard_row(read, receipt.clock));
                    None
                } else {
                    Some(ClipboardStage::Publish { read, fence_completed, deadline })
                }
            }
            Some(stage @ ClipboardStage::Pre { notice, fence }) if safe && (at < fence.deadline_qpc || source.deferred_read(fence))
                && clipboard::notice_current(notice, visit) => Some(stage),
            Some(ClipboardStage::Read { notice, pre, permit }) if safe && at < permit.until()
                && clipboard::notice_current(notice, visit) => Some(ClipboardStage::Read { notice, pre, permit }),
            Some(ClipboardStage::Post { read, fence }) if safe && (at < fence.deadline_qpc || source.deferred_read(fence))
                && clipboard::notice_current(read.notice, visit) => Some(ClipboardStage::Post { read, fence }),
            _ => None,
        };
        if !safe || self.elevated.clipboard.is_some() { return Ok(()); }
        let eligible = self.clipboard_notices.iter().rposition(|notice| {
            notice.received <= at && clipboard_change_after_visit(*notice, visit, receipt)
                && (notice.owner.is_some() || clipboard_activity(&self.approved_activity, *notice, visit, receipt.clock))
                && clipboard::notice_current(*notice, visit)
        });
        if let Some(index) = eligible && let Some(notice) = self.clipboard_notices.remove(index) {
            let until = source.read_deadline(notice.received, clipboard::MAX_AGE)?;
            let Some(fence) = clipboard_request(source, binding, at, until)? else { return Ok(()); };
            self.elevated.clipboard = Some(ClipboardStage::Pre { notice, fence });
        }
        Ok(())
    }

    pub(super) fn elevated_ime_for_key(&mut self, key: RequestId, qpc: QpcTick, max_wait: Duration) -> Option<ImeState> {
        if let Some((candidate, state)) = self.elevated.ime_result.take() && candidate == key {
            return Some(state);
        }
        if let Some((candidate, _, _, _)) = self.elevated.ime_key {
            return (candidate != key).then_some(ImeState::Unknown);
        }
        self.elevated.ime_key = Some((key, qpc, max_wait, None));
        None
    }

    fn poll_elevated_key(&mut self, source: &mut Selector<'_>, visit: VisitBinding, focus: FocusAtReceipt, at: QpcTick)
        -> Result<()>
    {
        let Some((key, original, max_wait, pending)) = self.elevated.ime_key else { return Ok(()); };
        let cutoff = source.read_deadline(original, max_wait)?;
        if cutoff <= at || pending.is_some_and(|read| read.deadline_qpc <= at && !source.deferred_read(read)) {
            self.elevated.ime_key = None;
            self.elevated.ime_result = Some((key, ImeState::Unknown));
        } else if pending.is_none() {
            let read = ReadBinding { visit, request: RequestId(0), target: visit.foreground,
                requested_qpc: at, deadline_qpc: source.read_deadline(at, IME_BUDGET)?.min(cutoff) };
            if let Some(read) = source.query(read, |read| Query::ImeKey { read, expected: focus, key, key_qpc: original }, None)? {
                self.elevated.ime_key = Some((key, original, max_wait, Some(read)));
            }
        }
        Ok(())
    }

    fn elevated_ime_focus(
        &mut self, visit: Visit, state: ImeState, start: Option<QpcTick>, end: Option<QpcTick>, clock: ClockOrigin,
        rows: &mut Vec<Value>,
    ) {
        if self.last_ime_focus != Some((visit.generation, state)) {
            self.last_ime_focus = Some((visit.generation, state));
            rows.push(json!({ "kind": "ime_state", "generation": visit.generation, "ime": state,
                "started_t_us": start.and_then(|at| clock.time(at).ok()),
                "completed_t_us": end.and_then(|at| clock.time(at).ok()) }));
        }
    }

    /// Stage a collector commit; one that cannot be staged is a TextEdit input loss (I7).
    fn elevated_commit(&mut self, commit: CommitEvidence, visit: Visit, arrived: QpcTick, at: QpcTick, clock: ClockOrigin) {
        if !self.queries.remote_commit(&commit, at, clock) || !self.confirm_focus(visit) {
            self.text_losses.note(LossReason::LateCommit, LossSpan::at(commit.receipt_qpc));
            return;
        }
        if self.elevated.commits.len() == COMMIT_CAPACITY {
            self.text_losses.note(LossReason::CommitCapacity, LossSpan::at(commit.receipt_qpc));
            return;
        }
        self.elevated.commits.push_back(ConfirmedText {
            visit, revision: commit.binding.visit.focus_revision, source_read: Some(commit.binding),
            callback_at: commit.receipt_qpc, observed_at: arrived, text: commit.text.as_str().to_owned(), valid: commit.valid,
        });
    }

    pub(super) fn take_elevated_text(&mut self, now: QpcTick) -> Vec<ConfirmedText> {
        let pending = std::mem::take(&mut self.elevated.commits);
        let mut confirmed = Vec::new();
        for commit in pending {
            if commit.observed_at <= now && self.confirm_text(&commit) {
                confirmed.push(commit);
            } else {
                // A commit that cannot be confirmed is counted (E3, I7).
                self.text_losses.note(LossReason::LateCommit, LossSpan::at(commit.callback_at));
            }
        }
        confirmed
    }

    fn elevated_window_row(
        &mut self, visit: Visit, kind: WindowRowKind, read: &ReadEvidence<WindowEvidence>, at: QpcTick,
    ) -> Value {
        let target = read.binding.target;
        let Some((observed, evidence)) = fresh_read(read, at) else {
            if target == visit.foreground.into() { self.observe_bounds(visit, None, at); }
            return native_status_row(kind, Some(target.window), NativeStatus::Unavailable);
        };
        let window = WindowProcess::from(target);
        if WindowProcess::read(window.hwnd()) != Some(window) {
            return native_status_row(kind, Some(target.window), NativeStatus::Stale);
        }
        let Some(identity) = self.approvals.get(target.process) else {
            return json!({ "kind": kind, "approved": false });
        };
        let snapshot = WindowMetadata {
            bounds: evidence.bounds, title: evidence.title.as_ref().map(|text| text.as_str().to_owned()),
            class: evidence.class.as_ref().map(|text| text.as_str().to_owned()), dpi: evidence.dpi,
            dpi_awareness: evidence.dpi_awareness, enabled: evidence.enabled, owned_visible: evidence.owned_visible,
        };
        if visit.foreground == window {
            self.observe_bounds(visit, snapshot.bounds, observed);
            self.elevated.foreground_busy = !snapshot.enabled || snapshot.owned_visible;
        } else if visit.foreground.process == window.process {
            let owned = &mut self.elevated.owned_windows;
            owned.retain(|observed| *observed != window.window);
            if snapshot.owned_visible {
                if owned.len() == WINDOW_NOTICES { owned.pop_front(); }
                owned.push_back(window.window);
            }
        }
        let mut row = self.window_evidence_row(window, identity, kind, snapshot);
        row["observed_qpc"] = json!(observed);
        row
    }

    fn queue_window_notice(&mut self, notice: Notice) {
        if self.elevated.windows.len() == WINDOW_NOTICES {
            self.losses.note(LossReason::NotificationOverflow, LossSpan::at(notice.receipt));
        } else {
            self.elevated.windows.push_back(notice);
        }
    }

    fn elevated_window_notice(
        &mut self, source: &mut Selector<'_>, visit: Visit, at: QpcTick, receipt: Receipt, rows: &mut Vec<Value>,
    ) -> Result<()> {
        while let Some(notice) = self.elevated.windows.pop_front() {
            let Some(kind) = window_notice_kind(notice) else { continue; };
            if !recent(receipt.clock, notice.receipt, at, NATIVE_BUDGET) {
                rows.push(native_status_row(kind, None, NativeStatus::TimedOut));
                continue;
            }
            let hwnd = ::windows::Win32::Foundation::HWND(notice.window.0 as usize as *mut _);
            let Some(window) = WindowProcess::read(hwnd).filter(|window| window.thread.get() == notice.thread
                && unsafe { GetAncestor(hwnd.0, GA_ROOT) } == hwnd.0) else { continue; };
            if self.approvals.get_at(window.process, notice.receipt).is_none() {
                continue;
            }
            if kind == WindowRowKind::WindowLocation {
                let now = Instant::now();
                if self.location_limited(window.window, now) {
                    self.mark_location_dirty(window.window);
                    continue;
                }
                self.rate_location(window.window, now);
            }
            if self.ordinary_process(window.process) {
                if let Some(mut row) = self.window_row(window, kind) {
                    event_timing(&mut row, notice, receipt)?;
                    rows.push(row);
                }
                continue;
            }
            if !self.request_window(source, visit, window, (kind, WindowCause::Notice(notice)), at, rows)? {
                break;
            }
        }
        Ok(())
    }

    /// Trailing edge of the location rate limit in an elevated visit: one
    /// re-read per dropped burst (C4). The collector reads one window at a
    /// time; the other due windows wait for a later poll.
    fn elevated_trailing_locations(
        &mut self, source: &mut Selector<'_>, visit: Visit, at: QpcTick, rows: &mut Vec<Value>,
    ) -> Result<()> {
        let now = Instant::now();
        let mut open = true;
        for window in self.due_locations(now) {
            if !open || self.elevated.window.is_some() {
                self.mark_location_dirty(window);
                continue;
            }
            let Some(proof) = read_window(window) else { continue };
            if self.approvals.get(proof.process).is_none() {
                continue;
            }
            if self.ordinary_process(proof.process) {
                if let Some(mut row) = self.window_row(proof, WindowRowKind::WindowLocation) {
                    row["trailing"] = json!(true);
                    self.rate_location(window, now);
                    rows.push(row);
                }
                continue;
            }
            self.rate_location(window, now);
            open = self.request_window(source, visit, proof, (WindowRowKind::WindowLocation, WindowCause::Trailing), at, rows)?;
        }
        Ok(())
    }

    /// Ask the collector to read the elevated `window` for a `kind` row. A
    /// window it cannot read is reported unavailable. Returns whether the
    /// collector can take another window read in this poll.
    fn request_window(
        &mut self, source: &mut Selector<'_>, visit: Visit, window: WindowProcess,
        (kind, cause): (WindowRowKind, WindowCause), at: QpcTick, rows: &mut Vec<Value>,
    ) -> Result<bool> {
        let Some((binding, _)) = visit.elevated else { return Ok(false); };
        let unavailable = || native_status_row(kind, Some(window.window), NativeStatus::Unavailable);
        let secondary = if window == visit.foreground { None } else { self.secondary_claim(window) };
        if window != visit.foreground && secondary.is_none() {
            rows.push(unavailable());
            return Ok(true);
        }
        self.elevated.window = request(source, binding, window.into(), at, Query::Window, secondary)?
            .map(|read| (read, kind, cause));
        if self.elevated.window.is_none() {
            rows.push(unavailable());
        }
        Ok(false)
    }

    pub(super) fn secondary_claim(&mut self, window: WindowProcess) -> Option<IdentityClaim> {
        let approval = self.approvals.get(window.process)?;
        let process = ProcessHandle::open_instance(window.process)?;
        let image = process.image_path()?;
        if !matches!(screen_fix_elevated::foreground_access(window.process, &image),
            Ok(screen_fix_elevated::ForegroundAccess::Elevated))
            || WindowProcess::read(window.hwnd()) != Some(window)
        {
            return None;
        }
        Some(IdentityClaim { target: window.into(), approval: ApprovalKey::new(approval.executable()).ok()?,
            image: ImagePath::new(&image).ok()? })
    }
}

/// The failure for a notice timestamp outside the session's time range: the
/// stamp came from the collector's evidence.
const TIMESTAMP_FAILURE: Failure = Failure::Protocol;

/// Add the notice timing to an elevated row (`add_win_event_timing`).
fn event_timing(row: &mut Value, notice: Notice, receipt: Receipt) -> Result<()> {
    add_win_event_timing(row, notice, receipt).map_err(|_| TIMESTAMP_FAILURE)
}

/// How an elevated window read ended without window evidence.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum NativeStatus {
    TimedOut,
    Unavailable,
    /// The window changed identity before its evidence arrived.
    Stale,
    /// The collector's or the Recorder's outcome of a rejected read.
    #[serde(untagged)]
    Rejected(ReadOutcome<()>),
}

/// The row of an elevated window read that ended with `status` instead of
/// window evidence. Only an approved `window` is named; a notice that timed
/// out before its window was checked names none.
fn native_status_row(kind: WindowRowKind, window: Option<WindowId>, status: NativeStatus) -> Value {
    match window {
        Some(window) => json!({ "kind": kind, "approved": true, "window": window, "native_status": status }),
        None => json!({ "kind": kind, "approved": false, "native_status": status }),
    }
}

fn valid_point(query: uia::Query, routes: &[MonitorGeometry]) -> bool {
    let uia::Query::Pointer { point, window, root, geometry, .. } = query else { return false; };
    let mut matching = routes.iter().filter(|route| route.bounds.contains(point));
    matching.next().is_some_and(|route| route.id == geometry)
        && matching.next().is_none() && uia::window_at(point) == Some(window)
        && uia::window_root(window) == Some(root)
}

fn clipboard_request(source: &mut Selector<'_>, visit: VisitBinding, at: QpcTick, expires: QpcTick)
    -> Result<Option<ReadBinding>>
{
    let deadline_qpc = source.read_deadline(at, NATIVE_BUDGET)?.min(expires);
    if deadline_qpc <= at { return Ok(None); }
    source.query(ReadBinding { visit, request: RequestId(0), target: visit.foreground,
        requested_qpc: at, deadline_qpc }, Query::ClipboardFence, None)
}

fn request(
    source: &mut Selector<'_>, visit: VisitBinding, target: WindowInstance, at: QpcTick,
    kind: impl FnOnce(ReadBinding) -> Query, secondary: Option<IdentityClaim>,
) -> Result<Option<ReadBinding>> {
    source.query(ReadBinding { visit, request: RequestId(0), target, requested_qpc: at,
        deadline_qpc: source.read_deadline(at, NATIVE_BUDGET)? }, kind, secondary)
}

fn fresh_read<T>(read: &ReadEvidence<T>, at: QpcTick) -> Option<(QpcTick, &T)> {
    let (start, end) = (read.started_qpc?, read.completed_qpc?);
    if start < read.binding.requested_qpc || end < start || end > at || at >= read.binding.deadline_qpc {
        return None;
    }
    match &read.outcome {
        ReadOutcome::Observed(value) => Some((end, value)),
        ReadOutcome::Unavailable(_) => None,
    }
}

fn authorized_notice(notice: Notice, visit: Visit, receipt: Receipt, at: QpcTick) -> bool {
    if notice.receipt < visit.started_at || notice.receipt > at { return false; }
    notice_interval(notice, receipt).is_some_and(|(lower, _)| lower >= visit.started_at && lower <= notice.receipt)
}

/// The window row kind of a notice the collector path reports: a known
/// top-level window event with a window and an owner thread.
fn window_notice_kind(notice: Notice) -> Option<WindowRowKind> {
    if notice.window.0 == 0 || notice.thread == 0 || notice.object != OBJID_WINDOW { return None; }
    window_event_kind(notice.event)
}

fn recent(clock: ClockOrigin, earlier: QpcTick, at: QpcTick, max: Duration) -> bool {
    earlier <= at && clock.time(earlier).ok().zip(clock.time(at).ok())
        .is_some_and(|(then, now)| i128::from(now.0.saturating_sub(then.0)) < max.as_micros() as i128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ipc::elevated::ReadUnavailable;

    /// The `native_status` rows of an elevated window read.
    #[test]
    fn native_status_bytes() -> serde_json::Result<()> {
        assert_eq!(serde_json::to_string(&native_status_row(WindowRowKind::WindowTitle, Some(WindowId(42)), NativeStatus::Stale))?,
            r#"{"approved":true,"kind":"window_title","native_status":"stale","window":42}"#);
        let outcome = ReadOutcome::<()>::Unavailable(ReadUnavailable::Hung);
        assert_eq!(serde_json::to_string(&native_status_row(WindowRowKind::Foreground, Some(WindowId(42)), NativeStatus::Rejected(outcome)))?,
            r#"{"approved":true,"kind":"foreground","native_status":{"status":"unavailable","value":"hung"},"window":42}"#);
        assert_eq!(serde_json::to_string(&native_status_row(WindowRowKind::Foreground, None, NativeStatus::TimedOut))?,
            r#"{"approved":false,"kind":"foreground","native_status":"timed_out"}"#);
        Ok(())
    }
}
