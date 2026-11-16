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
    session::windows::WorkerSession,
};

use super::{Monitor, enumerate};
use crate::platform::windows::{ProbeError, clock, mp4::io_error, stage};

#[derive(Clone, PartialEq, Eq)]
pub(in crate::platform::windows) struct Output {
    pub key: DisplayKey,
    // Used only to distinguish an updated Windows monitor item. Native owners
    // always re-resolve the actual HMONITOR before creating a capture item.
    pub identity: usize,
    pub adapter: (u32, i32),
    pub bounds: PhysicalRect,
    pub dpi_x: NonZeroU32,
    pub dpi_y: NonZeroU32,
    pub refresh: (u32, NonZeroU32),
    pub rotation: i32,
    pub primary: bool,
    pub hdr: bool,
    pub sdr_white: Option<NonZeroU32>,
}

impl Output {
    pub fn from_native(monitor: &Monitor) -> Result<Self, ProbeError> {
        let adapter = stage(unsafe { monitor.adapter.GetDesc1() }, "Identify the monitor's display adapter")?.AdapterLuid;
        Ok(Self {
            key: monitor.key.clone(), identity: monitor.handle.0 as usize, adapter: (adapter.LowPart, adapter.HighPart),
            bounds: monitor.bounds, dpi_x: monitor.dpi_x, dpi_y: monitor.dpi_y, refresh: monitor.refresh,
            rotation: monitor.rotation, primary: monitor.primary, hdr: monitor.hdr, sdr_white: monitor.sdr_white,
        })
    }
}

pub(in crate::platform::windows) struct Snapshot {
    pub received: QpcTick,
    pub outputs: Option<Vec<Output>>,
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
                    let outputs = enumerate().and_then(|displays| displays.iter().map(Output::from_native).collect());
                    if let Ok(received) = received {
                        // A failed enumeration carries no proof of removal.
                        let snapshot = Arc::new(Snapshot { received, outputs: outputs.ok() });
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
