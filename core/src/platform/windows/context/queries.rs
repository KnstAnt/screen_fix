use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows::Win32::{
    Foundation::POINT,
    UI::{HiDpi::{GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetWindowDpiAwarenessContext},
        WindowsAndMessaging::GetPhysicalCursorPos},
};

use crate::{
    capture::context::TextTarget,
    protocol::{FocusGeneration, GeometryId, PhysicalPoint, QpcTick, RequestId, ClockOrigin, ipc::ProcessInstance},
};

use super::{Visit, hung::Hung,
    identity::{ApprovedProcesses, MetadataScrubber, WindowProcess},
    uia::{self, Query, Read, Request, Status, Worker},
};
use super::super::{ProbeError, unavailable};

const POINTER_REST: Duration = Duration::from_millis(500);
const POINTER_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
pub(super) enum Classification {
    Pending,
    Resolved(TextTarget, Option<QpcTick>),
}

#[derive(serde::Serialize)]
pub(in crate::platform::windows) struct PointerLink {
    request_id: RequestId,
    age_us: Option<i64>,
    basis: PointerBasis,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum PointerBasis {
    PreAction,
    PostAction,
    PointerRest,
}

pub(super) struct Queries {
    worker: Worker,
    generation: FocusGeneration,
    revision: u64,
    next_id: u64,
    focus: Option<Request>,
    focus_sent: bool,
    classification: Classification,
    pointer: Option<Request>,
    pointer_sent: bool,
    last_point: Option<PhysicalPoint>,
    moved_at: Instant,
    pointer_rest_emitted: bool,
    next_pointer_poll: Instant,
    cached_pointer: Option<(PhysicalPoint, GeometryId, WindowProcess, QpcTick, RequestId)>,
    deferred_reads: Vec<Read>,
    denied_target: bool,
}

impl Queries {
    pub fn start(scrubber: MetadataScrubber) -> Result<Self, ProbeError> {
        let now = Instant::now();
        Ok(Self {
            worker: Worker::start(scrubber)?,
            generation: FocusGeneration(0), revision: 0, next_id: 0, focus: None, focus_sent: false,
            classification: Classification::Resolved(TextTarget::PossiblySensitive, None),
            pointer: None, pointer_sent: false, last_point: None, moved_at: now,
            pointer_rest_emitted: true, next_pointer_poll: now, cached_pointer: None, deferred_reads: Vec::new(), denied_target: false,
        })
    }

    pub fn stop_reads(&self) {
        self.worker.retain_generation(FocusGeneration(0));
    }

    pub fn update_hung(&mut self, hung: &Hung) {
        self.worker.update_hung(hung);
        if self.cached_pointer.is_some_and(|(_, _, window, _, _)| self.blocked(window.process).is_some()) {
            self.cached_pointer = None;
        }
    }

    pub fn blocked(&self, process: ProcessInstance) -> Option<Status> {
        self.worker.blocked(process)
    }

    pub fn changed(&self) -> bool {
        self.worker.focus_revision() != self.revision
    }

    pub fn classification(&self) -> Classification {
        if self.changed() { Classification::Pending } else { self.classification }
    }

    pub fn commits(&self) -> Vec<super::uia_commits::Commit> {
        self.worker.commits()
    }

    pub fn accepts_commit(&self, generation: FocusGeneration, revision: u64) -> bool {
        !self.changed() && self.generation == generation && self.revision == revision
            && matches!(self.classification, Classification::Resolved(TextTarget::Editable, Some(_)))
    }

    pub fn clipboard_safe(&self) -> bool {
        !self.changed() && matches!(self.classification,
            Classification::Resolved(TextTarget::Editable | TextTarget::NonText, Some(_)))
    }

    pub fn clipboard_guard(&self) -> uia::FocusGuard {
        self.worker.focus_guard(self.generation, self.revision)
    }

