use std::time::{Duration, Instant};

use serde_json::{Value, json};
use screen_fix_protocol::ipc::elevated::{
    CommitEvidence, PointBinding, Query as RemoteQuery, ReadBinding, ReadEvidence, ReadOutcome, ReadUnavailable, UiaEvidence,
    runtime::{IdentityClaim, Observation}, ContextEvidence,
};
use windows::Win32::{
    Foundation::POINT,
    UI::{HiDpi::{GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetWindowDpiAwarenessContext},
        WindowsAndMessaging::GetPhysicalCursorPos},
};

use crate::{
    capture::context::TextTarget,
    package::outcome::UiaOutcome,
    protocol::{FocusGeneration, GeometryId, PhysicalPoint, QpcTick, RequestId, ClockOrigin, ipc::ProcessInstance},
};

use super::{ImeState, Visit, hung::Hung,
    identity::{MetadataScrubber, WindowProcess},
    uia::{self, Query, Read, Request, Worker},
};
use super::super::{ProbeError, elevated::{ContextDelivery, Selector}, unavailable};

const POINTER_REST: Duration = Duration::from_millis(500);
const POINTER_POLL: Duration = Duration::from_millis(50);
/// A collector commit is accepted at most this long after its receipt.
const REMOTE_COMMIT_AGE_US: i64 = 500_000;
/// A completed pointer read serves a later action or rest at most this long after it.
const CACHED_POINTER_AGE_US: i64 = 500_000;

/// What a focus row adds beside its read (CTX-9): the cursor shape and the
/// focus thread's IME state, when already known.
pub(super) struct FocusExtras {
    pub cursor: Option<Value>,
    pub ime: Option<ImeState>,
}

#[derive(Clone, Copy)]
pub(super) enum Classification {
    Pending,
    Resolved(TextTarget, Option<QpcTick>),
}

#[derive(Clone, serde::Serialize)]
pub(in crate::platform::windows) struct PointerLink {
    /// None when no read can serve the action (`reason`).
    request_id: Option<RequestId>,
    age_us: Option<i64>,
    basis: PointerBasis,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum PointerBasis {
    PreAction,
    PostAction,
    PointerRest,
}

pub(super) enum PointerBackend {
    Ordinary,
    Elevated(Option<IdentityClaim>),
    Unavailable,
}

/// The last authorized pointer read, which can serve a later query at the same place.
#[derive(Clone, Copy)]
struct CachedPointer {
    point: PhysicalPoint,
    geometry: GeometryId,
    window: WindowProcess,
    root: WindowProcess,
    completed: QpcTick,
    request: RequestId,
}

pub(super) struct Queries {
    worker: Worker,
    generation: FocusGeneration,
    revision: u64,
    next_id: u64,
    focus: Option<Request>,
    focus_sent: bool,
    /// The focus read was re-issued after a stale result (D-4d).
    focus_retried: bool,
    remote_focus: Option<ReadBinding>,
    remote_edit: Option<(ReadBinding, QpcTick)>,
    remote: bool,
    classification: Classification,
    pointer: Option<Request>,
    pointer_sent: bool,
    pointer_backend: PointerBackend,
    remote_pointer: Option<ReadBinding>,
    last_point: Option<PhysicalPoint>,
    moved_at: Instant,
    pointer_rest_emitted: bool,
    next_pointer_poll: Instant,
    cached_pointer: Option<CachedPointer>,
    deferred_reads: Vec<Read>,
    denied_target: bool,
}

impl Queries {
    pub fn start(scrubber: MetadataScrubber) -> Result<Self, ProbeError> {
        let now = Instant::now();
        Ok(Self {
            worker: Worker::start(scrubber)?,
            generation: FocusGeneration(0), revision: 0, next_id: 0, focus: None, focus_sent: false, focus_retried: false, remote_focus: None, remote_edit: None, remote: false,
            classification: Classification::Resolved(TextTarget::PossiblySensitive, None),
            pointer: None, pointer_sent: false, pointer_backend: PointerBackend::Unavailable, remote_pointer: None, last_point: None, moved_at: now,
            pointer_rest_emitted: true, next_pointer_poll: now, cached_pointer: None, deferred_reads: Vec::new(), denied_target: false,
        })
    }

