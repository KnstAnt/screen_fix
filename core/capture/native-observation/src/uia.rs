//! Native UI Automation reads and subscriptions on one owning MTA thread.

use std::{
    sync::{Arc, atomic::{AtomicU64, Ordering::Release}},
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HWND, POINT},
        System::{
            Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance},
            Variant::{VT_BOOL, VT_UNKNOWN, VariantToBoolean},
        },
        UI::{Accessibility::*, WindowsAndMessaging::{IsHungAppWindow, WindowFromPhysicalPoint}},
    },
    core::{BSTR, Interface, Ref, implement},
};

use screen_fix_protocol::{
    BoundedList, FocusGeneration, PayloadLimit, PhysicalPoint, PhysicalRect, QpcTick, RequestId,
    ipc::elevated::{MAX_METADATA_BYTES, MAX_UIA_ANCESTORS, MAX_UIA_VALUE_UTF16, Metadata, UiaElement},
};

pub use screen_fix_protocol::ipc::elevated::{ReadUnavailable, TextClassification};

/// The client transaction timeout never exceeds the request deadline.
const MAX_CALL: Duration = Duration::from_millis(500);
/// The client connection timeout.
const MAX_CONNECT: Duration = Duration::from_millis(250);
/// A provider call with less remaining budget cannot complete usefully.
const MIN_CALL: Duration = Duration::from_millis(10);

use crate::{
    com::ComApartment,
    hung::is_ghost,
    identity::{MetadataScrubber, WindowProcess},
    uia_commits::{CommitScope, Handler, Queue as CommitQueue},
    windows::{FocusSnapshot, password_style},
};

#[derive(Debug, serde::Serialize)]
pub struct Element {
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

impl TryFrom<Element> for UiaElement {
    type Error = PayloadLimit;

    fn try_from(element: Element) -> Result<Self, Self::Error> {
        let text = |value: Option<String>| value.as_deref().map(Metadata::new).transpose();
        let ancestor_names = element.ancestor_names.iter().map(|name| Metadata::new(name))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            name: text(element.name)?,
            automation_id: text(element.automation_id)?,
            class: text(element.class)?,
            framework: text(element.framework)?,
            control_type: element.control_type,
            bounds: element.bounds,
            ancestor_names: BoundedList::new(ancestor_names)?,
            password: element.password,
            editable: element.editable,
        })
    }
}

#[derive(Clone, Copy)]
pub enum Query {
    Focus { foreground: WindowProcess, native: Option<FocusSnapshot> },
    Pointer { point: PhysicalPoint, window: WindowProcess, root: WindowProcess },
}

#[derive(Clone, Copy)]
pub struct Request {
    pub id: RequestId,
    pub generation: FocusGeneration,
    pub focus_revision: u64,
    pub expires: Instant,
    pub query: Query,
}

pub struct Read {
    pub started: Option<QpcTick>,
    pub completed: Option<QpcTick>,
    /// Ok for a completed read; otherwise why the read is unavailable.
    pub status: Result<(), ReadUnavailable>,
    pub target: TextClassification,
    pub element: Option<Element>,
}

impl Read {
    fn outcome(started: Option<QpcTick>, reason: ReadUnavailable) -> Self {
        Self { started, completed: qpc(), status: Err(reason), target: TextClassification::PossiblySensitive, element: None }
    }
}

fn qpc() -> Option<QpcTick> {
    let mut value = 0;
    (unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut value) } != 0)
        .then_some(QpcTick(value))
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

pub struct Client {
    automation: IUIAutomation2,
    cache: IUIAutomationCacheRequest,
    identification: IUIAutomationCacheRequest,
    classification: IUIAutomationCacheRequest,
    strings: IUIAutomationCacheRequest,
    scoped: bool,
    focus_handler: Option<IUIAutomationFocusChangedEventHandler>,
    text_edit: Option<IUIAutomation3>,
    subscription: Option<TextEditSubscription>,
    pending_subscription: Option<(Request, IUIAutomationElement)>,
    _apartment: ComApartment,
}

impl Client {
    pub fn open(revision: Arc<AtomicU64>) -> windows::core::Result<Self> {
        Self::open_inner(revision, false)
    }

