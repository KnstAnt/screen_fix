//! The elevated input lane: an open visit's certified input prefix, its
//! coverage losses and interruptions, and its context rows.

use std::time::Duration;

use screen_fix_native_observation::LossSpan;
use serde_json::json;

use crate::{
    capture::input::HeldBoundary,
    package::{metadata::SourceBlindReason, outcome::{LossEvidence, LossReason}},
    platform::windows::controller::{Receipt, WorkerSession},
    protocol::{QpcTick, SessionTimeUs, ipc::elevated::runtime::CoverageReason},
    session::AdmissionBoundary,
};

use super::{
    ContextWriter, ProbeError, Pump, keep_first_error, publish_staging_loss, receiver::Receiver, rows::source_loss_row,
    unavailable,
};
use super::super::{
    context::{Context, Invalidation},
    elevated::{CoverageLossReport, InputCertificate, InputStagingLoss, Selector},
};

/// An open elevated visit whose certified input lags its observation by
/// this much fails the source (witness).
const MAX_ELEVATED_CERTIFICATION_LAG_US: i64 = 250_000;

impl Pump<'_> {
    /// Service the elevated source in an open admission interval: its
    /// invalidations, losses and interruptions, the opening of a visit, and
    /// the certified prefix of the current visit.
    pub(super) fn service_elevated(&mut self, receipt: Receipt, observed_at: QpcTick) -> Result<(), ProbeError> {
        let session = self.env.session;
        let epoch = receipt.boundary.epoch;
        let admission = session.active_source(epoch);
        self.check_open_visit(receipt);
        let loss = self.source.take_loss();
        let staging_loss = self.source.take_staging_loss();
        let context_pressure = self.source.take_context_pressure();
        let overflowed = staging_loss.is_some() || context_pressure.is_some();
        if overflowed { self.receiver.note_native_gap(); }
        if let Some(loss) = &staging_loss {
            note_staging_loss(&mut self.receiver, loss, receipt)?;
        }
        for gap in self.source.take_prior_gaps() {
            self.receiver.note_elevated_loss(gap.cause.into(), Some(gap), gap.first_qpc, receipt)?;
        }
        // Elevated context evidence the Recorder's staging could not hold is a
        // typed context loss (E3, D-1).
        if let Some(gap) = context_pressure {
            self.native_context.note_loss(LossReason::StagingPressure, LossSpan { count: gap.count(), first: gap.first_qpc, last: gap.last_qpc });
        }
        if let Some(span) = self.source.take_optional_loss() {
            self.native_context.note_loss(LossReason::StagingPressure, span);
        }
        if let Some(loss) = staging_loss && let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
            publish_staging_loss(&mut self.context, &loss, timing)?;
        }
        if let Some(loss) = loss {
            self.report_coverage_loss(loss, receipt, observed_at)?;
        }
        if let Some(interrupted) = self.source.take_interruption() {
            self.interrupt_visit(interrupted, loss, overflowed, receipt, observed_at)?;
        }
        let live = self.source.current_visit();
        if !live && let Some(visit) = self.source.open_visit() {
            self.source.observe_focus_loss(self.source.certified_before().unwrap_or(visit.completed));
        }
        let Some(opened) = self.source.open_visit().filter(|visit| live && visit.binding.epoch == epoch
            && session.elevated_active(epoch)) else {
            self.leave_visit();
            return Ok(());
        };
        if self.elevated_visit != Some(opened.binding) {
            self.source.note_opening(receipt.boundary, opened);
            self.receiver.select_source(Some(opened.binding));
            let visit = self.native_context.activate_elevated(opened.binding, opened.focus, opened.completed)?;
            self.receiver.elevated_synchronization(receipt, opened.completed, opened.held, visit)?;
            self.elevated_visit = Some(opened.binding);
            if let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
                self.context.boundary(json!({
                    "kind": "input_source_coverage", "source": "elevated", "coverage": opened.coverage,
                    "admission_epoch": opened.binding.epoch, "registration": opened.registration,
                }), timing)?;
            }
        }
        if let Some(recovered) = self.source.take_recovered()
            && recovered.binding == opened.binding && let Some(visit) = self.native_context.visit()
        {
            self.receiver.recovered_synchronization(receipt, recovered.completed, recovered.snapshot, visit)?;
        }
        let rows = match self.native_context.poll_elevated_queries(&mut self.source, observed_at, receipt, &admission) {
            Ok(rows) if self.visit_live(receipt) => rows,
            Ok(_) => {
                self.leave_visit();
                if !self.source.current_visit() {
                    self.source.observe_focus_loss(self.source.certified_before().unwrap_or(opened.completed));
                }
                return Ok(());
            }
            Err(failure) => {
                self.env.fail_source(&mut self.source, failure);
                return Ok(());
            }
        };
        let observed = receipt.clock.time(observed_at)?;
        let since = receipt.clock.time(opened.coverage.before_qpc)?;
        let certificate = self.source.input_certificate(None);
        let certified = certificate.map(|certificate| receipt.clock.time(certificate.before)).transpose()?;
        if observed.0.saturating_sub(certified.unwrap_or(since).0) >= MAX_ELEVATED_CERTIFICATION_LAG_US {
            self.env.fail_source(&mut self.source, screen_fix_elevated::Failure::Witness);
            return Ok(());
        }
        let frames = self.env.frame_policy;
        let (Some(certificate), Some(through)) = (certificate, certified) else {
            if self.visit_live(receipt) {
                if self.source.media_open() {
                    frames.publish_elevated(opened.binding, since, observed, since, receipt.boundary, receipt.clock);
                }
                if let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
                    for row in rows {
                        self.context.event(row, timing)?;
                    }
                }
            } else {
                frames.clear();
            }
            return Ok(());
        };
        if !self.source.current_visit() {
            self.leave_visit();
            self.source.observe_focus_loss(certificate.before);
            return Ok(());
        }
        if !session.elevated_active(epoch) {
            self.leave_visit();
            return Ok(());
        }
        if !accept_elevated_certificate(&mut self.source, certificate, &mut self.receiver, &mut self.native_context, receipt, None)? {
            return Ok(());
        }
        self.receiver.advance(through);
        if close_failed_hooks(&mut self.source, &mut self.receiver, &mut self.native_context, session, receipt)?.is_some() {
            return Ok(());
        }
        if !self.visit_live(receipt) {
            self.leave_visit();
            return Ok(());
        }
        // Materialization follows the actual Recorder receipts in this poll.
        if let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
            for row in rows {
                self.context.event(row, timing)?;
            }
        }
        // A delivered certificate can still contain records awaiting correlation.
        let frame_before = self.receiver.frame_before().map_or(since, |before| before.min(through));
        if self.source.media_open() {
            frames.publish_elevated(opened.binding, since, observed, frame_before, receipt.boundary, receipt.clock);
            self.receiver.open_elevated_stills(opened.binding, frame_before);
        }
        self.stage_confirmed_text(receipt, observed_at);
        self.receiver.publish(&mut self.native_context, &mut self.input, Receipt { qpc: observed_at, ..receipt }, through, false)?;
        if self.source.media_open() && record_elevated_coverage(session, receipt, &self.receiver, certificate, through)? {
            self.source.finish_blind(receipt.boundary, receipt.clock, certificate.opening);
            self.source.certified_capture(epoch);
            session.source_enhanced(epoch);
        }
        // Pointer rest is decided only through a whole microsecond and one tick
        // before the certificate, so its session time is certified as well.
        let ticks_per_us = receipt.clock.qpc_frequency.ticks(Duration::from_micros(1)).unwrap_or(i64::MAX);
        let last_certified = QpcTick(certificate.before.0.saturating_sub(ticks_per_us.saturating_add(1)));
        self.receiver.poll_pointer_rest(&mut self.native_context, receipt, last_certified)?;
        Ok(())
    }

    /// Whether the current visit is still the source's and its epoch's.
    fn visit_live(&self, receipt: Receipt) -> bool {
        self.source.current_visit() && self.env.session.elevated_active(receipt.boundary.epoch)
    }

    /// The visit is no longer live: no frame or clipboard evidence is kept for it.
    fn leave_visit(&mut self) {
        self.env.frame_policy.clear();
        self.native_context.discard_elevated_clipboard();
    }

    /// Close, suspend or fail the open visit on the context's invalidation;
    /// without an open visit, its unbound notifications are discarded.
    fn check_open_visit(&mut self, receipt: Receipt) {
        let Some(opened) = self.source.open_visit() else {
            self.native_context.discard_unbound_notifications();
            return;
        };
        match self.native_context.elevated_invalidation(opened.binding, opened.focus, opened.completed, receipt) {
            Ok(Some(Invalidation::Foreground(before))) => {
                let closed = self.source.close(before.max(receipt.boundary.qpc));
                self.env.frame_policy.clear();
                self.env.session.require_source_change(receipt.boundary.epoch);
                if let Err(failure) = closed {
                    self.env.session.fail_source(self.source.stage(), failure);
                    self.source.cancel();
                }
            }
            Ok(Some(Invalidation::Focus(before))) => self.source.observe_focus_loss(before),
            Err(failure) => self.env.fail_source(&mut self.source, failure),
            Ok(None) => {}
        }
    }

    /// The input-stream marker and the context `input_source_loss` row of an
    /// open visit's coverage loss.
    fn report_coverage_loss(&mut self, loss: CoverageLossReport, receipt: Receipt, observed_at: QpcTick) -> Result<(), ProbeError> {
        if loss.gap.is_some() {
            self.receiver.note_native_gap();
        }
        let reason = match (loss.cause, loss.gap) {
            (_, Some(gap)) => gap.cause.into(),
            (CoverageReason::NativeGap, None) => LossReason::NativeQueueOverflow,
            (CoverageReason::UncertifiedSource, None) => LossReason::UncertifiedSource,
            (CoverageReason::Opening | CoverageReason::HeldUnknown, None) => LossReason::ResyncPending,
        };
        self.receiver.note_elevated_loss(reason, loss.gap, loss.before, receipt)?;
        if let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
            let row = source_loss_row(loss.binding, loss.before, loss.cause, loss.gap)
                .map_err(|_| unavailable("Encode an input source loss row"))?;
            self.context.boundary(row, timing)?;
        }
        Ok(())
    }

    /// The visit's input was interrupted at `interrupted`: a source blind
    /// interval opens, and the applied visit's certified prefix is published
    /// and closed (E1).
    fn interrupt_visit(
        &mut self, interrupted: QpcTick, loss: Option<CoverageLossReport>, overflowed: bool, receipt: Receipt,
        observed_at: QpcTick,
    ) -> Result<(), ProbeError> {
        let session = self.env.session;
        let reason = if session.source_failure().is_some() {
            SourceBlindReason::SourceFailure
        } else if overflowed {
            SourceBlindReason::RecorderStaging
        } else if let Some(loss) = loss {
            SourceBlindReason::ElevatedOpening(loss.cause)
        } else {
            SourceBlindReason::FocusSuspended
        };
        self.source.note_blind(receipt.boundary.epoch, interrupted, reason);
        session.source_resynchronizing(receipt.boundary.epoch);
        self.env.frame_policy.clear();
        let Some(previous) = self.elevated_visit.take() else { return Ok(()) };
        let certified = if let Some(certificate) = self.source.input_certificate(Some(interrupted))
            .filter(|certificate| certificate.visit == previous)
            && accept_elevated_certificate(&mut self.source, certificate, &mut self.receiver, &mut self.native_context, receipt, None)?
        {
            self.receiver.advance(receipt.clock.time(certificate.before)?);
            let before = close_failed_hooks(&mut self.source, &mut self.receiver, &mut self.native_context, session, receipt)?
                .map_or(certificate.before, |hook| hook.min(certificate.before));
            let through = receipt.clock.time(before)?;
            record_elevated_coverage(session, receipt, &self.receiver, certificate, through)?;
            before
        } else {
            receipt.boundary.qpc
        };
        let cutoff = receipt.clock.time(certified.max(receipt.boundary.qpc))?;
        let through = self.input.raised(cutoff);
        let boundary = if loss.is_some() || overflowed { HeldBoundary::InputGap } else { HeldBoundary::FocusChanged };
        self.receiver.interrupt_certified(cutoff, through, boundary);
        self.receiver.publish(&mut self.native_context, &mut self.input, receipt, through, true)?;
        self.receiver.finish_still_prefix(through);
        self.native_context.pause(interrupted)?;
        if let Some(timing) = ContextWriter::timing(receipt, observed_at)? {
            self.context.boundaries(self.native_context.close_hung_observation(), timing)?;
        }
        self.receiver.reopen(through, false);
        // E1: records past the certified prefix are counted, never dropped silently.
        let mut counted = Ok(());
        self.source.take_uncertified(previous, |record| {
            keep_first_error(&mut counted, || self.receiver.note_uncertified(record, receipt));
        });
        counted
    }
}