    pub fn stop_reads(&self) {
        self.worker.retain_generation(FocusGeneration(0));
    }

    pub fn update_hung(&mut self, hung: &Hung) {
        self.worker.update_hung(hung);
        if self.cached_pointer.is_some_and(|cached| self.blocked(cached.window.process).is_some()) {
            self.cached_pointer = None;
        }
    }

    pub fn blocked(&self, process: ProcessInstance) -> Option<ReadUnavailable> {
        self.worker.blocked(process)
    }

    pub fn changed(&self) -> bool {
        !self.remote && self.worker.focus_revision() != self.revision
    }

    pub fn classification(&self) -> Classification {
        if self.changed() { Classification::Pending } else { self.classification }
    }

    pub fn commits(&self) -> Vec<super::uia_commits::Commit> {
        self.worker.commits()
    }

    pub fn commit_losses(&self) -> Vec<screen_fix_native_observation::uia_commits::CommitLoss> {
        self.worker.commit_losses()
    }

    pub fn uia_failure(&self) -> Option<&'static str> {
        self.worker.failure()
    }

    pub fn accepts_commit(&self, generation: FocusGeneration, revision: u64) -> bool {
        !self.changed() && self.generation == generation && self.revision == revision
            && matches!(self.classification, Classification::Resolved(TextTarget::Editable, Some(_)))
    }

    pub fn accepts_remote_commit(&self, binding: ReadBinding, callback: QpcTick) -> bool {
        self.remote_edit.is_some_and(|(read, opened)| read == binding && callback >= opened)
            && matches!(self.classification(), Classification::Resolved(TextTarget::Editable, Some(_)))
    }

    pub fn remote_commit(&self, commit: &CommitEvidence, at: QpcTick, clock: ClockOrigin) -> bool {
        commit.receipt_qpc <= at && self.accepts_remote_commit(commit.binding, commit.receipt_qpc)
            && clock.time(commit.receipt_qpc).ok().zip(clock.time(at).ok())
                .is_some_and(|(then, now)| now.0.saturating_sub(then.0) < REMOTE_COMMIT_AGE_US)
    }

    pub fn clipboard_safe(&self) -> bool {
        !self.changed() && matches!(self.classification,
            Classification::Resolved(TextTarget::Editable | TextTarget::NonText | TextTarget::TypingUnknown, Some(_)))
    }

    pub fn elevated_clipboard_safe(&self, visit: screen_fix_protocol::ipc::elevated::VisitBinding) -> bool {
        self.remote_edit.is_some_and(|(read, _)| read.visit == visit)
            && matches!(self.classification(), Classification::Resolved(TextTarget::Editable, Some(_)))
    }

    pub fn clipboard_guard(&self) -> uia::FocusGuard {
        self.worker.focus_guard(self.generation, self.revision)
    }

    pub fn focus(&mut self, generation: FocusGeneration, visit: Option<Visit>, now: Instant) -> Result<(), ProbeError> {
        self.generation = generation;
        self.revision = self.worker.focus_revision();
        self.worker.retain_generation(generation);
        self.focus_sent = false;
        self.focus_retried = false;
        self.remote_focus = None;
        self.remote_edit = None;
        self.remote = visit.is_some_and(|visit| visit.elevated.is_some());
        self.cached_pointer = None;
        self.pointer = None;
        self.remote_pointer = None;
        self.pointer_rest_emitted = true;
        self.classification = if visit.is_some() {
            Classification::Pending
        } else {
            Classification::Resolved(TextTarget::PossiblySensitive, None)
        };
        self.focus = visit.map(|visit| self.request(Query::Focus(visit), visit.started_at, now)).transpose()?;
        Ok(())
    }

    fn request(&mut self, query: Query, requested: QpcTick, now: Instant) -> Result<Request, ProbeError> {
        self.next_id = self.next_id.checked_add(1)
            .ok_or_else(|| unavailable("The UI Automation request ID counter overflowed"))?;
        Ok(Request {
            id: RequestId(self.next_id), generation: self.generation, focus_revision: self.revision,
            requested, expires: now + uia::DEADLINE, query,
        })
    }