    pub fn open_scoped(revision: Arc<AtomicU64>) -> windows::core::Result<Self> {
        Self::open_inner(revision, true)
    }

    fn open_inner(revision: Arc<AtomicU64>, scoped: bool) -> windows::core::Result<Self> {
        // The apartment guard is the last field to drop.
        let _apartment = ComApartment::initialize(COINIT_MULTITHREADED)?;
        let automation: IUIAutomation2 = unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;
        unsafe {
            automation.SetConnectionTimeout(MAX_CONNECT.as_millis() as u32)?;
            automation.SetTransactionTimeout(MAX_CALL.as_millis() as u32)?;
            automation.SetAutoSetFocus(false)?;
        }
        let identity = [UIA_ProcessIdPropertyId, UIA_NativeWindowHandlePropertyId];
        let traits = [
            UIA_HasKeyboardFocusPropertyId, UIA_ControlTypePropertyId, UIA_FrameworkIdPropertyId,
            UIA_BoundingRectanglePropertyId, UIA_IsPasswordPropertyId, UIA_IsKeyboardFocusablePropertyId,
            UIA_IsValuePatternAvailablePropertyId, UIA_ValueIsReadOnlyPropertyId, UIA_IsTextEditPatternAvailablePropertyId,
        ];
        let text = [UIA_NamePropertyId, UIA_AutomationIdPropertyId, UIA_ClassNamePropertyId];
        let cache = cache_request(&automation, identity.into_iter().chain(traits).chain(text), true)?;
        let identification = cache_request(&automation, identity, false)?;
        let classification = cache_request(&automation, identity.into_iter().chain(traits), true)?;
        let strings = cache_request(&automation, identity.into_iter().chain([UIA_IsPasswordPropertyId]).chain(text), false)?;
        let focus_handler: IUIAutomationFocusChangedEventHandler = FocusNotification(revision).into();
        unsafe { automation.AddFocusChangedEventHandler(None, &focus_handler) }?;
        let text_edit = automation.cast().ok();
        Ok(Self { automation, cache, identification, classification, strings, scoped, focus_handler: Some(focus_handler), text_edit, subscription: None, pending_subscription: None, _apartment })
    }

    pub fn read(&mut self, request: Request, scrubber: &MetadataScrubber, invalid: &impl Fn(Request) -> Option<ReadUnavailable>) -> Read {
        self.read_inner(request, Some(scrubber), invalid)
    }

    pub fn editable_focus(&mut self, request: Request, invalid: &impl Fn(Request) -> Option<ReadUnavailable>) -> bool {
        self.scoped && matches!(request.query, Query::Focus { .. })
            && self.read_inner(request, None, invalid).target == TextClassification::Editable
    }

    fn read_inner(
        &mut self, request: Request, scrubber: Option<&MetadataScrubber>, invalid: &impl Fn(Request) -> Option<ReadUnavailable>,
    ) -> Read {
        let started = qpc();
        self.read_checked(request, scrubber, invalid, started)
            .unwrap_or_else(|reason| Read::outcome(started, reason))
    }