pub(super) fn record_elevated_coverage(
    session: &WorkerSession, receipt: Receipt, receiver: &Receiver, certificate: InputCertificate, through: SessionTimeUs,
) -> Result<bool, ProbeError> {
    let opening = receipt.clock.time(certificate.opening.max(receipt.boundary.qpc))?;
    let Some(start) = opening.0.checked_add(1).map(SessionTimeUs) else { return Ok(false); };
    let Some(before) = receiver.unresolved_before().map(|before| before.min(through)).filter(|before| *before > start)
        else { return Ok(false); };
    session.record_source_coverage(receipt.boundary.epoch, Some(certificate.visit), start, before);
    Ok(true)
}

/// Accept the certified elevated prefix through `closing` at a pause or
/// stop. The certified receipt; the admission boundary when none is.
pub(super) fn drain_elevated_prefix(
    source: &mut Selector<'_>, receiver: &mut Receiver, native: &mut Context<'_>,
    receipt: Receipt, closing: AdmissionBoundary, session: &WorkerSession,
) -> Result<SessionTimeUs, ProbeError> {
    let Some(certificate) = source.input_certificate(Some(closing.qpc))
        .filter(|_| !source.awaiting_close()) else { return Ok(receipt.boundary.time); };
    let time = receipt.clock.time(certificate.before)?;
    let closing = AdmissionBoundary { qpc: certificate.before, time, ..closing };
    if !accept_elevated_certificate(source, certificate, receiver, native, receipt, Some(closing))? {
        return Ok(receipt.boundary.time);
    }
    receiver.advance(time);
    let certified = match close_failed_hooks(source, receiver, native, session, receipt)? {
        Some(before) => receipt.clock.time(before.min(certificate.before))?,
        None => time,
    };
    if record_elevated_coverage(session, receipt, receiver, certificate, certified)? {
        source.finish_blind(receipt.boundary, receipt.clock, certificate.opening);
    }
    Ok(certified)
}