    pub fn pointer_rest(&mut self, now: Instant) -> Option<PhysicalPoint> {
        if now < self.next_pointer_poll {
            return None;
        }
        self.next_pointer_poll = now + POINTER_POLL;
        let mut point = POINT::default();
        let point = unsafe { GetPhysicalCursorPos(&mut point) }.is_ok().then_some(PhysicalPoint { x: point.x, y: point.y });
        if point != self.last_point {
            self.last_point = point;
            self.moved_at = now;
            self.pointer_rest_emitted = false;
        }
        if self.pointer_rest_emitted || now.saturating_duration_since(self.moved_at) < POINTER_REST {
            return None;
        }
        self.pointer_rest_emitted = true;
        point
    }

    pub fn deny_pointer(&mut self) { self.denied_target = true; }

    pub fn pointer(
        &mut self, point: PhysicalPoint, geometry: GeometryId, visit: Visit, window: WindowProcess, root: WindowProcess,
        backend: PointerBackend, clock: ClockOrigin, at: QpcTick, action_at: Option<QpcTick>,
    ) -> Result<Option<PointerLink>, ProbeError> {
        if self.changed() || self.generation != visit.generation {
            return Ok(None);
        }
        if !matches!(backend, PointerBackend::Unavailable)
            && let Some(cached) = self.cached_pointer
        {
            let reference = action_at.unwrap_or(at);
            if cached.point == point && cached.geometry == geometry && cached.window == window && cached.root == root
                && cached.completed <= reference
                && let Some(age) = clock.time(reference).ok().zip(clock.time(cached.completed).ok())
                    .map(|(reference, completed)| reference.0.saturating_sub(completed.0))
                    .filter(|age| *age < CACHED_POINTER_AGE_US)
            {
                return Ok(Some(PointerLink { request_id: Some(cached.request), age_us: Some(age),
                    basis: if action_at.is_some() { PointerBasis::PreAction } else { PointerBasis::PointerRest }, reason: None }));
            }
        }
        if let Some(pending) = self.pointer {
            // C4: an action at the pending read's point links that read, which
            // completes within its own deadline, before the action's. An action
            // elsewhere cannot be read now.
            let same = matches!(pending.query, Query::Pointer { point: pending_point, geometry: pending_geometry, .. }
                if pending_point == point && pending_geometry == geometry);
            return Ok(action_at.map(|_| PointerLink {
                request_id: same.then_some(pending.id), age_us: None, basis: PointerBasis::PreAction,
                reason: (!same).then_some("pointer_query_busy"),
            }));
        }
        let mut request = self.request(Query::Pointer { point, window, root, geometry, action_at }, at, Instant::now())?;
        if self.remote && matches!(backend, PointerBackend::Ordinary) {
            request.focus_revision = self.worker.focus_revision();
        }
        self.pointer_backend = backend;
        self.pointer = Some(request);
        self.worker.retain_pointer(Some(request.id));
        self.pointer_sent = false;
        Ok(Some(PointerLink { request_id: Some(request.id), age_us: None,
            basis: if action_at.is_some() { PointerBasis::PostAction } else { PointerBasis::PointerRest }, reason: None }))
    }

    pub fn poll(&mut self, now: Instant, at: QpcTick, clock: ClockOrigin, extras: Option<&FocusExtras>, rows: &mut Vec<Value>) {
        self.poll_local_results(at, clock, extras, |_| true, rows);
        if let Some(request) = self.focus {
            if let Some(status) = self.unfinished_status(request, now) {
                self.finish(Read::unavailable(request, Some(at), status), clock, extras, rows);
            } else if !self.focus_sent && at >= request.requested {
                self.focus_sent = self.worker.submit(request);
            }
        }
        self.poll_local_pointer(now, at, clock, rows);
    }

    fn poll_local_results(
        &mut self, at: QpcTick, clock: ClockOrigin, extras: Option<&FocusExtras>,
        valid: impl Fn(Query) -> bool, rows: &mut Vec<Value>,
    ) {
        if std::mem::take(&mut self.denied_target) {
            rows.push(json!({ "kind": "element", "approved": false }));
        }
        // The caller samples the pass watermark before polling. Keep any
        // just-completed result until its actual completion is in the pass.
        let mut readable = std::mem::take(&mut self.deferred_reads);
        readable.extend(self.worker.results());
        for mut read in readable {
            if !valid(read.request.query) {
                read.status = Err(ReadUnavailable::Stale);
                read.element = None;
            }
            if read.completed.is_some_and(|completed| completed > at) {
                self.deferred_reads.push(read);
            } else {
                self.finish(read, clock, extras, rows);
            }
        }
    }

