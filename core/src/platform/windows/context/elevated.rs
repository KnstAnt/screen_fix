use screen_fix_elevated::{Failure, Result};
use screen_fix_protocol::ipc::elevated::{
    CommitEvidence, ContextEvidence, Query, ReadBinding, ReadEvidence, ReadOutcome, WindowEvidence, WindowInstance,
    runtime::{ContextGapCause, ContextProducer, IdentityClaim, Observation},
};

use super::*;
use crate::protocol::ipc::elevated::{ImagePath, runtime::ApprovalKey};
use crate::{platform::windows::elevated::ContextDelivery, session::windows::ActiveSourceProof};

const NATIVE_BUDGET: Duration = Duration::from_millis(250);
const WINDOW_NOTICES: usize = 32;
const CLIPBOARD_READ_BUDGET: Duration = Duration::from_millis(100);
const IME_BUDGET: Duration = Duration::from_millis(50);
const COMMIT_CAPACITY: usize = screen_fix_native_observation::uia_commits::CAPACITY;

#[derive(Default)]
pub(super) struct Observations {
    focus: Option<ReadBinding>,
    focus_observed: Option<QpcTick>,
    window: Option<(ReadBinding, &'static str, Option<Notice>)>,
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

enum ClipboardStage {
    Pre { notice: clipboard::Notice, fence: ReadBinding },
    Read { notice: clipboard::Notice, pre: QpcTick, permit: clipboard::Permit },
    Post { read: clipboard::Read, fence: ReadBinding },
    Publish { read: clipboard::Read, fence_completed: QpcTick, deadline: QpcTick },
}

impl Context<'_> {
    pub fn certify_elevated_input(&mut self, certificate: InputCertificate) {
        if self.current.and_then(|visit| visit.elevated.map(|(binding, _)| binding)) == Some(certificate.visit) {
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
        let context_before = source.context_before();
        for notice in std::mem::take(&mut self.future_notices) {
            if !recent(receipt.clock, notice.receipt, at, NATIVE_BUDGET) { continue; }
            if context_before.is_none_or(|before| notice.receipt >= before) {
                self.future_notices.push(notice);
            } else if authorized_notice(notice, visit, receipt, at) {
                if notice.object == OBJID_WINDOW && matches!(notice.event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) {
                    self.elevated.owned_windows.retain(|window| *window != notice.window);
                }
                if window_notice_kind(notice).is_some() { self.queue_window_notice(notice); }
            }
        }
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
                    .map(|read| (read, "foreground_window", None));
            }
        }
        if self.elevated.ime_focus.is_some_and(|read| read.deadline_qpc <= at && !source.deferred_read(read)) {
            self.elevated.ime_focus = None;
            self.elevated_ime_focus(visit, ImeState::Unknown, None, None, receipt.clock, &mut rows);
        }
        self.poll_elevated_key(source, binding, focus, at)?;
        self.poll_elevated_clipboard(source, visit, at, receipt, &mut rows)?;
        self.queries.poll_remote_focus(source, at, receipt.clock, &mut rows)?;
        if let Some(point) = self.queries.pointer_rest(now) && let Some(geometry) = self.point_geometry(point) {
            self.pointer_query(point, geometry, receipt.clock, at, None).map_err(|_| Failure::Supervisor)?;
        }
        let routes = &self.routable;
        self.queries.poll_elevated_pointer(source, visit, at, receipt.clock,
            |query| valid_point(query, routes), &mut rows)?;
        if !self.elevated.foreground {
            self.elevated.foreground = true;
            rows.push(json!({ "kind": "keyboard_layout", "generation": visit.generation, "layout": focus.keyboard_layout }));
        }
        if self.elevated.focus.is_some_and(|read| read.deadline_qpc <= at && !source.deferred_read(read)) {
            self.elevated.focus = None;
        }
        if self.elevated.hung.is_some_and(|read| read.deadline_qpc <= at && !source.deferred_read(read)) {
            self.elevated.hung = None;
            rows.push(json!({ "kind": "app_hung_coverage", "complete": false, "source": "elevated" }));
        }
        if self.elevated.window.is_some_and(|(read, _, _)| read.deadline_qpc <= at && !source.deferred_read(read))
            && let Some((read, kind, _)) = self.elevated.window.take()
        {
            if read.target == binding.foreground { self.observe_bounds(visit, None, at); }
            rows.push(json!({ "kind": kind, "approved": true, "window": read.target.window, "native_status": "timed_out" }));
        }
        if self.elevated.window.is_none() {
            self.elevated_window_notice(source, visit, at, receipt, &mut rows)?;
        }
        let target = match self.queries.classification() {
            Classification::Resolved(target, _) => Some(target),
            Classification::Pending => None,
        };
        let process_hung = self.hung.contains(visit.foreground.process);
        let scoped_busy = self.elevated.foreground_busy || !self.elevated.owned_windows.is_empty();
        if let Some(change) = self.busy.refresh(self.current, process_hung,
            target, &notices, scoped_busy, at, receipt.clock)
        {
            rows.push(json!({ "kind": "app_busy", "approved": change.is_some(), "hint": change }));
        }
        if self.elevated.caret.is_some_and(|notice| !recent(receipt.clock, notice.receipt, at, NATIVE_BUDGET)) {
            self.elevated.caret = None;
        }
        if target == Some(TextTarget::Editable)
            && let Some(notice) = self.elevated.caret
            && let Some(snapshot) = self.current.and_then(|visit| visit.native)
            && self.elevated.focus_observed.is_some_and(|observed| observed >= notice.receipt)
            && notice.thread == snapshot.focus.thread.get()
            && (Some(notice.window) == snapshot.caret_window || notice.window == snapshot.focus.window)
            && self.confirm_focus(visit)
        {
            self.elevated.caret = None;
            let visible = notice.event != EVENT_OBJECT_HIDE && snapshot.caret.is_some();
            let change = match notice.event {
                EVENT_OBJECT_SHOW => "show",
                EVENT_OBJECT_HIDE => "hide",
                _ => "location",
            };
            let mut row = json!({ "kind": "caret", "generation": visit.generation, "change": change,
                "visible": visible, "bounds": visible.then_some(snapshot.caret).flatten() });
            add_win_event_timing(&mut row, notice, receipt).map_err(|_| Failure::Witness)?;
            rows.push(row);
        }
        self.sample_cursor(notices.iter().any(|notice| notice.event == EVENT_OBJECT_NAMECHANGE && notice.object == OBJID_CURSOR as i32));
        let mut changes = std::mem::take(&mut self.cursor_completions);
        changes.extend(self.cursor.results());
        for change in changes {
            if change.received() <= at {
                rows.push(change.row(receipt.clock));
            } else if self.cursor_completions.len() < 16 {
                self.cursor_completions.push(change);
            } else {
                self.losses.note(LossReason::StagingPressure, 1, change.received(), change.received());
            }
        }
        Ok(rows)
    }

