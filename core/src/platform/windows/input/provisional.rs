//! Provisional still requests of the bounded input prefix: the private
//! envelopes opened as input is matched, resolved as its rows are published.

use std::{collections::BTreeMap, sync::mpsc::SyncSender};

use crate::{
    capture::{
        input::{HeldInput, InputGap, PendingInput, PointerEvidence, ProvisionalInputChange},
        observation::{ProvisionalInputKind, ProvisionalInputObservation},
    },
    package::outcome::LossReason,
    platform::windows::controller::Receipt,
    protocol::{
        AdmissionEpoch, DeviceId, EventRef, Evidence, FocusGeneration, GeometryId, MonitorId, ObservationId, PhysicalPoint,
        QpcTick, SessionTimeUs, ipc::{ForegroundAtReceipt, elevated::VisitBinding},
    },
    util::raise,
};

use super::ProbeError;
use super::super::{context::Context, still_rows, stills};

/// Pointer rest after the last motion that triggers a pointer-rest still.
const POINTER_REST_US: i64 = 500_000;
/// Motion idle time before a motion opens a pre-motion observation (IMG-2).
const MOTION_IDLE_US: i64 = 300_000;

/// This map owns the private envelopes for the bounded input prefix. P10
/// can consume the same opening/resolution edges to drive actual retention.
pub(super) struct ProvisionalRequests {
    pending: BTreeMap<ObservationId, PrivateRequest>,
    endpoints: BTreeMap<MonitorId, stills::Requests>,
    rows: still_rows::Rows,
    motion: Option<MotionRest>,
    /// The latest motion receipt, for the IMG-2 pre-motion idle condition.
    last_motion: Option<SessionTimeUs>,
}

struct MotionRest {
    point: PhysicalPoint,
    geometry: GeometryId,
    proof: ForegroundAtReceipt,
    focus: FocusGeneration,
    admission: AdmissionEpoch,
    last: SessionTimeUs,
}

struct PrivateRequest {
    envelope: ProvisionalInputObservation,
    decision: Option<SyncSender<stills::Permission>>,
    deferred_source: Option<VisitBinding>,
    rows: Option<stills::EarlyRows>,
}

impl ProvisionalRequests {
    pub(super) fn new(endpoints: BTreeMap<MonitorId, stills::Requests>, rows: still_rows::Rows) -> Self {
        Self { pending: BTreeMap::new(), endpoints, rows, motion: None, last_motion: None }
    }

    pub(super) fn advance(&self, before: SessionTimeUs, input_live: bool) {
        let local = self.pending.values().map(|entry| entry.envelope.time.recv_t_us).min();
        let certified = local.map_or(before, |local| local.min(before));
        self.rows.advance(certified);
        if input_live {
            for endpoint in self.endpoints.values() {
                endpoint.input_progress(certified);
            }
        }
    }

    pub(super) fn approved_observation_gap(&self, at: SessionTimeUs, geometry: Evidence<GeometryId>, kind: Option<ProvisionalInputKind>,
        observation: Option<ObservationId>,
    ) {
        if let Some(kind) = kind {
            self.mark_action(at, geometry, kind);
        }
        // A motion that opened no pre-motion envelope reserved no still rows.
        let reserved = kind != Some(ProvisionalInputKind::PreMotion)
            || observation.and_then(|id| self.pending.get(&id)).is_some_and(|entry| entry.rows.is_some());
        if reserved && let Some(kind) = kind {
            // The gate had no confirmation capacity for the approved action.
            for _ in 0..stills::trigger_rows(kind) {
                self.rows.note_loss(at, LossReason::StagingPressure);
            }
        }
    }

    fn mark_action(&self, at: SessionTimeUs, geometry: Evidence<GeometryId>, kind: ProvisionalInputKind) {
        if kind == ProvisionalInputKind::PreMotion { return; }
        if let Evidence::Observed(geometry) = geometry {
            if let Some(endpoint) = self.endpoints.get(&geometry.monitor) {
                endpoint.action(at);
                return;
            }
        }
        // Unknown routing cannot certify quiet on any selected monitor.
        for endpoint in self.endpoints.values() {
            endpoint.action(at);
        }
    }

