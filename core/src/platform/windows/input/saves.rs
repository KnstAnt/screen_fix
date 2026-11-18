//! Fixture and checkpoint Saves: routing a confirmed fixture Save to the
//! still endpoints, and the elevated Saves deferred until their receipt is
//! certified.

use std::collections::VecDeque;

use crate::{
    capture::save::{
        ConfirmationSource, ConfirmedSave, FixtureSaveError, FixtureSaveResult, MonitorRoutingBasis, SaveEvidence, SaveRouting,
        SaveWindowContext,
    },
    platform::windows::controller::{FixtureNotice, MAX_FIXTURE_SAVES, Receipt},
    protocol::{QpcTick, RecordTime, RequestId, SessionTimeUs, ipc::elevated::VisitBinding},
};

use super::{ContextWriter, ProbeError, PumpEnv, unavailable};
use super::super::{context::Context, stills};

/// A deferred elevated Save not certified within this time is answered
/// without a selected context.
const DEFERRED_SAVE_TIMEOUT_US: i64 = 250_000;

/// The Saves of an elevated visit, answered once the certified prefix
/// covers their receipts.
#[derive(Default)]
pub(super) struct ElevatedSaves {
    fixtures: VecDeque<FixtureNotice>,
    checkpoints: VecDeque<(RequestId, QpcTick)>,
    /// Checkpoints refused by the full queue; each still gets its row (E4).
    refused: Vec<(RequestId, QpcTick)>,
}

impl ElevatedSaves {
    pub(super) fn fixture(&mut self, notice: FixtureNotice) {
        if self.fixtures.len() < MAX_FIXTURE_SAVES {
            self.fixtures.push_back(notice);
        } else {
            let _ = notice.reply.try_send(Err(FixtureSaveError::Busy));
        }
    }

    pub(super) fn checkpoint(&mut self, id: RequestId, at: QpcTick) {
        if self.checkpoints.len() < MAX_FIXTURE_SAVES {
            self.checkpoints.push_back((id, at));
        } else {
            self.refused.push((id, at));
        }
    }

    /// Resolve deferred Saves against the certified prefix. When `closing`
    /// (Pause or Stop, E4), this is the final poll: every remaining item gets
    /// its terminal outcome, `admission_closed`.
    pub(super) fn poll(
        &mut self, certified: Option<(VisitBinding, QpcTick)>, receipt: Receipt, observed: QpcTick, env: PumpEnv<'_>,
        native: &mut Context<'_>, context: &mut ContextWriter, closing: bool,
    ) -> Result<(), ProbeError> {
        let now = receipt.clock.time(observed)?;
        let expired = |at| receipt.clock.time(at).is_ok_and(|at| now.0.saturating_sub(at.0) >= DEFERRED_SAVE_TIMEOUT_US);
        for _ in 0..self.fixtures.len() {
            let Some(notice) = self.fixtures.pop_front() else { break; };
            let received = notice.receipt.qpc;
            let result = if receipt.boundary != notice.receipt.boundary || env.session.stop_requested() {
                Err(FixtureSaveError::AdmissionClosed)
            } else if expired(received) {
                Err(FixtureSaveError::UnapprovedContext)
            } else if let Some((binding, _)) = certified.filter(|(_, before)| received < *before) {
                route_fixture(&notice, env, native, receipt, Some(binding))
            } else if closing {
                Err(FixtureSaveError::AdmissionClosed)
            } else {
                self.fixtures.push_back(notice);
                continue;
            };
            let _ = notice.reply.try_send(result);
        }
        let Some(timing) = ContextWriter::timing(receipt, observed)? else { return Ok(()) };
        let mut row = |id: RequestId, at: QpcTick, selected: Result<SaveWindowContext, CheckpointUnavailable>| {
            let row = checkpoint_context_row(id, receipt.clock.time(at).ok(), selected)
                .map_err(|_| unavailable("Encode a checkpoint context row"))?;
            context.event(row, timing)
        };
        for (id, at) in std::mem::take(&mut self.refused) {
            row(id, at, Err(CheckpointUnavailable::QueueFull))?;
        }
        for _ in 0..self.checkpoints.len() {
            let Some((id, at)) = self.checkpoints.pop_front() else { break; };
            let available = certified.filter(|(_, before)| at < *before && !expired(at));
            if available.is_none() && !expired(at) && !closing {
                self.checkpoints.push_back((id, at));
                continue;
            }
            let selected = available.and_then(|(binding, _)| native.checkpoint_context(at, Some(binding)))
                .ok_or_else(|| if expired(at) { CheckpointUnavailable::TimedOut } else { CheckpointUnavailable::AdmissionClosed });
            row(id, at, selected)?;
        }
        Ok(())
    }

    /// Without a closing receipt no row can be written; this happens only
    /// when no admitted epoch closed, so no checkpoint can be pending.
    pub(super) fn close(&mut self) {
        for notice in self.fixtures.drain(..) {
            let _ = notice.reply.try_send(Err(FixtureSaveError::AdmissionClosed));
        }
        self.checkpoints.clear();
        self.refused.clear();
    }
}