    fn elevated_fact(
        &mut self, delivery: ContextDelivery, notices: &mut Vec<Notice>, rows: &mut Vec<Value>,
        visit: Visit, arrived: QpcTick, at: QpcTick, receipt: Receipt, admission: &ActiveSourceProof,
    ) -> Result<()> {
        let Some((binding, focus)) = visit.elevated else { return Ok(()); };
        let evidence = match delivery {
            ContextDelivery::Observation(Observation::Item { evidence, .. }) => evidence,
            ContextDelivery::Observation(Observation::Gap(gap)) if gap.visit == binding => {
                // A collector gap is a typed context loss per cause (E3).
                let reason = match (gap.producer, gap.cause) {
                    (ContextProducer::TextEdit, ContextGapCause::Overflow) => LossReason::CommitCapacity,
                    (ContextProducer::Notices, ContextGapCause::Overflow) => LossReason::NotificationOverflow,
                    (_, ContextGapCause::Overflow) => LossReason::QueueOverflow,
                    (_, ContextGapCause::Stale) => LossReason::LateCommit,
                    (_, ContextGapCause::CommitTooLong) => LossReason::CommitTooLong,
                    (_, ContextGapCause::CommitParts) => LossReason::CommitParts,
                    (_, ContextGapCause::CommitRejected) => LossReason::CommitRejected,
                };
                let count = gap.last_position.get().saturating_sub(gap.first_position.get()).saturating_add(1);
                self.losses.note(reason, count, gap.first_qpc, gap.last_qpc);
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
                let (_, kind, notice) = self.elevated.window.take().ok_or(Failure::Supervisor)?;
                let mut row = self.elevated_window_row(visit, kind, &read, arrived);
                if let Some(notice) = notice {
                    add_win_event_timing(&mut row, notice, receipt).map_err(|_| Failure::Witness)?;
                    row["source_event_tick_ms"] = json!(notice.source_tick);
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
                if self.elevated.clipboard.as_ref().is_some_and(|stage| match stage {
                    ClipboardStage::Pre { fence, .. } | ClipboardStage::Post { fence, .. } => *fence == read.binding,
                    _ => false,
                }) {
                    self.elevated.clipboard = None;
                }
                if self.elevated.ime_focus == Some(read.binding) {
                    self.elevated.ime_focus = None;
                    self.elevated_ime_focus(visit, ImeState::Unknown, read.started_qpc, read.completed_qpc, receipt.clock, rows);
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
                    if read.binding.target == binding.foreground { self.observe_bounds(visit, None, at); }
                    rows.push(json!({ "kind": kind, "approved": true, "window": read.binding.target.window,
                        "native_status": read.outcome }));
                }
            }
            _ => {}
        }
        Ok(())
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
                        let until = elapsed_deadline(pre, clock, CLIPBOARD_READ_BUDGET)?.min(expected.deadline_qpc);
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
                    elapsed_deadline(completed, clock, NATIVE_BUDGET)
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
        if let Some((count, first, last)) = worker.take_loss() {
            self.losses.note(LossReason::ClipboardQueue, count, first, last);
        }
        for notice in worker.notices() {
            if self.clipboard_notices.len() < 8 {
                self.clipboard_notices.push_back(notice);
            } else {
                self.losses.note(LossReason::ClipboardQueue, 1, notice.received, notice.received);
            }
        }
        self.clipboard_notices.retain(|notice| notice.observed.elapsed() < clipboard::MAX_AGE
            && notice.foreground == visit.foreground && notice.received >= visit.started_at
            && notice.received >= receipt.boundary.qpc);
        let safe = self.queries.elevated_clipboard_safe(binding) && visit.elevated_clipboard_focus_is_current();
        let mut results = std::mem::take(&mut self.future_clipboard);
        results.extend(worker.results());
        for read in results {
            if read.completed > at {
                if self.future_clipboard.len() < 8 {
                    self.future_clipboard.push(read);
                } else {
                    self.losses.note(LossReason::ClipboardQueue, 1, read.completed, read.completed);
                }
                continue;
            }
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

    fn elevated_commit(&mut self, commit: CommitEvidence, visit: Visit, arrived: QpcTick, at: QpcTick, clock: ClockOrigin) {
        if !self.queries.remote_commit(&commit, at, clock) || !self.confirm_focus(visit) {
            self.losses.note(LossReason::LateCommit, 1, commit.receipt_qpc, commit.receipt_qpc);
            return;
        }
        if self.elevated.commits.len() == COMMIT_CAPACITY {
            self.losses.note(LossReason::CommitCapacity, 1, commit.receipt_qpc, commit.receipt_qpc);
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
                // A commit that cannot be confirmed is counted (E3).
                self.losses.note(LossReason::LateCommit, 1, commit.callback_at, commit.callback_at);
            }
        }
        confirmed
    }

    fn elevated_window_row(
        &mut self, visit: Visit, kind: &'static str, read: &ReadEvidence<WindowEvidence>, at: QpcTick,
    ) -> Value {
        let target = read.binding.target;
        let Some((observed, evidence)) = fresh_read(read, at) else {
            if target == visit.foreground.into() { self.observe_bounds(visit, None, at); }
            return json!({ "kind": kind, "approved": true, "window": target.window, "native_status": "unavailable" });
        };
        let window = WindowProcess { window: target.window, process: target.process, thread: target.thread };
        if WindowProcess::read(window.hwnd()) != Some(window) {
            return json!({ "kind": kind, "approved": true, "window": target.window, "native_status": "stale" });
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
            self.losses.note(LossReason::NotificationOverflow, 1, notice.receipt, notice.receipt);
        } else {
            self.elevated.windows.push_back(notice);
        }
    }

    fn elevated_window_notice(
        &mut self, source: &mut Selector<'_>, visit: Visit, at: QpcTick, receipt: Receipt, rows: &mut Vec<Value>,
    ) -> Result<()> {
        let Some((binding, _)) = visit.elevated else { return Ok(()); };
        while let Some(notice) = self.elevated.windows.pop_front() {
            let Some(kind) = window_notice_kind(notice) else { continue; };
            if !recent(receipt.clock, notice.receipt, at, NATIVE_BUDGET) {
                rows.push(json!({ "kind": kind, "approved": false, "native_status": "timed_out" }));
                continue;
            }
            let hwnd = ::windows::Win32::Foundation::HWND(notice.window.0 as usize as *mut _);
            let Some(window) = WindowProcess::read(hwnd).filter(|window| window.thread.get() == notice.thread
                && unsafe { GetAncestor(hwnd.0, GA_ROOT) } == hwnd.0) else { continue; };
            if self.approvals.get_at(window.process, notice.receipt).is_none() {
                continue;
            }
            if kind == "window_location" {
                let now = Instant::now();
                if self.window_rate.iter().any(|(id, observed)| *id == window.window
                    && now.saturating_duration_since(*observed) < WINDOW_REFRESH) { continue; }
                self.window_rate.retain(|(id, _)| *id != window.window);
                if self.window_rate.len() == WINDOW_RATE_CACHE { self.window_rate.pop_front(); }
                self.window_rate.push_back((window.window, now));
            }
            if self.ordinary_process(window.process) {
                if let Some(mut row) = self.window_row(window, kind) {
                    add_win_event_timing(&mut row, notice, receipt).map_err(|_| Failure::Witness)?;
                    rows.push(row);
                }
                continue;
            }
            let secondary = if window == visit.foreground { None } else { self.secondary_claim(window) };
            if window != visit.foreground && secondary.is_none() {
                rows.push(json!({ "kind": kind, "approved": true, "window": window.window, "native_status": "unavailable" }));
                continue;
            }
            self.elevated.window = request(source, binding, window.into(), at, Query::Window, secondary)?
                .map(|read| (read, kind, Some(notice)));
            if self.elevated.window.is_none() {
                rows.push(json!({ "kind": kind, "approved": true, "window": window.window, "native_status": "unavailable" }));
            }
            break;
        }
        Ok(())
    }

    pub(super) fn secondary_claim(&mut self, window: WindowProcess) -> Option<IdentityClaim> {
        let approval = self.approvals.get(window.process)?;
        let process = ProcessHandle::open(window.process.pid).filter(|process|
            process.instance(window.process.pid) == Some(window.process))?;
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

pub(super) fn fresh_read<T>(read: &ReadEvidence<T>, at: QpcTick) -> Option<(QpcTick, &T)> {
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
    receipt.tick_anchor.zip(notice.source_tick).and_then(|(anchor, tick)|
        clock::message_tick_interval(anchor, receipt.clock.qpc_frequency, CoarseTime::windows_message(tick)))
        .is_some_and(|(lower, _)| lower >= visit.started_at && lower <= notice.receipt)
}

fn window_notice_kind(notice: Notice) -> Option<&'static str> {
    if notice.window.0 == 0 || notice.thread == 0 || notice.object != OBJID_WINDOW { return None; }
    match notice.event {
        EVENT_OBJECT_NAMECHANGE => Some("window_title"),
        EVENT_OBJECT_LOCATIONCHANGE => Some("window_location"),
        EVENT_OBJECT_SHOW => Some("window_shown"),
        EVENT_OBJECT_HIDE => Some("window_hidden"),
        _ => None,
    }
}

fn elapsed_deadline(at: QpcTick, clock: ClockOrigin, elapsed: Duration) -> Option<QpcTick> {
    let ticks = elapsed.as_nanos().checked_mul(u128::from(clock.qpc_frequency.0.get()))? / 1_000_000_000;
    at.0.checked_add(i64::try_from(ticks).ok()?).map(QpcTick)
}

fn recent(clock: ClockOrigin, earlier: QpcTick, at: QpcTick, max: Duration) -> bool {
    earlier <= at && clock.time(earlier).ok().zip(clock.time(at).ok())
        .is_some_and(|(then, now)| i128::from(now.0.saturating_sub(then.0)) < max.as_micros() as i128)
}
