//! Windows UI Automation calls and COM lifetimes belong to this windowless
//! MTA owner. An unresponsive provider cannot block the context or input gate.

use std::{
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Release}},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use screen_fix_native_observation::{
    uia::{Client, Query as NativeQuery, Read as NativeRead, Request as NativeRequest, TextClassification},
    uia_commits::{Commit, Queue as CommitQueue},
};

pub(super) use screen_fix_native_observation::uia::{Element, ReadUnavailable, window_at, window_root};

use crate::{
    capture::context::TextTarget,
    protocol::{FocusGeneration, GeometryId, PhysicalPoint, QpcTick, RequestId, ipc::ProcessInstance},
};

use super::{Visit, hung::Hung, identity::{MetadataScrubber, WindowProcess}};
use super::super::{ProbeError, clock, io_error};

pub(super) const DEADLINE: Duration = Duration::from_millis(500);
const QUEUE: usize = 8;

#[derive(Clone, Copy)]
pub(super) enum Query {
    Focus(Visit),
    Pointer { point: PhysicalPoint, window: WindowProcess, root: WindowProcess, geometry: GeometryId, action_at: Option<QpcTick> },
}

#[derive(Clone, Copy)]
pub(super) struct Request {
    pub id: RequestId,
    pub generation: FocusGeneration,
    pub focus_revision: u64,
    pub requested: QpcTick,
    pub expires: Instant,
    pub query: Query,
}

pub(super) struct Read {
    pub request: Request,
    pub started: Option<QpcTick>,
    pub completed: Option<QpcTick>,
    pub status: Result<(), ReadUnavailable>,
    pub target: TextTarget,
    pub element: Option<Element>,
    pub scoped_dpi: Option<(Option<std::num::NonZeroU32>, Option<i32>)>,
}

impl Read {
    fn observed(request: Request, native: NativeRead) -> Self {
        let target = match native.target {
            TextClassification::Password => TextTarget::Password,
            TextClassification::Editable => TextTarget::Editable,
            TextClassification::NonText => TextTarget::NonText,
            TextClassification::PossiblySensitive => TextTarget::PossiblySensitive,
            TextClassification::TypingUnknown => TextTarget::TypingUnknown,
        };
        Self { request, started: native.started, completed: native.completed,
            status: native.status, target, element: native.element, scoped_dpi: None }
    }

    fn outcome(request: Request, started: Option<QpcTick>, reason: ReadUnavailable) -> Self {
        Self { request, started, completed: clock::qpc().ok(), status: Err(reason), target: TextTarget::PossiblySensitive, element: None, scoped_dpi: None }
    }
}

pub(in crate::platform::windows) struct FocusGuard {
    generation: FocusGeneration,
    expected_revision: u64,
    desired: Arc<AtomicU64>,
    revision: Arc<AtomicU64>,
}

impl FocusGuard {
    pub fn current(&self) -> bool {
        self.desired.load(Acquire) == self.generation.0
            && self.revision.load(Acquire) == self.expected_revision
    }
}

#[derive(Default)]
struct Blocks {
    processes: Vec<ProcessInstance>,
    limited: bool,
}

#[derive(Default)]
struct Gate {
    desired: Arc<AtomicU64>,
    revision: Arc<AtomicU64>,
    pointer_id: AtomicU64,
    blocks: RwLock<Blocks>,
    /// The client failed to open; every request resolves as unavailable.
    unopened: AtomicBool,
    /// When the worker left its queue wait; none while it waits.
    busy_since: Mutex<Option<Instant>>,
}

impl Gate {
    fn blocked(&self, process: ProcessInstance) -> Option<ReadUnavailable> {
        let Ok(blocks) = self.blocks.read() else { return Some(ReadUnavailable::Unavailable) };
        if blocks.processes.contains(&process) {
            Some(ReadUnavailable::Hung)
        } else {
            blocks.limited.then_some(ReadUnavailable::Unavailable)
        }
    }

    fn invalid(&self, request: NativeRequest) -> Option<ReadUnavailable> {
        if request.generation.0 != self.desired.load(Acquire)
            || request.focus_revision != self.revision.load(Acquire)
            || (matches!(request.query, NativeQuery::Pointer { .. }) && request.id.0 != self.pointer_id.load(Acquire))
        {
            Some(ReadUnavailable::Stale)
        } else {
            let process = match request.query {
                NativeQuery::Focus { foreground, .. } => foreground.process,
                NativeQuery::Pointer { window, .. } => window.process,
            };
            self.blocked(process)
        }
    }
}

pub(super) struct Worker {
    requests: SyncSender<Request>,
    results: Receiver<Read>,
    gate: Arc<Gate>,
    thread: thread::JoinHandle<()>,
    commits: Arc<CommitQueue>,
}