    /// The read itself. Every checkpoint that ends it is an `Err`.
    fn read_checked(
        &mut self, request: Request, scrubber: Option<&MetadataScrubber>,
        invalid: &impl Fn(Request) -> Option<ReadUnavailable>, started: Option<QpcTick>,
    ) -> Result<Read, ReadUnavailable> {
        if Instant::now() >= request.expires {
            return Err(ReadUnavailable::TimedOut);
        }
        let window = request.query.window().ok_or(ReadUnavailable::Stale)?;
        let root = root_window(window.hwnd());
        if unsafe { IsHungAppWindow(window.hwnd()) }.as_bool()
            || (!root.0.is_null() && unsafe { IsHungAppWindow(root) }.as_bool())
        {
            return Err(ReadUnavailable::Hung);
        }
        let guard = ReadGuard { request, window, invalid };
        guard.valid()?;
        let discovery = if self.scoped { &self.identification } else { &self.cache };
        self.budget(request)?;
        let element = unsafe {
            match request.query {
                Query::Focus { .. } => self.automation.GetFocusedElementBuildCache(discovery),
                Query::Pointer { point, .. } => self.automation.ElementFromPointBuildCache(point_native(point), discovery),
            }
        };
        guard.live()?;
        let element = element.map_err(|error| call_status(&error, ReadUnavailable::Unavailable))?;
        if !self.matches(&element, &guard)? {
            return Err(ReadUnavailable::OutsideApproval);
        }
        let native_password = match request.query {
            Query::Focus { native, .. } => native.and_then(|native| native.focus.password_style),
            Query::Pointer { .. } => password_style(window.hwnd()),
        };
        let element = if self.scoped {
            guard.valid()?;
            self.budget(request)?;
            let element = unsafe { element.BuildUpdatedCache(&self.classification) }
                .map_err(|error| call_status(&error, ReadUnavailable::Stale))?;
            if !self.matches(&element, &guard)? {
                return Err(ReadUnavailable::Stale);
            }
            element
        } else {
            element
        };
        let password = if native_password == Some(true) {
            Some(true)
        } else if self.scoped && native_password.is_none() {
            None
        } else {
            property_bool(&element, UIA_IsPasswordPropertyId)
        };
        let control_type = unsafe { element.CachedControlType() }.ok();
        let keyboard_focus = property_bool(&element, UIA_HasKeyboardFocusPropertyId);
        let focused = match request.query {
            // D-4d: an element without keyboard focus is a stale read, which
            // the owner re-issues once within the classification wait.
            Query::Focus { .. } if keyboard_focus == Some(false) && password != Some(true) =>
                return Err(ReadUnavailable::Stale),
            // An unsupported property is resolved by the focused-element query
            // when the native snapshot, taken from hwndFocus, shows the same window.
            Query::Focus { native, .. } => keyboard_focus == Some(true) || keyboard_focus.is_none()
                && native.is_some_and(|native| !native.active_fallback
                    && unsafe { element.CachedNativeWindowHandle() }
                        .is_ok_and(|hwnd| hwnd.0.is_null() || hwnd.0 as usize as u64 == native.focus.window.0)),
            Query::Pointer { .. } => keyboard_focus == Some(true),
        };
        let editable = if password == Some(false) {
            self.editable(&element, &guard)?
        } else {
            Writability::Undetermined
        };
        let target = classify(password, editable, focused);
        let editable = editable.reported();
        let Some(scrubber) = scrubber else {
            // Every failed check of the final checkpoint is a stale read here.
            guard.live().map_err(|_| ReadUnavailable::Stale)?;
            return Ok(Read { started, completed: qpc(), status: Ok(()), target, element: None });
        };
        let allow_strings = password == Some(false)
            && (!matches!(request.query, Query::Focus { .. }) || target != TextClassification::PossiblySensitive);
        let strings = if allow_strings { self.fetch_strings(&element, &guard) } else { None };
        let mut fields = Element {
            name: strings.as_ref().and_then(|element| bounded(unsafe { element.CachedName() }, scrubber)),
            automation_id: strings.as_ref().and_then(|element| bounded(unsafe { element.CachedAutomationId() }, scrubber)),
            class: strings.as_ref().and_then(|element| bounded(unsafe { element.CachedClassName() }, scrubber)),
            framework: bounded(unsafe { element.CachedFrameworkId() }, scrubber),
            control_type: control_type.map(|value| value.0),
            bounds: unsafe { element.CachedBoundingRectangle() }.ok()
                .and_then(|rect| PhysicalRect::new(rect.left, rect.top, rect.right, rect.bottom)),
            ancestor_names: Vec::new(), password, editable,
        };
        if strings.is_some() && matches!(request.query, Query::Pointer { .. }) {
            self.ancestors(element.clone(), &guard, scrubber, &mut fields.ancestor_names);
        }
        guard.live()?;
        if target == TextClassification::Editable && matches!(request.query, Query::Focus { .. })
            && property_bool(&element, UIA_IsTextEditPatternAvailablePropertyId) == Some(true)
        {
            self.pending_subscription = Some((request, element));
        }
        Ok(Read { started, completed: qpc(), status: Ok(()), target, element: Some(fields) })
    }