    pub(super) fn activity(&self, at: SessionTimeUs) {
        for endpoint in self.endpoints.values() {
            endpoint.activity(at);
        }
    }

    pub(super) fn pointer_moved(&mut self, at: SessionTimeUs, admission: AdmissionEpoch, proof: ForegroundAtReceipt,
        focus: Option<FocusGeneration>, pointer: Option<PointerEvidence>,
    ) {
        self.activity(at);
        self.motion = focus.zip(pointer).and_then(|(focus, pointer)| {
            let (Evidence::Observed(point), Evidence::Observed(geometry)) = (pointer.event_position, pointer.geometry) else {
                return None;
            };
            Some(MotionRest { point, geometry, proof, focus, admission, last: at })
        });
    }

    /// Forget the pending pointer rest; a reopening starts without one.
    pub(super) fn clear_motion(&mut self) {
        self.motion = None;
    }

    pub(super) fn poll_pointer_rest(&mut self, context: &mut Context<'_>, receipt: Receipt, observed: QpcTick, source: Option<VisitBinding>)
        -> Result<(), ProbeError>
    {
        let now = receipt.clock.time(observed)?;
        let Some(motion) = self.motion.take_if(|motion| now.0.saturating_sub(motion.last.0) >= POINTER_REST_US) else {
            return Ok(());
        };
        if motion.admission != receipt.boundary.epoch || !context.is_current(motion.proof, Some(motion.focus)) {
            return Ok(());
        }
        let Some(endpoint) = self.endpoints.get(&motion.geometry.monitor) else { return Ok(()) };
        let rows = &self.rows;
        let fallback = stills::fallback_row(stills::Trigger::PointerRest, stills::Role::Image, now,
            Evidence::Observed(motion.geometry), motion.admission, None,
            stills::DropReason::PointerRestHandoffUnavailable);
        let reservation = match rows.reserve(now, Some(fallback)) {
            Ok(reservation) => reservation,
            Err(reason) => {
                rows.note_loss(now, reason);
                return Ok(());
            }
        };
        let pointer = context.pointer_query(motion.point, motion.geometry, receipt.clock, observed, None)?;
        endpoint.pointer_rest(stills::Rest { reservation, at: now, admission: motion.admission,
            geometry: motion.geometry, proof: motion.proof, visit: source, pointer });
        Ok(())
    }

    pub(super) fn choose(&self, input: &HeldInput, geometry: Evidence<GeometryId>, proof: ForegroundAtReceipt, kind: ProvisionalInputKind)
        -> Option<ObservationId>
    {
        let correlated = &input.normalized.correlated;
        correlated.slots()
            .filter_map(|slot| slot.observation)
            .filter_map(|id| self.pending.get(&id).map(|entry| (id, entry)))
            .filter(|(_, entry)| entry.envelope.kind == kind)
            .max_by_key(|(_, entry)| (entry.envelope.geometry == geometry && entry.envelope.foreground_at_receipt == Some(proof)
                    && entry.decision.is_some(), entry.envelope.device.is_some(),
                std::cmp::Reverse(entry.envelope.time.recv_t_us)))
            .map(|(id, _)| id)
    }

    pub(super) fn published(
        &mut self, id: Option<ObservationId>, geometry: Evidence<GeometryId>, proof: ForegroundAtReceipt,
        device: Option<DeviceId>, at: SessionTimeUs, kind: ProvisionalInputKind, event: EventRef, admission: AdmissionEpoch,
    ) {
        self.mark_action(at, geometry, kind);
        let Some(mut entry) = id.and_then(|id| self.pending.remove(&id)) else {
            for _ in 0..stills::trigger_rows(kind) {
                self.rows.note_loss(at, LossReason::RetentionUnavailable);
            }
            return;
        };
        let Some(mut rows) = entry.rows.take() else { return; };
        let routable = match geometry {
            Evidence::Observed(id) if entry.envelope.geometry == geometry && entry.envelope.foreground_at_receipt == Some(proof) => {
                Some(id)
            }
            _ => None,
        };
        let reason = if routable.is_some() {
            stills::DropReason::ProvisionalRetentionUnavailable
        } else {
            stills::DropReason::MonitorRoutingUnknown
        };
        rows.approve(kind, event, geometry, admission, at, reason);
        if let Some((decision, geometry)) = entry.decision.zip(routable) {
            // A send failure drops the transferred reservations. Approved
            // fallback rows remain attached to those original receipt slots.
            let _ = decision.try_send(stills::Permission { event, geometry, proof, device, at, rows });
        }
    }

    pub(super) fn stage(&mut self, change: ProvisionalInputChange, source: Option<VisitBinding>) {
        match change {
            ProvisionalInputChange::Opened(request) => {
                // IMG-2: only the first motion after the idle period opens a
                // pre-motion envelope. Later motion stays pending without an
                // envelope or still rows, so its publication reserves nothing.
                let at = request.time.recv_t_us;
                let opens = request.kind != ProvisionalInputKind::PreMotion || {
                    let previous = self.last_motion;
                    raise(&mut self.last_motion, at);
                    previous.is_none_or(|last| at.0.saturating_sub(last.0) >= MOTION_IDLE_US)
                };
                let source = source.filter(|_| opens);
                let rows = opens.then(|| stills::EarlyRows::reserve(&self.rows, at, request.kind));
                let decision = if opens && source.is_none() { Self::open(&self.endpoints, request, None) } else { None };
                self.pending.insert(request.id, PrivateRequest { envelope: request, decision, deferred_source: source, rows });
            }
            ProvisionalInputChange::Dropped(id) => self.resolve(id),
        }
    }

    pub(super) fn open_deferred(&mut self, source: VisitBinding, before: SessionTimeUs) {
        let mut ready: Vec<_> = self.pending.values_mut().filter(|entry| entry.deferred_source == Some(source)
            && entry.envelope.time.recv_t_us < before).collect();
        ready.sort_by_key(|entry| (entry.envelope.time.recv_t_us, entry.envelope.id));
        for entry in ready {
            entry.deferred_source = None;
            entry.decision = Self::open(&self.endpoints, entry.envelope, Some(source));
        }
    }

    fn open(endpoints: &BTreeMap<MonitorId, stills::Requests>, request: ProvisionalInputObservation, visit: Option<VisitBinding>)
        -> Option<SyncSender<stills::Permission>>
    {
        let Evidence::Observed(geometry) = request.geometry else { return None; };
        endpoints.get(&geometry.monitor)?.open(request, visit)
    }

    /// End a provisional request. Its journal outcome is accounted by the
    /// publication owner; the still side only releases the retained request.
    pub(super) fn resolve(&mut self, id: ObservationId) {
        self.pending.remove(&id);
    }

    pub(super) fn note_gap(&mut self, gap: InputGap) {
        for endpoint in self.endpoints.values() {
            endpoint.action(gap.last());
        }
    }

    /// End the provisional requests of `records`.
    pub(super) fn resolve_records<'a>(&mut self, records: impl IntoIterator<Item = &'a PendingInput>) {
        for id in records.into_iter().filter_map(|record| record.observation) {
            self.resolve(id);
        }
    }

    pub(super) fn retire_except(&mut self, input: &HeldInput, permit: Option<ObservationId>) {
        let input = &input.normalized.correlated;
        for slot in input.slots() {
            if let Some(id) = slot.observation.filter(|id| Some(*id) != permit) {
                self.resolve(id);
            }
        }
    }

    pub(super) fn retire_input(&mut self, input: HeldInput) {
        let input = &input.normalized.correlated;
        self.resolve_records(input.slots());
    }
}