impl Worker {
    pub fn start(scrubber: MetadataScrubber) -> Result<Self, ProbeError> {
        let (requests, queued) = sync_channel::<Request>(QUEUE);
        let (completed, results) = sync_channel(QUEUE);
        let gate = Arc::new(Gate::default());
        let commits = Arc::new(CommitQueue::new());
        let worker_commits = Arc::clone(&commits);
        let worker_gate = Arc::clone(&gate);
        let thread = thread::Builder::new().name("context-uia-mta".into()).spawn(move || {
            let mut client = Client::open(Arc::clone(&worker_gate.revision)).ok();
            worker_gate.unopened.store(client.is_none(), Release);
            let invalid = |request| worker_gate.invalid(request);
            let set_busy = |busy: bool| if let Ok(mut since) = worker_gate.busy_since.lock() {
                *since = busy.then(Instant::now);
            };
            loop {
                set_busy(false);
                let request = queued.recv_timeout(Duration::from_millis(50));
                set_busy(true);
                if let Some(client) = &mut client {
                    client.remove_stale_subscription(&invalid);
                }
                let request = match request {
                    Ok(request) => request,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                let native = request.native();
                if invalid(native).is_some() {
                    continue;
                }
                let read = if let Some(client) = &mut client {
                    Read::observed(request, client.read(native, &scrubber, &invalid))
                } else {
                    Read::outcome(request, None, ReadUnavailable::Unavailable)
                };
                let returned = completed.try_send(read).is_ok();
                if let Some(client) = &mut client {
                    client.finish_read(returned, &invalid, &worker_gate.desired, &worker_gate.revision, &worker_commits);
                }
            }
        }).map_err(|error| io_error("Start the UI Automation MTA worker", error))?;
        Ok(Self { requests, results, gate, thread, commits })
    }

    pub fn retain_generation(&self, generation: FocusGeneration) {
        self.gate.desired.store(generation.0, Release);
        self.retain_pointer(None);
    }

    pub fn focus_revision(&self) -> u64 {
        self.gate.revision.load(Acquire)
    }

    pub fn update_hung(&self, hung: &Hung) {
        if let Ok(mut blocks) = self.gate.blocks.write() {
            blocks.processes.clear();
            blocks.processes.extend(hung.processes());
            blocks.limited = hung.coverage_limited();
        }
    }

    pub fn blocked(&self, process: ProcessInstance) -> Option<ReadUnavailable> {
        self.gate.blocked(process)
    }

    pub fn retain_pointer(&self, request: Option<RequestId>) {
        self.gate.pointer_id.store(request.map_or(0, |id| id.0), Release);
    }

    pub fn submit(&self, request: Request) -> bool {
        self.requests.try_send(request).is_ok()
    }

    pub fn results(&self) -> impl Iterator<Item = Read> + '_ {
        self.results.try_iter().take(QUEUE)
    }

    pub fn focus_guard(&self, generation: FocusGeneration, expected_revision: u64) -> FocusGuard {
        FocusGuard {
            generation, expected_revision, desired: Arc::clone(&self.gate.desired), revision: Arc::clone(&self.gate.revision),
        }
    }

    pub fn commits(&self) -> Vec<Commit> {
        self.commits.take()
    }

    pub fn commit_losses(&self) -> Vec<screen_fix_native_observation::uia_commits::CommitLoss> {
        self.commits.take_loss()
    }

    pub fn running(&self) -> bool {
        !self.thread.is_finished()
    }

    /// The worker cannot serve UI Automation: its client did not open, or
    /// one call has been blocked longer than two transaction timeouts
    /// (§6.2 row 14). Requests keep resolving per request.
    pub fn failure(&self) -> Option<&'static str> {
        if self.gate.unopened.load(Acquire) {
            return Some("UI Automation client open");
        }
        let blocked = self.gate.busy_since.lock().ok()?.is_some_and(|since| since.elapsed() >= 2 * DEADLINE);
        blocked.then_some("UI Automation call blocked")
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.gate.desired.store(0, Release);
    }
}

impl Query {
    pub fn process(self) -> ProcessInstance {
        match self {
            Self::Focus(visit) => visit.foreground.process,
            Self::Pointer { window, .. } => window.process,
        }
    }
}

impl Request {
    fn native(self) -> NativeRequest {
        let query = match self.query {
            Query::Focus(visit) => NativeQuery::Focus { foreground: visit.foreground, native: visit.native },
            Query::Pointer { point, window, root, .. } => NativeQuery::Pointer { point, window, root },
        };
        NativeRequest { id: self.id, generation: self.generation, focus_revision: self.focus_revision,
            expires: self.expires, query }
    }
}