    /// The element whose cached strings may be read, unless the request is
    /// invalid. A scoped read caches them anew and keeps them only when the
    /// element still belongs to the window and is not a password field.
    fn fetch_strings(
        &self, element: &IUIAutomationElement, guard: &ReadGuard<'_, impl Fn(Request) -> Option<ReadUnavailable>>,
    ) -> Option<IUIAutomationElement> {
        if guard.invalidated() {
            return None;
        }
        if !self.scoped {
            return Some(element.clone());
        }
        // Strings are optional; a call without budget leaves them out.
        self.budget(guard.request).ok()
            .and_then(|()| unsafe { element.BuildUpdatedCache(&self.strings) }.ok())
            .filter(|candidate| {
                self.matches(candidate, guard) == Ok(true)
                    && property_bool(candidate, UIA_IsPasswordPropertyId) == Some(false)
            })
    }

    /// A candidate may be subscribed only after its result is queued for the owner.
    /// Taking it also drops obsolete or unqueued candidates.
    pub fn finish_read(
        &mut self, returned: bool, invalid: &impl Fn(Request) -> Option<ReadUnavailable>,
        desired: &Arc<AtomicU64>, revision: &Arc<AtomicU64>, commits: &Arc<CommitQueue>,
    ) {
        self.finish_scoped_read(returned, invalid, desired, revision, commits, None);
    }

    pub fn finish_scoped_read(
        &mut self, returned: bool, invalid: &impl Fn(Request) -> Option<ReadUnavailable>,
        desired: &Arc<AtomicU64>, revision: &Arc<AtomicU64>, commits: &Arc<CommitQueue>,
        scope: Option<CommitScope>,
    ) {
        let Some((request, element)) = self.pending_subscription.take() else { return };
        if !returned || Instant::now() >= request.expires || invalid(request).is_some() {
            return;
        }
        let Some(client) = &self.text_edit else { return };
        if self.subscription.is_some() || self.budget(request).is_err() {
            return;
        }
        let event_cache: Option<&IUIAutomationCacheRequest> = scope.as_ref().map(|_| &self.identification);
        let handler: IUIAutomationTextEditTextChangedEventHandler = Handler {
            generation: request.generation, focus_revision: request.focus_revision,
            desired: Arc::clone(desired), revision: Arc::clone(revision), queue: Arc::clone(commits), scope,
        }.into();
        let registered = unsafe {
            client.AddTextEditTextChangedEventHandler(&element, TreeScope_Element,
                TextEditChangeType_CompositionFinalized, event_cache, &handler)
        };
        if registered.is_ok() {
            self.subscription = Some(TextEditSubscription { request, element, handler });
        }
    }

    pub fn remove_stale_subscription(&mut self, invalid: &impl Fn(Request) -> Option<ReadUnavailable>) {
        if self.subscription.as_ref().is_some_and(|subscription| invalid(subscription.request).is_some()) {
            self.remove_subscription();
        }
    }

    fn remove_subscription(&mut self) {
        if let (Some(client), Some(subscription)) = (&self.text_edit, self.subscription.take()) {
            let _ = unsafe { client.RemoveTextEditTextChangedEventHandler(&subscription.element, &subscription.handler) };
        }
    }

    pub fn clear_subscription(&mut self) {
        self.pending_subscription.take();
        self.remove_subscription();
    }

    /// Limit the next provider call to the request's remaining budget (ALG §7.1).
    /// The total of all calls therefore never exceeds the request deadline.
    fn budget(&self, request: Request) -> Result<(), ReadUnavailable> {
        let remaining = request.expires.saturating_duration_since(Instant::now()).min(MAX_CALL);
        if remaining < MIN_CALL {
            return Err(ReadUnavailable::TimedOut);
        }
        let millis = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        unsafe { self.automation.SetTransactionTimeout(millis) }.map_err(|_| ReadUnavailable::Unavailable)
    }