fn close_failed_hooks(
    source: &mut Selector<'_>, receiver: &mut Receiver, native: &mut Context<'_>,
    session: &WorkerSession, receipt: Receipt,
) -> Result<Option<QpcTick>, ProbeError> {
    let Some((visit, before)) = receiver.elevated_hook_cutoff() else { return Ok(None); };
    source.cancel_before(visit, before);
    native.discard_elevated_clipboard();
    native.truncate_elevated_input(before);
    if let Some(certificate) = source.input_certificate(Some(before)) {
        receiver.adopt_elevated_certificate(certificate, receipt, native);
    }
    if receiver.take_hook_reports().next().is_some() {
        session.required_elevated_hook_failed(session.stop_requested());
    }
    Ok(Some(before))
}

fn accept_elevated_certificate(
    source: &mut Selector<'_>, certificate: InputCertificate, receiver: &mut Receiver,
    native: &mut Context<'_>, receipt: Receipt, closing: Option<AdmissionBoundary>,
) -> Result<bool, ProbeError> {
    if !receiver.adopt_elevated_certificate(certificate, receipt, native) { return Ok(false); }
    let mut accepted = Ok(());
    source.take_input(certificate, |record| {
        keep_first_error(&mut accepted, || receiver.accept_elevated(record, receipt, closing, native));
    });
    accepted?;
    receiver.finish_elevated_delivery(receipt)?;
    Ok(true)
}

/// The input-stream markers of a Recorder staging overflow (E3): one per
/// overflowed producer range, and the undecodable records.
fn note_staging_loss(receiver: &mut Receiver, loss: &InputStagingLoss, receipt: Receipt) -> Result<(), ProbeError> {
    for gap in loss.gaps.iter().flatten() {
        receiver.note_elevated_loss(LossReason::StagingPressure, Some(*gap), loss.before, receipt)?;
    }
    if let Some(count) = std::num::NonZeroU64::new(loss.malformed) && loss.before >= receipt.boundary.qpc {
        let at = receipt.clock.time(loss.before)?;
        receiver.note_loss(LossEvidence::ordinary(LossReason::MalformedPacket, None, count, at, at));
    } else if loss.gaps.iter().all(Option::is_none) {
        receiver.note_elevated_loss(LossReason::StagingPressure, None, loss.before, receipt)?;
    }
    Ok(())
}