    fn poll_local_pointer(&mut self, now: Instant, at: QpcTick, clock: ClockOrigin, rows: &mut Vec<Value>) {
        if let Some(request) = self.pointer {
            let status = match self.pointer_backend {
                PointerBackend::Ordinary => self.unfinished_status(request, now),
                PointerBackend::Unavailable => Some(ReadUnavailable::Unavailable),
                PointerBackend::Elevated(_) => return,
            };
            if let Some(status) = status {
                self.finish(Read::unavailable(request, Some(at), status), clock, None, rows);
            } else if !self.pointer_sent && at >= request.requested {
                self.pointer_sent = self.worker.submit(request);
            }
        }
    }

    fn finish_unavailable(
        &mut self, request: Request, status: ReadUnavailable, clock: ClockOrigin, extras: Option<&FocusExtras>, rows: &mut Vec<Value>,
    ) {
        self.finish(Read::unavailable(request, None, status), clock, extras, rows);
    }

    pub fn remote_focus_result(
        &mut self, observation: &ContextDelivery, at: QpcTick, clock: ClockOrigin, rows: &mut Vec<Value>,
    ) {
        let (Some(expected), Some(request)) = (self.remote_focus, self.focus) else { return; };
        let read = match observation {
            ContextDelivery::Observation(Observation::Item { evidence: ContextEvidence::FocusUia(read), .. })
                if read.binding == expected => read,
            ContextDelivery::Rejected(binding, reason) if *binding == expected => {
                self.finish_unavailable(request, *reason, clock, None, rows);
                self.remote_focus = None;
                return;
            }
            ContextDelivery::Observation(Observation::Item { evidence: ContextEvidence::Rejected(read), .. })
                if read.binding == expected => {
                if let ReadOutcome::Unavailable(reason) = &read.outcome {
                    self.finish_unavailable(request, *reason, clock, None, rows);
                    self.remote_focus = None;
                }
                return;
            }
            _ => return,
        };
        if at >= expected.deadline_qpc || Instant::now() >= request.expires { return; }
        if let Some(read) = remote_read(request, read, at) {
            let enabled = read.target == TextTarget::Editable
                && matches!(request.query, Query::Focus(visit)
                    if visit.receipt_proof().focus.is_some_and(|focus| focus.password_style == Some(false)));
            let edit = read.completed.filter(|_| enabled);
            self.finish(read, clock, None, rows);
            self.remote_edit = edit.map(|completed| (expected, completed));
            self.remote_focus = None;
        }
    }

    pub fn poll_remote_focus(
        &mut self, source: &mut Selector<'_>, at: QpcTick, clock: ClockOrigin, rows: &mut Vec<Value>,
    ) -> screen_fix_elevated::Result<()> {
        let Some(request) = self.focus else { return Ok(()); };
        let Query::Focus(visit) = request.query else { return Ok(()); };
        let Some((binding, expected)) = visit.elevated else { return Ok(()); };
        let remaining = request.expires.saturating_duration_since(Instant::now());
        if remaining.is_zero() || self.remote_focus.is_some_and(|read| at >= read.deadline_qpc) {
            self.finish_unavailable(request, ReadUnavailable::TimedOut, clock, None, rows);
            self.remote_focus = None;
        } else if !self.focus_sent {
            let read = ReadBinding {
                visit: binding, request: request.id, target: binding.foreground,
                requested_qpc: at, deadline_qpc: source.read_deadline(at, remaining)?,
            };
            if let Some(read) = source.query(read, |read| RemoteQuery::Focus { read, expected }, None)? {
                self.remote_focus = Some(read);
                self.focus_sent = true;
            }
        }
        Ok(())
    }