    /// Whether the element belongs to the approved window. An exhausted budget
    /// or a provider timeout is reported as such, not as a mismatch.
    fn matches(
        &self, element: &IUIAutomationElement, guard: &ReadGuard<'_, impl Fn(Request) -> Option<ReadUnavailable>>,
    ) -> Result<bool, ReadUnavailable> {
        let window = guard.window;
        if guard.invalidated() || !belongs_to(element, window) { return Ok(false); }
        if !self.scoped { return Ok(true); }
        let mut cursor = element.clone();
        let Ok(walker) = (unsafe { self.automation.ControlViewWalker() }) else { return Ok(false); };
        for index in 0..=MAX_UIA_ANCESTORS {
            if guard.invalidated() || !belongs_to(&cursor, window) { return Ok(false); }
            let Ok(hwnd) = (unsafe { cursor.CachedNativeWindowHandle() }) else { return Ok(false); };
            if !hwnd.0.is_null() {
                return Ok(same_root(hwnd, window) && !guard.invalidated());
            }
            if index == MAX_UIA_ANCESTORS { return Ok(false); }
            self.budget(guard.request)?;
            cursor = match unsafe { walker.GetParentElementBuildCache(&cursor, &self.identification) } {
                Ok(parent) => parent,
                Err(error) if is_timeout(&error) => return Err(ReadUnavailable::TimedOut),
                Err(_) => return Ok(false),
            };
        }
        Ok(false)
    }

    /// CTX-9 and D-4 (C2): writable when keyboard focusable and either
    /// pattern reports writable; read-only when not focusable, when every
    /// present pattern reports read-only, or when a focusable element has
    /// neither pattern (D-4a); mixed for a mixed Text attribute (D-4c);
    /// undetermined when an attribute of a possibly present pattern is
    /// unavailable (D-4b), which also covers an unknown Value pattern presence.
    fn editable(
        &self, element: &IUIAutomationElement, guard: &ReadGuard<'_, impl Fn(Request) -> Option<ReadUnavailable>>,
    ) -> Result<Writability, ReadUnavailable> {
        let Some(focusable) = property_bool(element, UIA_IsKeyboardFocusablePropertyId) else {
            return Ok(Writability::Undetermined);
        };
        if !focusable {
            return Ok(Writability::ReadOnly);
        }
        let value = match property_bool(element, UIA_ValueIsReadOnlyPropertyId) {
            Some(read_only) => Some(Writability::from_read_only(read_only)),
            None => (property_bool(element, UIA_IsValuePatternAvailablePropertyId) != Some(false))
                .then_some(Writability::Undetermined),
        };
        if value == Some(Writability::Writable) {
            // One writable pattern decides; the Text pattern needs no provider call.
            return Ok(Writability::Writable);
        }
        let text = match unsafe { element.GetCachedPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) } {
            Ok(text) => Some(self.text_writability(&text, guard)?),
            Err(_) => None,
        };
        let present = [value, text];
        Ok(if present.contains(&Some(Writability::Writable)) {
            Writability::Writable
        } else if present.contains(&Some(Writability::Undetermined)) {
            Writability::Undetermined
        } else if present.contains(&Some(Writability::Mixed)) {
            Writability::Mixed
        } else {
            Writability::ReadOnly
        })
    }

    fn text_writability(
        &self, text: &IUIAutomationTextPattern, guard: &ReadGuard<'_, impl Fn(Request) -> Option<ReadUnavailable>>,
    ) -> Result<Writability, ReadUnavailable> {
        self.budget(guard.request)?;
        let Some(range) = after_call(unsafe { text.DocumentRange() }, guard)? else {
            return Ok(Writability::Undetermined);
        };
        self.budget(guard.request)?;
        let Some(read_only) = after_call(unsafe { range.GetAttributeValue(UIA_IsReadOnlyAttributeId) }, guard)? else {
            return Ok(Writability::Undetermined);
        };
        // A mixed range returns the reserved mixed-attribute object, compared
        // by identity. Any other object, such as the reserved not-supported
        // value, leaves the attribute undetermined (D-4 b1).
        if read_only.vt() == VT_UNKNOWN {
            let mixed = unsafe { self.automation.ReservedMixedAttributeValue() }.ok();
            let value = windows::core::IUnknown::try_from(&read_only).ok();
            return Ok(if mixed.zip(value).is_some_and(|(mixed, value)| mixed.as_raw() == value.as_raw()) {
                Writability::Mixed
            } else {
                Writability::Undetermined
            });
        }
        Ok((read_only.vt() == VT_BOOL)
            .then(|| unsafe { VariantToBoolean(&read_only) }.ok().map(|value| Writability::from_read_only(value.as_bool())))
            .flatten()
            .unwrap_or(Writability::Undetermined))
    }

    /// Optional ancestor names. Any exhausted budget ends the walk.
    fn ancestors(
        &self, mut element: IUIAutomationElement, guard: &ReadGuard<'_, impl Fn(Request) -> Option<ReadUnavailable>>,
        scrubber: &MetadataScrubber, names: &mut Vec<String>,
    ) {
        let Ok(walker) = (unsafe { self.automation.ControlViewWalker() }) else { return };
        for _ in 0..MAX_UIA_ANCESTORS {
            if guard.invalidated() || self.budget(guard.request).is_err() {
                break;
            }
            let discovery = if self.scoped { &self.identification } else { &self.cache };
            let Ok(parent) = (unsafe { walker.GetParentElementBuildCache(&element, discovery) }) else { break };
            if self.matches(&parent, guard) != Ok(true) { break; }
            let metadata = if self.scoped {
                if self.budget(guard.request).is_err() { break; }
                let Ok(traits) = (unsafe { parent.BuildUpdatedCache(&self.classification) }) else { break; };
                if self.matches(&traits, guard) != Ok(true)
                    || property_bool(&traits, UIA_IsPasswordPropertyId) != Some(false) { break; }
                if self.budget(guard.request).is_err() { break; }
                let Ok(text) = (unsafe { parent.BuildUpdatedCache(&self.strings) }) else { break; };
                if self.matches(&text, guard) != Ok(true) { break; }
                text
            } else {
                parent.clone()
            };
            if property_bool(&metadata, UIA_IsPasswordPropertyId) != Some(false) { break; }
            if let Some(name) = bounded(unsafe { metadata.CachedName() }, scrubber) {
                names.push(name);
            }
            element = parent;
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.remove_subscription();
        if let Some(handler) = self.focus_handler.take() {
            let _ = unsafe { self.automation.RemoveFocusChangedEventHandler(&handler) };
        }
    }
}