/// Route a confirmed fixture Save received in the open interval `open` to
/// the still endpoints of its monitors; `source` is the elevated visit it
/// must belong to, none for the standard source.
pub(super) fn route_fixture(
    notice: &FixtureNotice, env: PumpEnv<'_>, native: &mut Context<'_>, open: Receipt, source: Option<VisitBinding>,
) -> FixtureSaveResult {
    let received = notice.receipt;
    if received.boundary != open.boundary {
        return Err(FixtureSaveError::AdmissionClosed);
    }
    let signal = &notice.signal;
    let (proof, visit) = native.fixture_save_proof(&signal.executable, received.qpc)
        .ok_or(FixtureSaveError::UnapprovedContext)?;
    if visit != source { return Err(FixtureSaveError::UnapprovedContext); }
    let routing = if let Some(monitors) = &signal.affected_monitors {
        SaveRouting::Monitors { monitors: monitors.clone(), basis: MonitorRoutingBasis::ApplicationGeometry }
    } else {
        native.checkpoint_context(received.qpc, source).ok_or(FixtureSaveError::InvalidRouting)?.fallback_routing()
    };
    let SaveRouting::Monitors { monitors, .. } = &routing else { return Err(FixtureSaveError::InvalidRouting) };
    let targets = monitors.as_slice().to_vec();
    let time = received.clock.time(received.qpc).map_err(|_| FixtureSaveError::ClockUnavailable)?;
    let evidence = SaveEvidence {
        id: notice.id, time: RecordTime { recv_t_us: time, source_t_us: None, source_uncertainty_us: None, coarse: None },
        kind: signal.kind, source: ConfirmationSource::Fixture, executable: signal.executable.clone(), request: None,
    };
    let confirmation = ConfirmedSave::new(env.layout.id().clone(), env.session.config(), evidence, routing)
        .map_err(|_| FixtureSaveError::InvalidRouting)?;
    let mut admitted = 0;
    for &monitor in &targets {
        let Some(geometry) = native.save_geometry(monitor, received.qpc) else { continue; };
        let Some(endpoint) = env.still_endpoints.get(&monitor) else { continue; };
        let Some(confirmation) = confirmation.for_monitor(monitor) else { continue; };
        let (trigger, role) = stills::save_slot(confirmation.kind());
        let fallback = stills::fallback_row(trigger, role, time, crate::protocol::Evidence::Observed(geometry),
            open.boundary.epoch, None, stills::DropReason::SaveHandoffUnavailable).with_save(&confirmation);
        let reservation = match env.still_rows.reserve(time, Some(fallback)) {
            Ok(reservation) => reservation,
            Err(reason) => {
                env.still_rows.note_loss(time, reason);
                continue;
            }
        };
        if endpoint.confirmed_save(stills::Save { reservation, confirmation, geometry, admission: open.boundary.epoch, proof, visit }) {
            admitted += 1;
        }
    }
    if admitted == targets.len() {
        Ok(notice.id)
    } else if admitted == 0 {
        Err(FixtureSaveError::MonitorUnavailable)
    } else {
        Err(FixtureSaveError::PartiallyRouted { id: notice.id, admitted })
    }
}

/// Why a checkpoint Save has no selected context.
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointUnavailable {
    /// The checkpoint queue was full when it was requested.
    QueueFull,
    TimedOut,
    AdmissionClosed,
}

/// Whether a checkpoint Save's context was observed.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ContextStatus {
    Observed,
    Unavailable,
}

/// The context row of a checkpoint Save's selected context, or of why it has none.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename = "checkpoint_context")]
struct CheckpointContextRow<S> {
    request_id: RequestId,
    control_receipt_t_us: Option<SessionTimeUs>,
    save_context: Option<S>,
    status: ContextStatus,
    reason: Option<CheckpointUnavailable>,
}

fn checkpoint_context_row<S: serde::Serialize>(
    id: RequestId, control_receipt: Option<SessionTimeUs>, selected: Result<S, CheckpointUnavailable>,
) -> serde_json::Result<serde_json::Value> {
    let status = if selected.is_ok() { ContextStatus::Observed } else { ContextStatus::Unavailable };
    let (save_context, reason) = match selected {
        Ok(selected) => (Some(selected), None),
        Err(reason) => (None, Some(reason)),
    };
    serde_json::to_value(CheckpointContextRow { request_id: id, control_receipt_t_us: control_receipt, save_context, status, reason })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn checkpoint_context_bytes() -> serde_json::Result<()> {
        let row = checkpoint_context_row(RequestId(5), Some(SessionTimeUs(300)), Err::<Value, _>(CheckpointUnavailable::QueueFull))?;
        assert_eq!(serde_json::to_string(&row)?,
            r#"{"control_receipt_t_us":300,"kind":"checkpoint_context","reason":"queue_full","request_id":5,"save_context":null,"status":"unavailable"}"#);
        let row = checkpoint_context_row(RequestId(6), None, Ok(json!({ "id": 1 })))?;
        assert_eq!(serde_json::to_string(&row)?,
            r#"{"control_receipt_t_us":null,"kind":"checkpoint_context","reason":null,"request_id":6,"save_context":{"id":1},"status":"observed"}"#);
        Ok(())
    }
}