    pub fn remote_pointer_result(
        &mut self, delivery: &ContextDelivery, at: QpcTick, clock: ClockOrigin,
        valid: impl Fn(Query) -> bool, rows: &mut Vec<Value>,
    ) {
        let (Some(expected), Some(request)) = (self.remote_pointer, self.pointer) else { return; };
        let Query::Pointer { point, root, geometry, action_at, .. } = request.query else { return; };
        let read = match delivery {
            ContextDelivery::Observation(Observation::Item {
                evidence: ContextEvidence::PointerUia { point: returned, read }, ..
            }) if *returned == (PointBinding { point, root: root.into(), geometry, action_qpc: action_at }) && read.binding == expected => read,
            ContextDelivery::Rejected(read, reason) if *read == expected => {
                self.finish_unavailable(request, *reason, clock, None, rows);
                return;
            }
            ContextDelivery::Observation(Observation::Item { evidence: ContextEvidence::Rejected(read), .. })
                if read.binding == expected => {
                if let ReadOutcome::Unavailable(reason) = &read.outcome {
                    self.finish_unavailable(request, *reason, clock, None, rows);
                }
                return;
            }
            _ => return,
        };
        if at >= expected.deadline_qpc || Instant::now() >= request.expires { return; }
        if !valid(request.query) {
            self.finish_unavailable(request, ReadUnavailable::Stale, clock, None, rows);
        } else if let Some(read) = remote_read(request, read, at) {
            self.finish(read, clock, None, rows);
        }
    }

    pub fn poll_elevated_pointer(
        &mut self, source: &mut Selector<'_>, visit: Visit, at: QpcTick, clock: ClockOrigin,
        valid: impl Fn(Query) -> bool, rows: &mut Vec<Value>,
    ) -> screen_fix_elevated::Result<()> {
        self.poll_local_results(at, clock, None, &valid, rows);
        let now = Instant::now();
        let Some(request) = self.pointer else { return Ok(()); };
        let Query::Pointer { point, window, root, geometry, action_at } = request.query else { return Ok(()); };
        if !valid(request.query) && !matches!(self.pointer_backend, PointerBackend::Unavailable) {
            self.finish_unavailable(request, ReadUnavailable::Stale, clock, None, rows);
            return Ok(());
        }
        let PointerBackend::Elevated(secondary) = &self.pointer_backend else {
            self.poll_local_pointer(now, at, clock, rows);
            return Ok(());
        };
        let Some((visit, _)) = visit.elevated else { return Ok(()); };
        let remaining = request.expires.saturating_duration_since(now);
        if remaining.is_zero() || self.remote_pointer.is_some_and(|read| at >= read.deadline_qpc) {
            self.finish_unavailable(request, ReadUnavailable::TimedOut, clock, None, rows);
        } else if !self.pointer_sent {
            let read = ReadBinding { visit, request: RequestId(0), target: window.into(), requested_qpc: at,
                deadline_qpc: source.read_deadline(at, remaining)? };
            let point = PointBinding { point, root: root.into(), geometry, action_qpc: action_at };
            if let Some(read) = source.query(read, |read| RemoteQuery::Pointer { read, point }, secondary.clone())? {
                self.remote_pointer = Some(read);
                self.pointer_sent = true;
            }
        }
        Ok(())
    }

    fn unfinished_status(&self, request: Request, now: Instant) -> Option<ReadUnavailable> {
        if let Some(status) = self.blocked(request.query.process()) {
            Some(status)
        } else if matches!(request.query, Query::Focus(visit) if visit.native.is_some_and(|native| native.hung)) {
            Some(ReadUnavailable::Hung)
        } else if now >= request.expires {
            Some(ReadUnavailable::TimedOut)
        } else {
            (!self.worker.running()).then_some(ReadUnavailable::Unavailable)
        }
    }