impl Query {
    fn window(self) -> Option<WindowProcess> {
        match self {
            Self::Focus { foreground: expected, native } => {
                let foreground = WindowProcess::foreground()?;
                // Without a focus window there is no focused element to classify.
                let current = FocusSnapshot::read(foreground).filter(|current| !current.active_fallback)?;
                (foreground == expected && !is_ghost(foreground.hwnd().0) && Some(current.focus) == native.map(|value| value.focus)
                    && !current.hung).then_some(foreground)
            }
            Self::Pointer { point, window, root } => {
                let found = window_at(point)?;
                (found == window && window_root(found) == Some(root)).then_some(found)
            }
        }
    }
}

pub fn window_at(point: PhysicalPoint) -> Option<WindowProcess> {
    let window = unsafe { WindowFromPhysicalPoint(point_native(point)) };
    let root = root_window(window);
    if root.0.is_null() || is_ghost(root.0) {
        return None;
    }
    WindowProcess::read(window)
}

pub fn window_root(window: WindowProcess) -> Option<WindowProcess> {
    let root = root_window(window.hwnd());
    if root.0.is_null() || is_ghost(root.0) { return None; }
    WindowProcess::read(root)
}

pub(super) fn same_root(hwnd: HWND, window: WindowProcess) -> bool {
    let root = root_window(hwnd);
    !root.0.is_null() && root == root_window(window.hwnd())
        && WindowProcess::read(hwnd).is_some_and(|proof| proof.process == window.process)
}

fn root_window(window: HWND) -> HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor};
    HWND(unsafe { GetAncestor(window.0, GA_ROOT) })
}

fn point_native(point: PhysicalPoint) -> POINT {
    POINT { x: point.x, y: point.y }
}

pub fn commit_sender(element: &IUIAutomationElement, expected: WindowProcess) -> bool {
    belongs_to(element, expected) && unsafe { element.CachedNativeWindowHandle() }.ok()
        .is_some_and(|window| same_root(window, expected))
}

fn belongs_to(element: &IUIAutomationElement, expected: WindowProcess) -> bool {
    unsafe { element.CachedProcessId() }.ok().is_some_and(|pid| pid as u32 == expected.process.pid.get())
        && WindowProcess::read(expected.hwnd()) == Some(expected)
}