    pub fn focus(&mut self, generation: FocusGeneration, visit: Option<Visit>, now: Instant) -> Result<(), ProbeError> {
        self.generation = generation;
        self.revision = self.worker.focus_revision();
        self.worker.retain_generation(generation);
        self.focus_sent = false;
        self.cached_pointer = None;
        self.pointer = None;
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

    pub fn pointer(
        &mut self, point: PhysicalPoint, geometry: GeometryId, visit: Option<Visit>, approvals: &mut ApprovedProcesses<'_>,
        clock: ClockOrigin, at: QpcTick, action_at: Option<QpcTick>,
    ) -> Result<Option<PointerLink>, ProbeError> {
        let Some(window) = uia::window_at(point).filter(|window| approvals.get(window.process).is_some()) else {
            self.denied_target = true;
            return Ok(None);
        };
        let Some(visit) = visit else { return Ok(None) };
        if self.changed() || self.generation != visit.generation || self.blocked(window.process).is_some() {
            return Ok(None);
        }
        if let Some((cached_point, cached_geometry, cached_window, completed, id)) = self.cached_pointer {
            let reference = action_at.unwrap_or(at);
            if cached_point == point && cached_geometry == geometry && cached_window == window
                && completed <= reference
                && let Some(age) = clock.time(reference).ok().zip(clock.time(completed).ok())
                    .map(|(reference, completed)| reference.0.saturating_sub(completed.0))
                    .filter(|age| *age < 500_000)
            {
                return Ok(Some(PointerLink { request_id: id, age_us: Some(age),
                    basis: if action_at.is_some() { PointerBasis::PreAction } else { PointerBasis::PointerRest } }));
            }
        }
        if self.pointer.is_some() {
            return Ok(None);
        }
        let request = self.request(Query::Pointer { point, window, geometry, action_at }, at, Instant::now())?;
        self.pointer = Some(request);
        self.worker.retain_pointer(Some(request.id));
        self.pointer_sent = false;
        Ok(Some(PointerLink { request_id: request.id, age_us: None,
            basis: if action_at.is_some() { PointerBasis::PostAction } else { PointerBasis::PointerRest } }))
    }

    pub fn poll(&mut self, now: Instant, at: QpcTick, clock: ClockOrigin, cursor: Option<&Value>, rows: &mut Vec<Value>) {
        if std::mem::take(&mut self.denied_target) {
            rows.push(json!({ "kind": "pointer_element", "approved": false }));
        }
        // The caller samples the pass watermark before polling. Keep any
        // just-completed result until its actual completion is in the pass.
        let mut readable = std::mem::take(&mut self.deferred_reads);
        readable.extend(self.worker.results());
        for read in readable {
            if read.completed.is_some_and(|completed| completed > at) {
                self.deferred_reads.push(read);
            } else {
                self.finish(read, clock, cursor, rows);
            }
        }
        if let Some(request) = self.focus {
            if let Some(status) = self.unfinished_status(request, now) {
                self.finish(Read {
                    request, started: None, completed: Some(at), status, target: TextTarget::PossiblySensitive, element: None,
                }, clock, cursor, rows);
            } else if !self.focus_sent && at >= request.requested {
                self.focus_sent = self.worker.submit(request);
            }
        }
        if let Some(request) = self.pointer {
            if let Some(status) = self.unfinished_status(request, now) {
                self.finish(Read {
                    request, started: None, completed: Some(at), status, target: TextTarget::PossiblySensitive, element: None,
                }, clock, cursor, rows);
            } else if !self.pointer_sent && at >= request.requested {
                self.pointer_sent = self.worker.submit(request);
            }
        }
    }

    fn unfinished_status(&self, request: Request, now: Instant) -> Option<Status> {
        if let Some(status) = self.blocked(request.query.process()) {
            Some(status)
        } else if matches!(request.query, Query::Focus(visit) if visit.native.is_some_and(|native| native.hung)) {
            Some(Status::Hung)
        } else if now >= request.expires {
            Some(Status::TimedOut)
        } else {
            (!self.worker.running()).then_some(Status::Unavailable)
        }
    }

    fn finish(&mut self, mut read: Read, clock: ClockOrigin, cursor: Option<&Value>, rows: &mut Vec<Value>) {
        if read.request.generation != self.generation || read.request.focus_revision != self.revision || self.changed() {
            return;
        }
        if let Some(status) = self.blocked(read.request.query.process()) {
            read.status = status;
            read.element = None;
        }
        let authorized_result = matches!(read.status, Status::Ready);
        let time = |qpc| clock.time(qpc).ok();
        let row = match read.request.query {
            Query::Focus(visit) if self.focus.is_some_and(|request| request.id == read.request.id) => {
                self.focus = None;
                if !authorized_result { read.target = TextTarget::PossiblySensitive; }
                self.classification = Classification::Resolved(read.target, read.completed);
                json!({
                    "kind": "focus", "window": visit.foreground.window, "target": read.target, "cursor": cursor,
                    "caret": (read.target == TextTarget::Editable).then(|| visit.native.and_then(|native| native.caret)).flatten(),
                    "layout": visit.native.map(|native| native.focus.keyboard_layout),
                    "dpi": unsafe { GetDpiForWindow(visit.foreground.hwnd()) },
                    "dpi_awareness": unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(visit.foreground.hwnd())) }.0,
                    "element": read.element,
                })
            }
            Query::Pointer { point, window, geometry, action_at }
                if self.pointer.is_some_and(|request| request.id == read.request.id) =>
            {
                self.pointer = None;
                self.worker.retain_pointer(None);
                if matches!(read.status, Status::OutsideApproval) {
                    rows.push(json!({ "kind": "pointer_element", "approved": false }));
                    return;
                }
                if authorized_result && let Some(completed) = read.completed {
                    self.cached_pointer = Some((point, geometry, window, completed, read.request.id));
                }
                json!({
                    "kind": "pointer_element", "window": window.window,
                    "geometry": geometry, "point": point, "element": read.element,
                    "action_t_us": action_at.and_then(time),
                    "post_action": read.started.zip(action_at).map(|(started, action)| started >= action),
                    "action_to_completion_us": read.completed.and_then(time).zip(action_at.and_then(time))
                        .filter(|(completed, action)| completed >= action)
                        .map(|(completed, action)| completed.0.saturating_sub(action.0)),
                    "dpi": unsafe { GetDpiForWindow(window.hwnd()) },
                    "dpi_awareness": unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(window.hwnd())) }.0,
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
        row["uia"] = json!(read.status);
        rows.push(row);
    }
}