    fn finish(&mut self, mut read: Read, clock: ClockOrigin, extras: Option<&FocusExtras>, rows: &mut Vec<Value>) {
        let local_pointer = matches!(read.request.query, Query::Pointer { .. })
            && matches!(self.pointer_backend, PointerBackend::Ordinary);
        if read.request.generation != self.generation || self.changed()
            || (!self.remote && read.request.focus_revision != self.revision)
        {
            return;
        }
        if self.remote && local_pointer && read.request.focus_revision != self.worker.focus_revision() {
            read.status = Err(ReadUnavailable::Stale);
            read.element = None;
        }
        if (!self.remote || local_pointer) && let Some(status) = self.blocked(read.request.query.process()) {
            read.status = Err(status);
            read.element = None;
        }
        let authorized_result = read.status.is_ok();
        let time = |qpc| clock.time(qpc).ok();
        let row = match read.request.query {
            Query::Focus(visit) if self.focus.is_some_and(|request| request.id == read.request.id) => {
                // A stale read, such as an element without keyboard focus, is
                // re-issued once within the same classification wait (D-4d).
                if read.status == Err(ReadUnavailable::Stale) && !self.focus_retried && Instant::now() < read.request.expires
                    && let Ok(mut retry) = self.request(read.request.query, read.request.requested, Instant::now())
                {
                    retry.expires = read.request.expires;
                    self.focus = Some(retry);
                    self.focus_sent = false;
                    self.focus_retried = true;
                    return;
                }
                self.focus = None;
                if !authorized_result { read.target = TextTarget::PossiblySensitive; }
                self.classification = Classification::Resolved(read.target, read.completed);
                json!({
                    "kind": "focus", "window": visit.foreground.window, "target": read.target,
                    "cursor": extras.and_then(|extras| extras.cursor.as_ref()), "ime": extras.and_then(|extras| extras.ime),
                    "caret": (read.target == TextTarget::Editable).then(|| visit.native.and_then(|native| native.caret)).flatten(),
                    "layout": visit.receipt_proof().focus.map(|focus| focus.keyboard_layout),
                    "dpi": if visit.is_elevated() { json!(read.scoped_dpi.and_then(|(dpi, _)| dpi)) }
                        else { json!(unsafe { GetDpiForWindow(visit.foreground.hwnd()) }) },
                    "dpi_awareness": if visit.is_elevated() { read.scoped_dpi.and_then(|(_, awareness)| awareness) }
                        else { Some(unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(visit.foreground.hwnd())).0 }) },
                })
            }
            Query::Pointer { point, window, root, geometry, action_at }
                if self.pointer.is_some_and(|request| request.id == read.request.id) =>
            {
                self.pointer = None;
                self.remote_pointer = None;
                self.worker.retain_pointer(None);
                if read.status == Err(ReadUnavailable::OutsideApproval) {
                    rows.push(json!({ "kind": "element", "approved": false }));
                    return;
                }
                if authorized_result && let Some(completed) = read.completed {
                    self.cached_pointer = Some(CachedPointer { point, geometry, window, root, completed, request: read.request.id });
                }
                json!({
                    "kind": "element", "trigger": if action_at.is_some() { "pointer_down" } else { "pointer_rest" },
                    "pos": point, "window": window.window, "geometry": geometry,
                    "action_t_us": action_at.and_then(time),
                    "post_action": read.started.zip(action_at).map(|(started, action)| started >= action),
                    "action_to_completion_us": read.completed.and_then(time).zip(action_at.and_then(time))
                        .filter(|(completed, action)| completed >= action)
                        .map(|(completed, action)| completed.0.saturating_sub(action.0)),
                    "dpi": if local_pointer { json!(unsafe { GetDpiForWindow(window.hwnd()) }) }
                        else { json!(read.scoped_dpi.and_then(|(dpi, _)| dpi)) },
                    "dpi_awareness": if local_pointer {
                        Some(unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(window.hwnd())).0 })
                    } else { read.scoped_dpi.and_then(|(_, awareness)| awareness) },
                })
            }
            _ => return,
        };
        let mut row = row;
        row["approved"] = true.into();
        row["request_id"] = json!(read.request.id);
        row["generation"] = json!(read.request.generation);
        row["requested_t_us"] = json!(time(read.request.requested));
        row["started_t_us"] = json!(read.started.and_then(time));
        row["completed_t_us"] = json!(read.completed.and_then(time));
        row["uia"] = uia_value(read.status, read.element.as_ref());
        rows.push(row);
    }
}

