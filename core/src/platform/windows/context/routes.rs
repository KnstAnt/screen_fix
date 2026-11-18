use std::{collections::BTreeMap, time::{Duration, Instant}};

use serde_json::{Value, json};

use crate::{
    capture::frame::MonitorGeometry,
    config::ResolvedCaptureConfig,
    protocol::{Evidence, FocusGeneration, GeometryId, MonitorId, PhysicalPoint, PhysicalRect, QpcTick, UnavailableReason,
        ipc::ForegroundAtReceipt},
};

use super::Visit;
use super::super::{clock, display::topology::{Route, Topology}};

const DISPLAY_REFRESH: Duration = Duration::from_secs(1);

/// The capture routes of the selected displays, their display rows and the
/// current visit's bounds and geometry on those routes. A route change
/// invalidates the visit's geometry.
pub(super) struct DisplayRoutes<'a> {
    config: &'a ResolvedCaptureConfig,
    topology: &'a Topology,
    next_refresh: Instant,
    topology_seen: Option<(QpcTick, u64)>,
    unavailable: BTreeMap<MonitorId, GeometryId>,
    reconfiguring: BTreeMap<MonitorId, GeometryId>,
    rows: Option<Vec<Value>>,
    displays_unavailable: bool,
    routable: Vec<MonitorGeometry>,
    /// When the current routes were established.
    since: Option<QpcTick>,
    visit_bounds: Option<(FocusGeneration, PhysicalRect, QpcTick)>,
    visit_geometry: Option<(FocusGeneration, GeometryId, QpcTick)>,
}

impl<'a> DisplayRoutes<'a> {
    pub fn new(config: &'a ResolvedCaptureConfig, topology: &'a Topology, now: Instant) -> Self {
        Self {
            config, topology, next_refresh: now, topology_seen: None, unavailable: BTreeMap::new(),
            reconfiguring: BTreeMap::new(), rows: None, displays_unavailable: false, routable: Vec::new(), since: None,
            visit_bounds: None, visit_geometry: None,
        }
    }

    /// Forget the routes, the written display rows and the visit geometry at
    /// a pause. The missing and reconfiguring monitors and the display
    /// availability are kept, so a resumed poll reports only their changes.
    pub fn reset(&mut self) {
        self.visit_geometry = None;
        self.visit_bounds = None;
        self.routable.clear();
        self.since = None;
        self.rows = None;
        self.topology_seen = None;
    }

    pub fn resume(&mut self, now: Instant) {
        self.next_refresh = now;
    }

    /// Whether a display refresh is due at `now` or was `notified`. A due
    /// refresh schedules the next periodic one.
    pub fn due(&mut self, now: Instant, notified: bool) -> bool {
        let due = now >= self.next_refresh || notified;
        if due {
            self.next_refresh = now + DISPLAY_REFRESH;
        }
        due
    }

    pub fn routable(&self) -> &[MonitorGeometry] {
        &self.routable
    }

    fn established_by(&self, at: QpcTick) -> bool {
        self.since.is_some_and(|since| since <= at)
    }

    /// The only route that contains `point`.
    pub fn point_geometry(&self, point: PhysicalPoint) -> Option<GeometryId> {
        let mut captures = self.routable.iter().filter(|capture| capture.bounds.contains(point));
        let geometry = captures.next()?.id;
        captures.next().is_none().then_some(geometry)
    }

    /// The only route that intersects `bounds`.
    pub fn only_geometry(&self, bounds: PhysicalRect) -> Option<GeometryId> {
        let mut candidates = self.routable.iter().filter(|monitor| monitor.bounds.intersects(bounds));
        let geometry = candidates.next()?.id;
        candidates.next().is_none().then_some(geometry)
    }

    pub fn save_geometry(&self, monitor: MonitorId, at: QpcTick) -> Option<GeometryId> {
        if !self.established_by(at) { return None; }
        self.routable.iter().find(|geometry| geometry.id.monitor == monitor).map(|geometry| geometry.id)
    }

    pub fn pointer_geometry_at(&self, point: PhysicalPoint, at: QpcTick) -> Evidence<GeometryId> {
        if !self.established_by(at) {
            return Evidence::Unavailable(UnavailableReason::NotObserved);
        }
        Evidence::from_option(self.point_geometry(point), UnavailableReason::NotObserved)
    }

    /// The geometry of the `current` visit observed by `at`, if `proof` is that visit.
    pub fn window_geometry_at(&self, current: Option<Visit>, proof: ForegroundAtReceipt, at: QpcTick) -> Evidence<GeometryId> {
        let Some((generation, geometry, observed)) = self.visit_geometry else {
            return Evidence::Unavailable(UnavailableReason::NotObserved);
        };
        if observed <= at && current.is_some_and(|visit| visit.generation == generation
            && visit.started_at <= at && visit.receipt_proof() == proof)
        {
            Evidence::Observed(geometry)
        } else {
            Evidence::Unavailable(UnavailableReason::NotObserved)
        }
    }

