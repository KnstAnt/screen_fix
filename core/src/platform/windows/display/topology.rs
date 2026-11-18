use std::{
    collections::BTreeMap,
    num::NonZeroU32,
    sync::{Arc, RwLock, atomic::{AtomicU64, Ordering}},
    thread,
    time::{Duration, Instant},
};

use crate::{
    config::DisplayKey,
    package::metadata::MonitorCapture,
    protocol::{GeometryId, MonitorId, PhysicalRect, QpcTick},
    platform::windows::controller::WorkerSession,
};

use super::{AdapterId, Monitor, Survey, survey};
use crate::platform::windows::{ProbeError, clock, io_error, stage};

#[derive(Clone, PartialEq, Eq)]
pub(in crate::platform::windows) struct Output {
    pub key: DisplayKey,
    // Used only to distinguish an updated Windows monitor item. Native owners
    // always re-resolve the actual HMONITOR before creating a capture item.
    pub identity: usize,
    pub adapter: AdapterId,
    pub bounds: PhysicalRect,
    pub dpi_x: Option<NonZeroU32>,
    pub dpi_y: Option<NonZeroU32>,
    pub refresh: Option<(u32, NonZeroU32)>,
    pub rotation: i32,
    pub primary: bool,
    pub hdr: bool,
    pub sdr_white: Option<NonZeroU32>,
    pub unavailable: BTreeMap<crate::package::metadata::DisplayField, Option<i32>>,
}

impl Output {
    pub fn from_native(monitor: &Monitor) -> Result<Self, ProbeError> {
        let adapter = stage(unsafe { monitor.adapter.GetDesc1() }, "Identify the monitor's display adapter")?.AdapterLuid;
        Ok(Self {
            key: monitor.key.clone(), identity: monitor.handle.0 as usize, adapter: adapter.into(),
            bounds: monitor.bounds, dpi_x: monitor.dpi_x, dpi_y: monitor.dpi_y, refresh: monitor.refresh,
            rotation: monitor.rotation, primary: monitor.primary, hdr: monitor.hdr, sdr_white: monitor.sdr_white,
            unavailable: monitor.unavailable.clone(),
        })
    }

    /// The same Windows monitor item on the same adapter: its capture item
    /// and device carry over.
    pub fn same_item(&self, other: &Self) -> bool {
        self.identity == other.identity && self.adapter == other.adapter
    }
}

pub(in crate::platform::windows) struct Snapshot {
    pub received: QpcTick,
    /// Every active output; none when the enumeration failed or left an
    /// output unreadable, so the list proves nothing.
    pub outputs: Option<Vec<Output>>,
    /// The readable outputs of an enumeration that left some unreadable.
    partial: Vec<Output>,
    /// Outputs identified but not read: their presence is unknown.
    unreadable: Vec<DisplayKey>,
}

impl Snapshot {
    fn new(received: QpcTick, survey: Result<Survey, ProbeError>) -> Self {
        let read = survey.and_then(|survey| {
            let outputs = survey.monitors.iter().map(Output::from_native).collect::<Result<Vec<_>, _>>()?;
            Ok((outputs, survey.unreadable.into_iter().map(|(key, _)| key).collect::<Vec<_>>()))
        });
        match read {
            Ok((outputs, unreadable)) if unreadable.is_empty() => {
                Self { received, outputs: Some(outputs), partial: Vec::new(), unreadable }
            }
            Ok((partial, unreadable)) => Self { received, outputs: None, partial, unreadable },
            // A failed enumeration carries no proof of removal.
            Err(_) => Self { received, outputs: None, partial: Vec::new(), unreadable: Vec::new() },
        }
    }

    /// The output of `key`: `Some(None)` proves it absent, `None` proves
    /// nothing. Another output's unreadable properties do not affect it (V4).
    pub fn output(&self, key: &DisplayKey) -> Option<Option<&Output>> {
        let outputs = match &self.outputs {
            Some(outputs) => outputs,
            None if self.unreadable.is_empty() || self.unreadable.contains(key) => return None,
            None => &self.partial,
        };
        Some(outputs.iter().find(|output| output.key == *key))
    }
}

#[derive(Clone)]
pub(in crate::platform::windows) enum Route {
    Active(MonitorCapture),
    Reconfiguring(GeometryId),
    Missing(GeometryId),
}

impl Route {
    pub fn capture(&self) -> Option<&MonitorCapture> {
        match self { Self::Active(capture) => Some(capture), _ => None }
    }
}

#[derive(Default)]
pub(in crate::platform::windows) struct Topology {
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    routes: RwLock<BTreeMap<MonitorId, Route>>,
    routes_revision: AtomicU64,
}

impl Topology {
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.try_read().ok()?.clone()
    }

    pub fn capture(&self, capture: MonitorCapture) {
        self.set_route(capture.geometry.id.monitor, Route::Active(capture));
    }

    pub fn missing(&self, geometry: GeometryId) {
        self.set_route(geometry.monitor, Route::Missing(geometry));
    }

    pub fn reconfiguring(&self, geometry: GeometryId) {
        self.set_route(geometry.monitor, Route::Reconfiguring(geometry));
    }

    fn set_route(&self, monitor: MonitorId, route: Route) {
        self.routes.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(monitor, route);
        self.routes_revision.fetch_add(1, Ordering::Release);
    }

    pub fn revision(&self) -> u64 {
        self.routes_revision.load(Ordering::Acquire)
    }

    pub fn geometry(&self, monitor: MonitorId) -> Result<Option<GeometryId>, ()> {
        let routes = self.routes.try_read().map_err(|_| ())?;
        Ok(routes.get(&monitor).and_then(Route::capture).map(|capture| capture.geometry.id))
    }

    /// The active capture geometry of a monitor, with its bounds.
    pub fn capture_geometry(&self, monitor: MonitorId) -> Result<Option<crate::capture::frame::MonitorGeometry>, ()> {
        let routes = self.routes.try_read().map_err(|_| ())?;
        Ok(routes.get(&monitor).and_then(Route::capture).map(|capture| capture.geometry))
    }

    pub fn routes(&self) -> Option<BTreeMap<MonitorId, Route>> {
        Some(self.routes.try_read().ok()?.clone())
    }

    pub fn spawn<'scope>(
        &'scope self, scope: &'scope thread::Scope<'scope, '_>, worker: &'scope WorkerSession,
        stop: &'scope std::sync::atomic::AtomicBool,
    ) -> Result<(), ProbeError> {
        use std::sync::atomic::Ordering::Acquire;
        thread::Builder::new().name("recording-display-topology".into()).spawn_scoped(scope, move || {
            let mut next = Instant::now();
            let mut revision = worker.display_revision();
            while !stop.load(Acquire) {
                let now = Instant::now();
                let observed = worker.display_revision();
                if now >= next || revision != observed {
                    let received = clock::qpc();
                    let surveyed = survey();
                    if let Ok(received) = received {
                        let snapshot = Arc::new(Snapshot::new(received, surveyed));
                        *self.snapshot.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
                    }
                    revision = observed;
                    next = Instant::now() + Duration::from_secs(1);
                }
                thread::park_timeout(Duration::from_millis(50));
            }
        }).map_err(|error| io_error("Start the recording display topology owner", error))?;
        Ok(())
    }
}