fn cache_request(
    automation: &IUIAutomation2,
    properties: impl IntoIterator<Item = UIA_PROPERTY_ID>,
    text_pattern: bool,
) -> windows::core::Result<IUIAutomationCacheRequest> {
    let cache = unsafe { automation.CreateCacheRequest() }?;
    unsafe { cache.SetTreeScope(TreeScope_Element) }?;
    for property in properties { unsafe { cache.AddProperty(property) }?; }
    if text_pattern { unsafe { cache.AddPattern(UIA_TextPatternId) }?; }
    Ok(cache)
}

fn property_bool(element: &IUIAutomationElement, property: UIA_PROPERTY_ID) -> Option<bool> {
    let value = unsafe { element.GetCachedPropertyValueEx(property, true) }.ok()?;
    (value.vt() == VT_BOOL).then(|| unsafe { VariantToBoolean(&value) }.ok().map(|value| value.as_bool())).flatten()
}

/// The read-only state one pattern reports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Writability {
    Writable,
    ReadOnly,
    /// The attribute is unavailable.
    Undetermined,
    /// The Text range mixes read-only and writable text.
    Mixed,
}

impl Writability {
    fn from_read_only(read_only: bool) -> Self {
        if read_only { Self::ReadOnly } else { Self::Writable }
    }

    /// The `editable` value a read reports.
    fn reported(self) -> Option<bool> {
        match self {
            Self::Writable => Some(true),
            Self::ReadOnly => Some(false),
            Self::Undetermined | Self::Mixed => None,
        }
    }
}

/// D-4 (C2): the classification of a read element. Each undetermined case
/// has one decision point here.
fn classify(password: Option<bool>, editable: Writability, focused: bool) -> TextClassification {
    match (password, editable, focused) {
        (Some(true), _, _) => TextClassification::Password,
        (Some(false), Writability::Writable, true) => TextClassification::Editable,
        (Some(false), Writability::ReadOnly, true) => TextClassification::NonText,
        (Some(false), Writability::Mixed, true) => TextClassification::TypingUnknown,
        _ => TextClassification::PossiblySensitive,
    }
}

/// The request one read serves, the window it was approved for, and the
/// owner's invalidation check.
struct ReadGuard<'a, F> {
    request: Request,
    window: WindowProcess,
    invalid: &'a F,
}

impl<F: Fn(Request) -> Option<ReadUnavailable>> ReadGuard<'_, F> {
    /// Whether the owner invalidated the request.
    fn invalidated(&self) -> bool {
        (self.invalid)(self.request).is_some()
    }

    /// The owner's invalidation status, if any, as an error.
    fn valid(&self) -> Result<(), ReadUnavailable> {
        (self.invalid)(self.request).map_or(Ok(()), Err)
    }

    /// The checkpoint after a provider call: the deadline has not passed, the
    /// request is valid, and the query still finds the same window.
    fn live(&self) -> Result<(), ReadUnavailable> {
        if Instant::now() >= self.request.expires {
            return Err(ReadUnavailable::TimedOut);
        }
        self.valid()?;
        if self.request.query.window() != Some(self.window) {
            return Err(ReadUnavailable::Stale);
        }
        Ok(())
    }
}

/// After one provider call: an invalidated request or a provider timeout ends
/// the read; another failure leaves the attribute undetermined.
fn after_call<T>(
    result: windows::core::Result<T>, guard: &ReadGuard<'_, impl Fn(Request) -> Option<ReadUnavailable>>,
) -> Result<Option<T>, ReadUnavailable> {
    guard.valid()?;
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_timeout(&error) => Err(ReadUnavailable::TimedOut),
        Err(_) => Ok(None),
    }
}

fn is_timeout(error: &windows::core::Error) -> bool {
    error.code().0 as u32 == UIA_E_TIMEOUT
}

/// A provider timeout is TimedOut; other failures keep the caller's status.
fn call_status(error: &windows::core::Error, otherwise: ReadUnavailable) -> ReadUnavailable {
    if is_timeout(error) { ReadUnavailable::TimedOut } else { otherwise }
}

fn bounded(value: windows::core::Result<BSTR>, scrubber: &MetadataScrubber) -> Option<String> {
    let value = value.ok()?;
    let units: &[u16] = &value;
    Some(scrubber.scrub(&String::from_utf16_lossy(&units[..units.len().min(MAX_UIA_VALUE_UTF16)]), MAX_METADATA_BYTES))
}