    /// The routes that the bounds of visit `generation`, observed by `at`, intersect.
    pub fn visit_geometries(&self, generation: FocusGeneration, at: QpcTick) -> Vec<GeometryId> {
        let bounds = self.visit_bounds.filter(|(known, _, observed)| {
            *known == generation && *observed <= at && self.established_by(at)
        });
        bounds.map_or_else(Vec::new, |(_, bounds, _)| {
            self.routable.iter().filter(|monitor| monitor.bounds.intersects(bounds)).map(|monitor| monitor.id).collect()
        })
    }

    pub fn has_visit_geometry(&self) -> bool {
        self.visit_geometry.is_some()
    }

    /// Forget the visit's bounds and geometry: the visit or its window moved.
    pub fn forget_visit(&mut self) {
        self.visit_geometry = None;
        self.visit_bounds = None;
    }

    pub fn observe_bounds(&mut self, visit: Visit, bounds: Option<PhysicalRect>, measured: QpcTick) {
        if !self.visit_bounds.is_some_and(|(generation, known, _)| generation == visit.generation && Some(known) == bounds) {
            self.visit_bounds = bounds.map(|bounds| (visit.generation, bounds, measured));
        }
        if let Some(geometry) = bounds.and_then(|bounds| self.only_geometry(bounds)) {
            if !self.visit_geometry.is_some_and(|(generation, id, _)| generation == visit.generation && id == geometry) {
                self.visit_geometry = Some((visit.generation, geometry, measured));
            }
        } else {
            self.visit_geometry = None;
        }
    }

    /// Route and display rows for a topology change, or for every route when `forced`.
    pub fn refresh(&mut self, at: QpcTick, rows: &mut Vec<Value>, forced: bool) {
        let Some(snapshot) = self.topology.snapshot() else { return };
        let revision = self.topology.revision();
        let observed = (snapshot.received, revision);
        if !forced && self.topology_seen == Some(observed) { return; }
        let Some(states) = self.topology.routes() else { return };
        self.topology_seen = Some(observed);
        for (&monitor, route) in &states {
            match route {
                Route::Missing(geometry) => {
                    self.reconfiguring.remove(&monitor);
                    if self.unavailable.insert(monitor, *geometry) != Some(*geometry) {
                        rows.push(json!({ "kind": "monitor_missing", "monitor": monitor, "geometry": geometry }));
                    }
                }
                Route::Active(capture) => {
                    self.reconfiguring.remove(&monitor);
                    if self.unavailable.remove(&monitor).is_some() {
                        rows.push(json!({ "kind": "monitor_returned", "monitor": monitor, "geometry": capture.geometry.id }));
                    }
                }
                Route::Reconfiguring(geometry) => {
                    if self.reconfiguring.insert(monitor, *geometry) != Some(*geometry)
                        && !self.unavailable.contains_key(&monitor)
                    {
                        rows.push(json!({ "kind": "display_reconfiguring", "monitor": monitor, "geometry": geometry }));
                    }
                }
            }
        }
        if let Some(monitors) = &snapshot.outputs {
            let mut routes = Vec::new();
            let displays: Vec<_> = monitors.iter().map(|monitor| {
                let selected = self.config.monitors().iter().find(|entry| entry.key == monitor.key);
                let geometry = selected.and_then(|entry| states.get(&entry.id).and_then(Route::capture))
                    .filter(|capture| {
                        capture.geometry.bounds == monitor.bounds && capture.geometry.dpi_x == monitor.dpi_x
                            && capture.geometry.dpi_y == monitor.dpi_y && capture.rotation == monitor.rotation
                            && capture.hdr == monitor.hdr && capture.sdr_white_level == monitor.sdr_white
                            && capture.refresh_numerator == monitor.refresh.map(|refresh| refresh.0)
                            && capture.refresh_denominator == monitor.refresh.map(|refresh| refresh.1)
                    }).map(|capture| capture.geometry);
                routes.extend(geometry);
                json!({
                    "display_key": monitor.key.as_str(), "selected_monitor": selected.map(|entry| entry.id),
                    "geometry": geometry.map(|value| value.id), "bounds": monitor.bounds,
                    "dpi_x": monitor.dpi_x, "dpi_y": monitor.dpi_y, "refresh": monitor.refresh,
                    "rotation": monitor.rotation, "hdr": monitor.hdr, "primary": monitor.primary,
                    "unavailable": &monitor.unavailable,
                })
            }).collect();
            if self.displays_unavailable || self.rows.as_ref() != Some(&displays) {
                rows.push(json!({ "kind": "display", "available": true, "monitors": &displays }));
                self.rows = Some(displays);
            }
            if self.routable != routes || self.since.is_none() {
                self.routable = routes;
                self.since = clock::qpc().ok().map(|sample| sample.max(at));
                self.forget_visit();
            }
            self.displays_unavailable = false;
        } else if !self.displays_unavailable {
            self.displays_unavailable = true;
            self.routable.clear();
            self.since = None;
            self.forget_visit();
            rows.push(json!({ "kind": "display", "available": false }));
        }
    }
}
