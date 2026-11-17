//! Native UI Automation reads and subscriptions on one owning MTA thread.

use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{Arc, atomic::{AtomicU64, Ordering::Release}},
    time::Instant,
};

use windows::{
    Win32::{
        Foundation::{HWND, POINT},
        System::{
            Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize},
            Variant::{VT_BOOL, VariantToBoolean},
        },
        UI::{Accessibility::*, WindowsAndMessaging::{IsHungAppWindow, WindowFromPhysicalPoint}},
    },
    core::{BSTR, Interface, Ref, implement},
};

use screen_fix_protocol::{
    BoundedList, FocusGeneration, PayloadLimit, PhysicalPoint, PhysicalRect, QpcTick, RequestId,
    ipc::elevated::{MAX_METADATA_BYTES, MAX_UIA_ANCESTORS, MAX_UIA_VALUE_UTF16, Metadata, UiaElement},
};

pub use screen_fix_protocol::ipc::elevated::{TextClassification, UiaStatus as Status};

use crate::{
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
    pub status: Status,
    pub target: TextClassification,
    pub element: Option<Element>,
}

impl Read {
    fn outcome(started: Option<QpcTick>, status: Status) -> Self {
        Self { started, completed: qpc(), status, target: TextClassification::PossiblySensitive, element: None }
    }
}

fn qpc() -> Option<QpcTick> {
    let mut value = 0;
    (unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut value) } != 0)
        .then_some(QpcTick(value))
}

struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn initialize() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
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
    _apartment: Apartment,
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
        let _apartment = Apartment::initialize().map_err(|_| windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_FAIL,
        ))?;
        let automation: IUIAutomation2 = unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;
        unsafe {
            automation.SetConnectionTimeout(250)?;
            automation.SetTransactionTimeout(500)?;
            automation.SetAutoSetFocus(false)?;
        }
        let identity = [UIA_ProcessIdPropertyId, UIA_NativeWindowHandlePropertyId];
        let traits = [
            UIA_HasKeyboardFocusPropertyId, UIA_ControlTypePropertyId, UIA_FrameworkIdPropertyId,
            UIA_BoundingRectanglePropertyId, UIA_IsPasswordPropertyId, UIA_IsKeyboardFocusablePropertyId,
            UIA_ValueIsReadOnlyPropertyId, UIA_IsTextEditPatternAvailablePropertyId,
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

    pub fn read(&mut self, request: Request, scrubber: &MetadataScrubber, invalid: &impl Fn(Request) -> Option<Status>) -> Read {
        self.read_inner(request, Some(scrubber), invalid)
    }

    pub fn editable_focus(&mut self, request: Request, invalid: &impl Fn(Request) -> Option<Status>) -> bool {
        self.scoped && matches!(request.query, Query::Focus { .. })
            && self.read_inner(request, None, invalid).target == TextClassification::Editable
    }

    fn read_inner(
        &mut self, request: Request, scrubber: Option<&MetadataScrubber>, invalid: &impl Fn(Request) -> Option<Status>,
    ) -> Read {
        let started = qpc();
        if Instant::now() >= request.expires {
            return Read::outcome(started, Status::TimedOut);
        }
        let Some(window) = request.query.window() else {
            return Read::outcome(started, Status::Stale);
        };
        let root = root_window(window.hwnd());
        if unsafe { IsHungAppWindow(window.hwnd()) }.as_bool()
            || (!root.0.is_null() && unsafe { IsHungAppWindow(root) }.as_bool())
        {
            return Read::outcome(started, Status::Hung);
        }
        if let Some(status) = invalid(request) {
            return Read::outcome(started, status);
        }
        let discovery = if self.scoped { &self.identification } else { &self.cache };
        let element = unsafe {
            match request.query {
                Query::Focus { .. } => self.automation.GetFocusedElementBuildCache(discovery),
                Query::Pointer { point, .. } => self.automation.ElementFromPointBuildCache(point_native(point), discovery),
            }
        };
        if Instant::now() >= request.expires {
            return Read::outcome(started, Status::TimedOut);
        }
        if let Some(status) = invalid(request) {
            return Read::outcome(started, status);
        }
        if request.query.window() != Some(window) {
            return Read::outcome(started, Status::Stale);
        }
        let Ok(element) = element else {
            return Read::outcome(started, Status::Unavailable);
        };
        if !self.matches(&element, window, request, invalid) {
            return Read::outcome(started, Status::OutsideApproval);
        }
        let native_password = match request.query {
            Query::Focus { native, .. } => native.and_then(|native| native.focus.password_style),
            Query::Pointer { .. } => password_style(window.hwnd()),
        };
        let element = if self.scoped {
            if let Some(status) = invalid(request) {
                return Read::outcome(started, status);
            }
            match unsafe { element.BuildUpdatedCache(&self.classification) } {
                Ok(element) if self.matches(&element, window, request, invalid) => element,
                _ => return Read::outcome(started, Status::Stale),
            }
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
        let has_keyboard_focus = property_bool(&element, UIA_HasKeyboardFocusPropertyId) == Some(true);
        let editable = if password == Some(false) {
            editable(&element, control_type, request, invalid)
        } else {
            None
        };
        let target = match (password, editable, has_keyboard_focus) {
            (Some(true), _, _) => TextClassification::Password,
            (Some(false), Some(true), true) => TextClassification::Editable,
            (Some(false), Some(false), true) => TextClassification::NonText,
            _ => TextClassification::PossiblySensitive,
        };
        let Some(scrubber) = scrubber else {
            return if Instant::now() < request.expires && invalid(request).is_none() && request.query.window() == Some(window) {
                Read { started, completed: qpc(), status: Status::Ready, target, element: None }
            } else {
                Read::outcome(started, Status::Stale)
            };
        };
        let allow_strings = password == Some(false)
            && (!matches!(request.query, Query::Focus { .. }) || target != TextClassification::PossiblySensitive);
        let strings = if allow_strings && invalid(request).is_none() {
            if self.scoped {
                unsafe { element.BuildUpdatedCache(&self.strings) }.ok().filter(|candidate| {
                    self.matches(candidate, window, request, invalid) && property_bool(candidate, UIA_IsPasswordPropertyId) == Some(false)
                })
            } else {
                Some(element.clone())
            }
        } else {
            None
        };
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
            self.ancestors(element.clone(), window, request, invalid, scrubber, &mut fields.ancestor_names);
        }
        if Instant::now() >= request.expires {
            return Read::outcome(started, Status::TimedOut);
        }
        if let Some(status) = invalid(request) {
            return Read::outcome(started, status);
        }
        if request.query.window() != Some(window) {
            return Read::outcome(started, Status::Stale);
        }
        if target == TextClassification::Editable && matches!(request.query, Query::Focus { .. })
            && property_bool(&element, UIA_IsTextEditPatternAvailablePropertyId) == Some(true)
        {
            self.pending_subscription = Some((request, element));
        }
        Read { started, completed: qpc(), status: Status::Ready, target, element: Some(fields) }
    }

    /// A candidate may be subscribed only after its result is queued for the owner.
    /// Taking it also drops obsolete or unqueued candidates.
    pub fn finish_read(
        &mut self, returned: bool, invalid: &impl Fn(Request) -> Option<Status>,
        desired: &Arc<AtomicU64>, revision: &Arc<AtomicU64>, commits: &Arc<CommitQueue>,
    ) {
        self.finish_scoped_read(returned, invalid, desired, revision, commits, None);
    }

    pub fn finish_scoped_read(
        &mut self, returned: bool, invalid: &impl Fn(Request) -> Option<Status>,
        desired: &Arc<AtomicU64>, revision: &Arc<AtomicU64>, commits: &Arc<CommitQueue>,
        scope: Option<CommitScope>,
    ) {
        let Some((request, element)) = self.pending_subscription.take() else { return };
        if !returned || Instant::now() >= request.expires || invalid(request).is_some() {
            return;
        }
        let Some(client) = &self.text_edit else { return };
        if self.subscription.is_some() || invalid(request).is_some() {
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

    pub fn remove_stale_subscription(&mut self, invalid: &impl Fn(Request) -> Option<Status>) {
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

    fn matches(&self, element: &IUIAutomationElement, window: WindowProcess, request: Request,
        invalid: &impl Fn(Request) -> Option<Status>,
    ) -> bool {
        if invalid(request).is_some() || !belongs_to(element, window) { return false; }
        if !self.scoped { return true; }
        let mut cursor = element.clone();
        let Ok(walker) = (unsafe { self.automation.ControlViewWalker() }) else { return false; };
        for index in 0..=MAX_UIA_ANCESTORS {
            if invalid(request).is_some() || !belongs_to(&cursor, window) { return false; }
            let Ok(hwnd) = (unsafe { cursor.CachedNativeWindowHandle() }) else { return false; };
            if !hwnd.0.is_null() {
                return same_root(hwnd, window) && invalid(request).is_none();
            }
            if index == MAX_UIA_ANCESTORS { return false; }
            let Ok(parent) = (unsafe { walker.GetParentElementBuildCache(&cursor, &self.identification) }) else { return false; };
            cursor = parent;
        }
        false
    }

    fn ancestors(&self, mut element: IUIAutomationElement, window: WindowProcess, request: Request,
        invalid: &impl Fn(Request) -> Option<Status>, scrubber: &MetadataScrubber, names: &mut Vec<String>,
    ) {
        let Ok(walker) = (unsafe { self.automation.ControlViewWalker() }) else { return };
        for _ in 0..MAX_UIA_ANCESTORS {
            if Instant::now() >= request.expires || invalid(request).is_some() {
                break;
            }
            let discovery = if self.scoped { &self.identification } else { &self.cache };
            let Ok(parent) = (unsafe { walker.GetParentElementBuildCache(&element, discovery) }) else { break };
            if !self.matches(&parent, window, request, invalid) { break; }
            let metadata = if self.scoped {
                let Ok(traits) = (unsafe { parent.BuildUpdatedCache(&self.classification) }) else { break; };
                if !self.matches(&traits, window, request, invalid)
                    || property_bool(&traits, UIA_IsPasswordPropertyId) != Some(false) { break; }
                let Ok(text) = (unsafe { parent.BuildUpdatedCache(&self.strings) }) else { break; };
                if !self.matches(&text, window, request, invalid) { break; }
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
                let current = FocusSnapshot::read(foreground)?;
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

fn editable(element: &IUIAutomationElement, control: Option<UIA_CONTROLTYPE_ID>, request: Request, invalid: &impl Fn(Request) -> Option<Status>) -> Option<bool> {
    if !property_bool(element, UIA_IsKeyboardFocusablePropertyId)? {
        return Some(false);
    }
    if let Some(read_only) = property_bool(element, UIA_ValueIsReadOnlyPropertyId) {
        return Some(!read_only);
    }
    if let Ok(text) = unsafe { element.GetCachedPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) } {
        if Instant::now() >= request.expires || invalid(request).is_some() {
            return None;
        }
        let range = unsafe { text.DocumentRange() }.ok()?;
        if Instant::now() >= request.expires || invalid(request).is_some() {
            return None;
        }
        let read_only = unsafe { range.GetAttributeValue(UIA_IsReadOnlyAttributeId) }.ok()?;
        return (read_only.vt() == VT_BOOL).then(|| unsafe { VariantToBoolean(&read_only) }.ok().map(|value| !value.as_bool())).flatten();
    }
    [
        UIA_ButtonControlTypeId,
        UIA_CheckBoxControlTypeId,
        UIA_RadioButtonControlTypeId,
        UIA_MenuControlTypeId,
        UIA_MenuItemControlTypeId,
        UIA_TabItemControlTypeId,
        UIA_TreeItemControlTypeId,
        UIA_ListItemControlTypeId,
        UIA_ToolBarControlTypeId,
    ]
    .contains(&control?)
    .then_some(false)
}

fn bounded(value: windows::core::Result<BSTR>, scrubber: &MetadataScrubber) -> Option<String> {
    let value = value.ok()?;
    let units: &[u16] = &value;
    Some(scrubber.scrub(&String::from_utf16_lossy(&units[..units.len().min(MAX_UIA_VALUE_UTF16)]), MAX_METADATA_BYTES))
}