/// The `uia` value of a focus or element row (CTX-9, CTX-10): the element of
/// a ready read, otherwise the read's outcome, such as `"timeout"`.
fn uia_value(status: Result<(), ReadUnavailable>, element: Option<&uia::Element>) -> Value {
    match (status, element) {
        (Err(reason), _) => json!(UiaOutcome::of(reason)),
        (Ok(()), Some(element)) => json!({
            "control": element.control_type.and_then(control_name), "control_type_id": element.control_type,
            "framework": element.framework, "editable": element.editable, "password": element.password,
            "name": element.name, "rect": element.bounds, "automation_id": element.automation_id,
            "class": element.class, "ancestor_names": element.ancestor_names,
        }),
        (Ok(()), None) => Value::Null,
    }
}

/// The programmatic name of a UI Automation control type identifier.
fn control_name(id: i32) -> Option<&'static str> {
    const NAMES: [&str; 41] = [
        "Button", "Calendar", "CheckBox", "ComboBox", "Edit", "Hyperlink", "Image", "ListItem", "List", "Menu",
        "MenuBar", "MenuItem", "ProgressBar", "RadioButton", "ScrollBar", "Slider", "Spinner", "StatusBar", "Tab",
        "TabItem", "Text", "ToolBar", "ToolTip", "Tree", "TreeItem", "Custom", "Group", "Thumb", "DataGrid",
        "DataItem", "Document", "SplitButton", "Window", "Pane", "Header", "HeaderItem", "Table", "TitleBar",
        "Separator", "SemanticZoom", "AppBar",
    ];
    // UIA_ButtonControlTypeId (50000) through UIA_AppBarControlTypeId (50040).
    usize::try_from(id.checked_sub(50_000)?).ok().and_then(|index| NAMES.get(index).copied())
}

fn remote_read(request: Request, read: &ReadEvidence<UiaEvidence>, at: QpcTick) -> Option<Read> {
    let started = read.started_qpc;
    let completed = read.completed_qpc;
    if completed.is_some_and(|end| end > at || end >= read.binding.deadline_qpc)
        || started.is_some_and(|start| start < read.binding.requested_qpc || completed.is_none_or(|end| start > end))
    {
        return None;
    }
    let (status, target, element, scoped_dpi) = match &read.outcome {
        ReadOutcome::Observed(evidence) if started.is_some() && completed.is_some() => {
            use screen_fix_protocol::ipc::elevated::TextClassification;
            let source = &evidence.element;
            let native_password = match request.query {
                Query::Focus(visit) => visit.receipt_proof().focus.and_then(|focus| focus.password_style),
                Query::Pointer { .. } => None,
            };
            let password = if native_password == Some(true) { Some(true) } else { source.password };
            let focus_safe = matches!(request.query, Query::Pointer { .. }) || native_password == Some(false);
            let target = match (evidence.target, password, source.editable) {
                (TextClassification::Password, _, _) | (_, Some(true), _) => TextTarget::Password,
                (TextClassification::Editable, Some(false), Some(true)) if focus_safe => TextTarget::Editable,
                (TextClassification::NonText, Some(false), Some(false)) if focus_safe => TextTarget::NonText,
                (TextClassification::TypingUnknown, Some(false), None) if focus_safe => TextTarget::TypingUnknown,
                _ => TextTarget::PossiblySensitive,
            };
            let strings = password == Some(false)
                && (matches!(request.query, Query::Pointer { .. })
                    || native_password == Some(false) && target != TextTarget::PossiblySensitive);
            let permitted = |value: &Option<screen_fix_protocol::ipc::elevated::Metadata>| {
                value.as_ref().filter(|_| strings).map(|value| value.as_str().to_owned())
            };
            let element = uia::Element {
                name: permitted(&source.name),
                automation_id: permitted(&source.automation_id),
                class: permitted(&source.class),
                framework: source.framework.as_ref().map(|value| value.as_str().to_owned()),
                control_type: source.control_type, bounds: source.bounds,
                ancestor_names: source.ancestor_names.as_slice().iter().filter(|_| strings)
                    .map(|name| name.as_str().to_owned()).collect(),
                password, editable: source.editable.filter(|_| password == Some(false)),
            };
            (Ok(()), target, Some(element), Some((evidence.dpi, evidence.dpi_awareness)))
        }
        ReadOutcome::Observed(_) => return None,
        ReadOutcome::Unavailable(reason) => (Err(*reason), TextTarget::PossiblySensitive, None, None),
    };
    Some(Read { request, started, completed, status, target, element, scoped_dpi })
}
