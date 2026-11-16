//! Windows UI Automation calls and COM lifetimes belong to this windowless
//! MTA owner. An unresponsive provider cannot block the context or input gate.

use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering::{Acquire, Release}},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HWND, POINT},
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            Variant::{VT_BOOL, VariantToBoolean},
        },
        UI::{Accessibility::*, WindowsAndMessaging::{IsHungAppWindow, WindowFromPhysicalPoint}},
    },
    core::{BSTR, Interface, Ref, implement},
};

use crate::{
    capture::context::TextTarget,
    protocol::{FocusGeneration, GeometryId, PhysicalPoint, PhysicalRect, QpcTick, RequestId, ipc::ProcessInstance},
};

use super::{
    Visit,
    hung::{Hung, is_ghost},
    identity::{MetadataScrubber, WindowProcess},
    windows::{FocusSnapshot, password_style},
    uia_commits::{Commit, Handler, Queue as CommitQueue},
};
use super::super::{Apartment, ProbeError, clock, mp4::io_error};

pub(super) const DEADLINE: Duration = Duration::from_millis(500);
const QUEUE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Status {
    Ready,
    TimedOut,
    Hung,
    Unavailable,
    Stale,
    OutsideApproval,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct Element {
    pub name: Option<String>,
    pub automation_id: Option<String>,
    pub class: Option<String>,
    pub framework: Option<String>,
    pub control_type: Option<i32>,
    pub bounds: Option<PhysicalRect>,
    pub ancestor_names: Vec<String>,
    pub password: Option<bool>,
    pub editable: Option<bool>,
}

#[derive(Clone, Copy)]
pub(super) enum Query {
    Focus(Visit),
    Pointer { point: PhysicalPoint, window: WindowProcess, geometry: GeometryId, action_at: Option<QpcTick> },
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
    pub status: Status,
    pub target: TextTarget,
    pub element: Option<Element>,
}

impl Read {
    fn outcome(request: Request, started: Option<QpcTick>, status: Status) -> Self {
        Self { request, started, completed: clock::qpc().ok(), status, target: TextTarget::PossiblySensitive, element: None }
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
}

impl Gate {
    fn blocked(&self, process: ProcessInstance) -> Option<Status> {
        let Ok(blocks) = self.blocks.read() else { return Some(Status::Unavailable) };
        if blocks.processes.contains(&process) {
            Some(Status::Hung)
        } else {
            blocks.limited.then_some(Status::Unavailable)
        }
    }

    fn invalid(&self, request: Request) -> Option<Status> {
        if request.generation.0 != self.desired.load(Acquire)
            || request.focus_revision != self.revision.load(Acquire)
            || (matches!(request.query, Query::Pointer { .. }) && request.id.0 != self.pointer_id.load(Acquire))
        {
            Some(Status::Stale)
        } else {
            self.blocked(request.query.process())
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
            loop {
                let request = queued.recv_timeout(Duration::from_millis(50));
                if let Some(client) = &mut client {
                    client.remove_stale_subscription(&worker_gate);
                }
                let request = match request {
                    Ok(request) => request,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                if worker_gate.invalid(request).is_some() {
                    continue;
                }
                let read = if let Some(client) = &mut client {
                    client.read(request, &scrubber, &worker_gate)
                } else {
                    Read::outcome(request, None, Status::Unavailable)
                };
                let returned = completed.try_send(read).is_ok();
                if let Some(client) = &mut client && let Some((request, element)) = client.pending_subscription.take()
                    && returned && Instant::now() < request.expires && worker_gate.invalid(request).is_none()
                {
                    client.subscribe(request, element, &worker_gate, &worker_commits);
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

    pub fn blocked(&self, process: ProcessInstance) -> Option<Status> {
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

    pub fn running(&self) -> bool {
        !self.thread.is_finished()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.gate.desired.store(0, Release);
    }
}

#[implement(IUIAutomationFocusChangedEventHandler)]
struct FocusNotification(Arc<AtomicU64>);

impl IUIAutomationFocusChangedEventHandler_Impl for FocusNotification_Impl {
    fn HandleFocusChangedEvent(&self, _: Ref<'_, IUIAutomationElement>) -> windows::core::Result<()> {
        // A native callback only invalidates old evidence. It reads no
        // provider properties and transfers no element to another thread.
        self.0.fetch_add(1, Release);
        Ok(())
    }
}

struct TextEditSubscription {
    request: Request,
    element: IUIAutomationElement,
    handler: IUIAutomationTextEditTextChangedEventHandler,
}

struct Client {
    automation: IUIAutomation2,
    cache: IUIAutomationCacheRequest,
    focus_handler: IUIAutomationFocusChangedEventHandler,
    text_edit: Option<IUIAutomation3>,
    subscription: Option<TextEditSubscription>,
    pending_subscription: Option<(Request, IUIAutomationElement)>,
    _apartment: Apartment,
}

impl Client {
    fn open(revision: Arc<AtomicU64>) -> windows::core::Result<Self> {
        // The apartment guard is the last field to drop.
        let _apartment = Apartment::initialize().map_err(|_| windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_FAIL,
        ))?;
        let automation: IUIAutomation2 = unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;
        unsafe {
            automation.SetConnectionTimeout(250)?;
            automation.SetTransactionTimeout(500)?;
            automation.SetAutoSetFocus(false)?;
        }
        let cache = unsafe { automation.CreateCacheRequest() }?;
        unsafe { cache.SetTreeScope(TreeScope_Element) }?;
        for property in [
            UIA_ProcessIdPropertyId, UIA_NativeWindowHandlePropertyId, UIA_HasKeyboardFocusPropertyId,
            UIA_ControlTypePropertyId, UIA_FrameworkIdPropertyId, UIA_NamePropertyId,
            UIA_AutomationIdPropertyId, UIA_ClassNamePropertyId, UIA_BoundingRectanglePropertyId,
            UIA_IsPasswordPropertyId, UIA_IsKeyboardFocusablePropertyId, UIA_ValueIsReadOnlyPropertyId,
            UIA_IsTextEditPatternAvailablePropertyId,
        ] {
            unsafe { cache.AddProperty(property) }?;
        }
        unsafe { cache.AddPattern(UIA_TextPatternId) }?;
        let focus_handler: IUIAutomationFocusChangedEventHandler = FocusNotification(revision).into();
        unsafe { automation.AddFocusChangedEventHandler(None, &focus_handler) }?;
        let text_edit = automation.cast().ok();
        Ok(Self { automation, cache, focus_handler, text_edit, subscription: None, pending_subscription: None, _apartment })
    }

    fn read(&mut self, request: Request, scrubber: &MetadataScrubber, gate: &Gate) -> Read {
        let started = clock::qpc().ok();
        if Instant::now() >= request.expires {
            return Read::outcome(request, started, Status::TimedOut);
        }
        let Some(window) = request.query.window() else {
            return Read::outcome(request, started, Status::Stale);
        };
        let root = root_window(window.hwnd());
        if unsafe { IsHungAppWindow(window.hwnd()) }.as_bool()
            || (!root.0.is_null() && unsafe { IsHungAppWindow(root) }.as_bool())
        {
            return Read::outcome(request, started, Status::Hung);
        }
        if let Some(status) = gate.invalid(request) {
            return Read::outcome(request, started, status);
        }
        let element = unsafe {
            match request.query {
                Query::Focus(_) => self.automation.GetFocusedElementBuildCache(&self.cache),
                Query::Pointer { point, .. } => self.automation.ElementFromPointBuildCache(point_native(point), &self.cache),
            }
        };
        if Instant::now() >= request.expires {
            return Read::outcome(request, started, Status::TimedOut);
        }
        if let Some(status) = gate.invalid(request) {
            return Read::outcome(request, started, status);
        }
        if request.query.window() != Some(window) {
            return Read::outcome(request, started, Status::Stale);
        }
        let Ok(element) = element else {
            return Read::outcome(request, started, Status::Unavailable);
        };
        if !belongs_to(&element, window) {
            return Read::outcome(request, started, Status::OutsideApproval);
        }
        let native_password = match request.query {
            Query::Focus(visit) => visit.native.is_some_and(|native| native.focus.password_style == Some(true)),
            Query::Pointer { .. } => password_style(window.hwnd()) == Some(true),
        };
        let password = if native_password { Some(true) } else { property_bool(&element, UIA_IsPasswordPropertyId) };
        let control_type = unsafe { element.CachedControlType() }.ok();
        let has_keyboard_focus = property_bool(&element, UIA_HasKeyboardFocusPropertyId) == Some(true);
        let editable = if password == Some(false) {
            editable(&element, control_type, request, gate)
        } else {
            None
        };
        let target = match (password, editable, has_keyboard_focus) {
            (Some(true), _, _) => TextTarget::Password,
            (Some(false), Some(true), true) => TextTarget::Editable,
            (Some(false), Some(false), true) => TextTarget::NonText,
            _ => TextTarget::PossiblySensitive,
        };
        let allow_strings = password == Some(false)
            && (!matches!(request.query, Query::Focus(_)) || target != TextTarget::PossiblySensitive);
        let mut fields = Element {
            name: allow_strings.then(|| bounded(unsafe { element.CachedName() }, scrubber)).flatten(),
            automation_id: allow_strings.then(|| bounded(unsafe { element.CachedAutomationId() }, scrubber)).flatten(),
            class: allow_strings.then(|| bounded(unsafe { element.CachedClassName() }, scrubber)).flatten(),
            framework: bounded(unsafe { element.CachedFrameworkId() }, scrubber),
            control_type: control_type.map(|value| value.0),
            bounds: unsafe { element.CachedBoundingRectangle() }.ok()
                .and_then(|rect| PhysicalRect::new(rect.left, rect.top, rect.right, rect.bottom)),
            ancestor_names: Vec::new(), password, editable,
        };
        if allow_strings && matches!(request.query, Query::Pointer { .. }) {
            self.ancestors(element.clone(), window, request, gate, scrubber, &mut fields.ancestor_names);
        }
        if Instant::now() >= request.expires {
            return Read::outcome(request, started, Status::TimedOut);
        }
        if let Some(status) = gate.invalid(request) {
            return Read::outcome(request, started, status);
        }
        if request.query.window() != Some(window) {
            return Read::outcome(request, started, Status::Stale);
        }
        if target == TextTarget::Editable && matches!(request.query, Query::Focus(_))
            && property_bool(&element, UIA_IsTextEditPatternAvailablePropertyId) == Some(true)
        {
            self.pending_subscription = Some((request, element));
        }
        Read { request, started, completed: clock::qpc().ok(), status: Status::Ready, target, element: Some(fields) }
    }

    fn subscribe(
        &mut self, request: Request, element: IUIAutomationElement, gate: &Gate, commits: &Arc<CommitQueue>,
    ) {
        let Some(client) = &self.text_edit else { return };
        if self.subscription.is_some() || gate.invalid(request).is_some() {
            return;
        }
        let handler: IUIAutomationTextEditTextChangedEventHandler = Handler {
            generation: request.generation, focus_revision: request.focus_revision,
            desired: Arc::clone(&gate.desired), revision: Arc::clone(&gate.revision), queue: Arc::clone(commits),
        }.into();
        let registered = unsafe {
            client.AddTextEditTextChangedEventHandler(&element, TreeScope_Element,
                TextEditChangeType_CompositionFinalized, None, &handler)
        };
        if registered.is_ok() {
            self.subscription = Some(TextEditSubscription { request, element, handler });
        }
    }

    fn remove_stale_subscription(&mut self, gate: &Gate) {
        if self.subscription.as_ref().is_some_and(|subscription| gate.invalid(subscription.request).is_some()) {
            self.remove_subscription();
        }
    }

    fn remove_subscription(&mut self) {
        if let (Some(client), Some(subscription)) = (&self.text_edit, self.subscription.take()) {
            let _ = unsafe { client.RemoveTextEditTextChangedEventHandler(&subscription.element, &subscription.handler) };
        }
    }

    fn ancestors(&self, mut element: IUIAutomationElement, window: WindowProcess, request: Request,
        gate: &Gate, scrubber: &MetadataScrubber, names: &mut Vec<String>,
    ) {
        let Ok(walker) = (unsafe { self.automation.ControlViewWalker() }) else { return };
        for _ in 0..8 {
            if Instant::now() >= request.expires || gate.invalid(request).is_some() {
                break;
            }
            let Ok(parent) = (unsafe { walker.GetParentElementBuildCache(&element, &self.cache) }) else { break };
            if !belongs_to(&parent, window) || property_bool(&parent, UIA_IsPasswordPropertyId) != Some(false) {
                break;
            }
            if let Some(name) = bounded(unsafe { parent.CachedName() }, scrubber) {
                names.push(name);
            }
            element = parent;
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.remove_subscription();
        let _ = unsafe { self.automation.RemoveFocusChangedEventHandler(&self.focus_handler) };
    }
}

impl Query {
    pub fn process(self) -> ProcessInstance {
        match self {
            Self::Focus(visit) => visit.foreground.process,
            Self::Pointer { window, .. } => window.process,
        }
    }

    fn window(self) -> Option<WindowProcess> {
        match self {
            Self::Focus(visit) => {
                let foreground = WindowProcess::foreground()?;
                let native = FocusSnapshot::read(foreground)?;
                (foreground == visit.foreground && !is_ghost(foreground.hwnd().0) && Some(native.focus) == visit.native.map(|value| value.focus)
                    && !native.hung).then_some(foreground)
            }
            Self::Pointer { point, window, .. } => {
                let found = window_at(point)?;
                (found == window).then_some(found)
            }
        }
    }
}

pub(super) fn window_at(point: PhysicalPoint) -> Option<WindowProcess> {
    let window = unsafe { WindowFromPhysicalPoint(point_native(point)) };
    let root = root_window(window);
    if root.0.is_null() || is_ghost(root.0) {
        return None;
    }
    WindowProcess::read(window)
}

fn root_window(window: HWND) -> HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor};
    HWND(unsafe { GetAncestor(window.0, GA_ROOT) })
}

fn point_native(point: PhysicalPoint) -> POINT {
    POINT { x: point.x, y: point.y }
}

fn belongs_to(element: &IUIAutomationElement, expected: WindowProcess) -> bool {
    unsafe { element.CachedProcessId() }.ok().is_some_and(|pid| pid as u32 == expected.process.pid.get())
        && WindowProcess::read(expected.hwnd()) == Some(expected)
}

fn property_bool(element: &IUIAutomationElement, property: UIA_PROPERTY_ID) -> Option<bool> {
    let value = unsafe { element.GetCachedPropertyValueEx(property, true) }.ok()?;
    (value.vt() == VT_BOOL).then(|| unsafe { VariantToBoolean(&value) }.ok().map(|value| value.as_bool())).flatten()
}

fn editable(element: &IUIAutomationElement, control: Option<UIA_CONTROLTYPE_ID>, request: Request, gate: &Gate) -> Option<bool> {
    if !property_bool(element, UIA_IsKeyboardFocusablePropertyId)? {
        return Some(false);
    }
    if let Some(read_only) = property_bool(element, UIA_ValueIsReadOnlyPropertyId) {
        return Some(!read_only);
    }
    if let Ok(text) = unsafe { element.GetCachedPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) } {
        if Instant::now() >= request.expires || gate.invalid(request).is_some() {
            return None;
        }
        let range = unsafe { text.DocumentRange() }.ok()?;
        if Instant::now() >= request.expires || gate.invalid(request).is_some() {
            return None;
        }
        let read_only = unsafe { range.GetAttributeValue(UIA_IsReadOnlyAttributeId) }.ok()?;
        return (read_only.vt() == VT_BOOL).then(|| unsafe { VariantToBoolean(&read_only) }.ok().map(|value| !value.as_bool())).flatten();
    }
    match control? {
        UIA_ButtonControlTypeId | UIA_CheckBoxControlTypeId | UIA_RadioButtonControlTypeId
        | UIA_MenuControlTypeId | UIA_MenuItemControlTypeId | UIA_TabItemControlTypeId
        | UIA_TreeItemControlTypeId | UIA_ListItemControlTypeId | UIA_ToolBarControlTypeId => Some(false),
        _ => None,
    }
}

fn bounded(value: windows::core::Result<BSTR>, scrubber: &MetadataScrubber) -> Option<String> {
    let value = value.ok()?;
    let units: &[u16] = &value;
    Some(scrubber.scrub(&String::from_utf16_lossy(&units[..units.len().min(1024)]), 4096))
}
