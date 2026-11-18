use std::{
    num::NonZeroU64,
    sync::{atomic::AtomicBool, mpsc::{self, Receiver, SyncSender, TryRecvError}},
    thread,
    time::{Duration, Instant},
};

use screen_fix_elevated::{Failure, READINESS, Result};
use screen_fix_native_observation::{LossSpan, StaticLease};
use screen_fix_protocol::{
    AdmissionEpoch, AttemptId, GeometryId, QpcFrequency, QpcTick, SessionId,
    ipc::{EnvironmentChange, FocusAtReceipt, InputRegistration, ProducerGap, elevated::{
        ContextEvidence, HeldSnapshot, InputEvidence, Query, ReadBinding, VisitBinding, WindowInstance,
        runtime::{CoverageLoss, CoverageReason, ContextGap, EnvironmentRevision, IdentityClaim, Observation, Revocation, Status},
    }},
};

use crate::{
    config::ResolvedCaptureConfig,
    package::metadata::{SourceBlindInterval, SourceBlindReason},
    platform::windows::controller::{SourcePreparation, WorkerSession},
    session::{AdmissionBoundary, SourceStage, SourceStatus},
    util::keep_earlier,
};

use super::{
    CLEANUP, LaunchStatus, Ticket, WORK_PER_POLL,
    context_queue::{ContextDelivery, ContextQueue},
    ingress::{Ingress, InputCertificate, InputStagingLoss},
    peer::{Peer, Progress},
    visit::{Change, Host, Recovered, add_time, matches_target},
};
use super::super::{clock, context::{ACCESS_RECHECK, FramePolicy, identity::WindowProcess}, display::topology::Topology,
    preflight::{Foreground, ForegroundApproval, classify_expected_foreground}};

static CLASSIFIER_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(in crate::platform::windows) struct Selector<'a> {
    attempt: AttemptId,
    session: SessionId,
    frequency: QpcFrequency,
    frames: &'a FramePolicy,
    session_status: &'a WorkerSession,
    blind: Option<(AdmissionEpoch, QpcTick, SourceBlindReason)>,
    certified_epoch: Option<AdmissionEpoch>,
    classifier: Classifier,
    ticket: Option<Ticket>,
    host: Option<Host>,
    launched: bool,
    withdrawn_dispatch: bool,
    pending: Option<Pending>,
    opened: Option<OpenVisit>,
    ingress: Option<Ingress>,
    queue: ContextQueue,
    interrupted: Option<QpcTick>,
    closed_before: Option<QpcTick>,
    focus_deadline: Option<Instant>,
    loss: Option<CoverageLossReport>,
    recovered: Option<Recovered>,
    environment_sequence: u64,
    geometry_revision: Option<u64>,
    stopping: bool,
    stage: SourceStage,
    helper_check: Option<HelperCheck>,
    /// The suspend count at the last tick that found the attached collector
    /// live (E4). A larger count means the system slept since.
    live_suspends: u64,
    /// The collector was lost across a system sleep. It is not relaunched in
    /// this attempt (A2, A5): elevated foregrounds stay with the standard
    /// source and are suppressed (E4).
    collector_lost: bool,
}

/// The coverage an open visit lost when its input closed for a resync (E3).
#[derive(Clone, Copy)]
pub(in crate::platform::windows) struct CoverageLossReport {
    pub(in crate::platform::windows) binding: VisitBinding,
    /// The first receipt the visit no longer covers.
    pub(in crate::platform::windows) before: QpcTick,
    pub(in crate::platform::windows) cause: CoverageReason,
    pub(in crate::platform::windows) gap: Option<ProducerGap>,
}

/// A standard-helper registration check requested before returning to the
/// standard source (E4).
#[derive(Clone, Copy)]
struct HelperCheck {
    /// The preparation epoch of the switch.
    epoch: AdmissionEpoch,
    /// The suspend sequence at the request; a sleep restarts the check.
    suspends: u64,
    requested: QpcTick,
    until: Instant,
}

struct Pending {
    epoch: AdmissionEpoch,
    approval: ForegroundApproval,
    until: Instant,
}

#[derive(Clone, Copy)]
pub(in crate::platform::windows) struct OpenVisit {
    pub(in crate::platform::windows) binding: VisitBinding,
    pub(in crate::platform::windows) completed: QpcTick,
    pub(in crate::platform::windows) held: Option<HeldSnapshot>,
    pub(in crate::platform::windows) focus: FocusAtReceipt,
    pub(in crate::platform::windows) coverage: CoverageLoss,
    pub(in crate::platform::windows) registration: InputRegistration,
}

impl<'a> Selector<'a> {
    pub(in crate::platform::windows) fn new(
        attempt: AttemptId, session: SessionId, frequency: QpcFrequency,
        config: &ResolvedCaptureConfig, frames: &'a FramePolicy, session_status: &'a WorkerSession,
    ) -> Result<Self> {
        Ok(Self {
            attempt, session, frequency, frames, session_status, blind: None, certified_epoch: None, classifier: Classifier::start(config)?,
            ticket: None, host: None, launched: false, withdrawn_dispatch: false, pending: None,
            opened: None, ingress: None, queue: ContextQueue::default(), interrupted: None,
            closed_before: None, focus_deadline: None, loss: None, recovered: None,
            environment_sequence: 0, geometry_revision: None, stopping: false, stage: SourceStage::Foreground,
            helper_check: None, live_suspends: 0, collector_lost: false,
        })
    }

    pub(in crate::platform::windows) fn tick(
        &mut self, topology: &Topology,
        active: Option<AdmissionBoundary>, changes: u32, ordinary: Option<QpcTick>,
    ) -> Result<()> {
        let session = self.session_status;
        let result = self.service(topology, active, changes, ordinary);
        let Some(host) = self.host.as_ref() else { return result; };
        if host.live() {
            if result.is_ok() { self.mark_live(); }
            return result;
        }
        // E4: a collector lost across a system sleep is G, not a source
        // failure. Its lease, watchdogs and suspend cleanup bound run on QPC,
        // which includes the sleep. It needs a suspend after the host was
        // last live, and no elevated epoch open. There is no relaunch. The
        // suspend count needs no resume mark, which may not be recorded yet
        // when the dead peer is seen (D3).
        let slept = session.suspend_marks().sequence > self.live_suspends;
        if result.is_err() && slept && active.is_none_or(|open| !session.elevated_epoch(open.epoch)) {
            self.host = None;
            self.pending = None;
            self.collector_lost = true;
            self.stage = SourceStage::Foreground;
            session.report_health(crate::package::outcome::HealthKind::ElevatedSourceSleep,
                crate::package::diagnostics::Failure::new(crate::package::diagnostics::FailureComponent::ElevatedSource,
                    "Keep the elevated collector across a system sleep", None), None, false);
            return Ok(());
        }
        result
    }

    fn service(
        &mut self, topology: &Topology,
        active: Option<AdmissionBoundary>, mut changes: u32, ordinary: Option<QpcTick>,
    ) -> Result<()> {
        let session = self.session_status;
        let observed = clock::qpc().map_err(|_| Failure::Supervisor)?;
        if self.stopping || session.stop_requested() {
            // The lease stays renewed until `stop` has sent Control::Stop (E4).
            // A failed renewal surfaces as the actual Stop outcome there.
            let progress = self.progress(observed)?;
            if let Some(host) = self.host.as_mut() { let _ = host.renew_lease(progress); }
            return Ok(());
        }
        self.withdraw_unauthorized(observed)?;
        let foreground = self.classifier.poll()?;
        let classified_failure = match foreground {
            Some(Foreground::Unknown(error)) => error.source_failure(),
            _ => None,
        };
        let elevated = foreground.is_some_and(Foreground::is_elevated);
        // D-8 A: only a failure of the current grant's required access fails
        // the source; any other unclassifiable foreground stays suppressed.
        let grant_target = self.classifier.current.as_ref().map(|(proof, _)| WindowInstance::from(*proof))
            .is_some_and(|proof| self.host.as_ref().and_then(Host::binding).is_some_and(|visit| visit.foreground == proof));
        if (active.is_some() || session.source_preparation().is_some_and(|pending| pending.startup.is_none()))
            && grant_target
            && let Some(failure) = classified_failure
        {
            if session.fail_classified_source(failure) {
                self.frames.clear();
                self.cancel();
            }
            return Ok(());
        }
        if self.geometry_revision.is_some_and(|revision| topology.revision() != revision) {
            changes |= EnvironmentChange::Display.bit();
            self.geometry_revision = None;
        }
        if let Some(open) = active {
            if session.elevated_epoch(open.epoch) {
                let host = self.host.as_mut().ok_or(Failure::Protocol)?;
                if !host.foreground_current() || changes != 0 {
                    self.stage = SourceStage::Cleanup;
                    self.interrupt(observed);
                    session.require_source_change(open.epoch);
                    if changes == 0 {
                        self.host.as_mut().ok_or(Failure::Protocol)?.revoke(observed)?;
                    }
                }
            } else if elevated && !self.collector_lost {
                self.frames.clear();
                session.require_source_change(open.epoch);
            }
        }
        if changes != 0 && self.host.is_some() {
            self.environment(changes, observed, session.cleanup_deadline())?;
        }
        self.poll_launch()?;
        self.poll_host(active)?;
        let preparation = if active.is_none() { session.source_preparation() } else { None };
        if active.is_none() && self.host.as_ref().is_some_and(|host| self.pending.as_ref().is_some_and(|pending| {
            Instant::now() >= pending.until && (!host.configured()
                || preparation.as_ref().is_some_and(|phase| phase.epoch == pending.epoch))
        }))
        {
            return Err(Failure::ReadinessDeadline);
        }
        if self.focus_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(Failure::Witness);
        }
        let progress = self.progress(observed)?;
        if let Some(active) = active {
            if session.elevated_epoch(active.epoch) {
                let host = self.host.as_mut().ok_or(Failure::Protocol)?;
                if !host.awaiting_close() && host.binding().is_some() {
                    if host.activating() {
                        self.stage = SourceStage::Activation;
                        host.activate(active.epoch, active.qpc, progress)?;
                    } else {
                        host.refresh_focus(progress)?;
                        host.retry_held(progress.now)?;
                    }
                }
            }
        } else if let Some(phase) = preparation {
            let candidate = match self.classifier.current.as_ref().map(|(_, classification)| classification) {
                Some(Foreground::Approved(approval)) => Some(approval.clone()),
                _ => None,
            };
            self.prepare(topology, phase, candidate, progress, ordinary)?;
        }
        if let Some(host) = self.host.as_mut() {
            host.maintain(progress)?;
        }
        Ok(())
    }

    pub(in crate::platform::windows) fn close(&mut self, cutoff: QpcTick) -> Result<()> {
        if self.host.as_ref().and_then(Host::binding).is_some() {
            self.stage = SourceStage::Cleanup;
        }
        self.interrupt(cutoff);
        if let Some(host) = self.host.as_mut() {
            host.revoke(cutoff)?;
        }
        Ok(())
    }

    pub(in crate::platform::windows) fn stage(&self) -> SourceStage { self.stage }

    pub(in crate::platform::windows) fn open_visit(&self) -> Option<OpenVisit> { self.opened }

    pub(in crate::platform::windows) fn awaiting_close(&self) -> bool {
        self.host.as_ref().is_some_and(Host::awaiting_close)
    }

    pub(in crate::platform::windows) fn closing_received(&self, cutoff: QpcTick) -> bool {
        self.ingress.as_ref().is_none_or(|ingress| {
            ingress.received_through(self.closed_before.map_or(cutoff, |before| before.min(cutoff)))
        })
    }

    pub(in crate::platform::windows) fn media_open(&self) -> bool {
        self.opened.is_some() && self.ingress.as_ref().is_some_and(Ingress::media_open)
    }

    pub(in crate::platform::windows) fn note_blind(
        &mut self, epoch: AdmissionEpoch, start: QpcTick, reason: SourceBlindReason,
    ) {
        if self.blind.is_none_or(|(old_epoch, old, _)| old_epoch != epoch || start < old) {
            self.blind = Some((epoch, start, reason));
        }
    }

    pub(in crate::platform::windows) fn finish_blind(
        &mut self, boundary: AdmissionBoundary, clock: crate::protocol::ClockOrigin, before: QpcTick,
    ) {
        if let Some((epoch, start, reason)) = self.blind.take()
            && epoch == boundary.epoch
            && let (Ok(start_t_us), Ok(before_t_us)) = (clock.time(start.max(boundary.qpc)), clock.time(before))
        {
            self.session_status.record_source_blind(SourceBlindInterval { epoch, start_t_us, before_t_us, reason });
        }
    }

    pub(in crate::platform::windows) fn note_opening(&mut self, boundary: AdmissionBoundary, visit: OpenVisit) {
        let start = if self.certified_epoch == Some(boundary.epoch) { visit.coverage.first_qpc } else { boundary.qpc };
        self.note_blind(boundary.epoch, start, SourceBlindReason::ElevatedOpening(visit.coverage.reason));
    }

    pub(in crate::platform::windows) fn certified_capture(&mut self, epoch: AdmissionEpoch) {
        self.certified_epoch = Some(epoch);
        self.stage = SourceStage::Capture;
    }

    pub(in crate::platform::windows) fn current_visit(&self) -> bool {
        self.host.as_ref().is_some_and(Host::current)
    }

    pub(in crate::platform::windows) fn certified_before(&self) -> Option<QpcTick> {
        self.input_certificate(None).map(|certificate| certificate.before)
    }

    pub(in crate::platform::windows) fn input_certificate(&self, cutoff: Option<QpcTick>) -> Option<InputCertificate> {
        let cutoff = match (cutoff, self.closed_before) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        // A native notice can sit behind an optional completion in receipt
        // order. An unmaterialized notice reserves its conservative source
        // interval even if a later lane watermark already covers it.
        let cutoff = self.queue.earliest_notice().map_or(cutoff,
            |first| Some(cutoff.map_or(first, |bound| bound.min(first))));
        self.ingress.as_ref()?.certificate(cutoff)
    }

    pub(in crate::platform::windows) fn context_before(&self) -> Option<QpcTick> {
        let before = self.ingress.as_ref()?.context_before()?;
        Some(self.closed_before.map_or(before, |cut| before.min(cut)))
    }

    pub(in crate::platform::windows) fn read_deadline(&self, at: QpcTick, budget: Duration) -> Result<QpcTick> {
        add_time(at, self.frequency, budget)
    }

    pub(in crate::platform::windows) fn query(
        &mut self, binding: ReadBinding, build: impl FnOnce(ReadBinding) -> Query, secondary: Option<IdentityClaim>,
    ) -> Result<Option<ReadBinding>> {
        if self.opened.is_none_or(|opened| opened.binding != binding.visit) || !self.current_visit() {
            return Ok(None);
        }
        let Some(host) = &mut self.host else { return Ok(None); };
        host.optional_read(binding, build, secondary)
    }

    pub(in crate::platform::windows) fn deferred_read(&self, expected: ReadBinding) -> bool {
        self.queue.deferred_read(expected)
    }

    /// The staged context deliveries ready at `at` (`ContextQueue::ready`).
    pub(in crate::platform::windows) fn observations(&mut self, at: QpcTick) -> Vec<(QpcTick, ContextDelivery)> {
        let before = self.context_before();
        self.queue.ready(before, at)
    }

    pub(in crate::platform::windows) fn take_input(&mut self, certificate: InputCertificate, accept: impl FnMut(InputEvidence)) {
        if let Some(ingress) = &mut self.ingress {
            ingress.take(certificate, accept);
        }
    }

    pub(in crate::platform::windows) fn take_interruption(&mut self) -> Option<QpcTick> { self.interrupted.take() }

    pub(in crate::platform::windows) fn take_loss(&mut self) -> Option<CoverageLossReport> {
        self.loss.take()
    }

    pub(in crate::platform::windows) fn take_staging_loss(&mut self) -> Option<InputStagingLoss> {
        self.ingress.as_mut().and_then(Ingress::take_pressure)
    }

    /// Native gaps of the previous binding delivered before the resync sample (E3).
    pub(in crate::platform::windows) fn take_prior_gaps(&mut self) -> Vec<ProducerGap> {
        self.ingress.as_mut().map(|ingress| ingress.take_prior().collect()).unwrap_or_default()
    }

    pub(in crate::platform::windows) fn take_context_pressure(&mut self) -> Option<ContextGap> {
        self.queue.take_pressure()
    }

    pub(in crate::platform::windows) fn take_optional_loss(&mut self) -> Option<LossSpan> {
        self.queue.take_optional_loss()
    }

    pub(in crate::platform::windows) fn take_recovered(&mut self) -> Option<Recovered> {
        self.recovered.take()
    }

    pub(in crate::platform::windows) fn observe_focus_loss(&mut self, before: QpcTick) {
        self.interrupt(before);
        self.focus_deadline.get_or_insert(Instant::now() + READINESS);
    }

    pub(in crate::platform::windows) fn switching(&self) -> bool {
        self.pending.is_some() || self.ticket.is_some()
    }

    /// E1: a pending or unknown classification is not inactivity. The
    /// standard source stays selected until the selector decides that the
    /// collector is required, which also latches the source change; approval
    /// is decided per receipt by the gate.
    pub(in crate::platform::windows) fn ordinary_allowed(&self) -> bool {
        !self.stopping && !self.source_latched()
    }

    /// The selector decided that the collector is required, which latches the
    /// source change (E1). Stopping is not a latch: its standard records stay
    /// inventory-only.
    pub(in crate::platform::windows) fn source_latched(&self) -> bool {
        !self.stopping && !self.collector_lost && self.classifier.current.as_ref().is_some_and(|(_, value)| value.is_elevated())
    }

    /// Records of an interrupted visit past its certified prefix (E1). They
    /// never reach the Receiver; the caller counts them.
    pub(in crate::platform::windows) fn take_uncertified(&mut self, visit: VisitBinding, lost: impl FnMut(InputEvidence)) {
        if let Some(ingress) = self.ingress.as_mut().filter(|ingress| ingress.visit() == visit) {
            ingress.drain().for_each(lost);
        }
    }

    pub(in crate::platform::windows) fn cancel_before(&mut self, visit: VisitBinding, cutoff: QpcTick) {
        restrict_visit(self.frames, self.session_status, visit, cutoff);
        if self.ingress.as_ref().is_some_and(|ingress| ingress.visit() == visit) {
            self.interrupt(cutoff);
        }
        self.cancel();
    }

    pub(in crate::platform::windows) fn cancel(&mut self) {
        if let Some(ingress) = &self.ingress {
            let before = self.input_certificate(None).map_or_else(|| ingress.opening(), |certificate| certificate.before);
            self.interrupt(before);
        }
        self.stopping = true;
        self.ticket.take();
        // The host stays attached: `stop` still sends Control::Stop and waits
        // for the terminal acknowledgement within the cleanup bound (E4).
        self.opened = None;
        self.retire_queue();
        self.focus_deadline = None;
        self.recovered = None;
    }

    pub(in crate::platform::windows) fn stop(&mut self, cutoff: QpcTick, limit: Option<Instant>) -> Result<()> {
        let session = self.session_status;
        self.stopping = true;
        self.ticket.take();
        self.interrupt(cutoff);
        let Some(host) = self.host.take() else { return Ok(()); };
        let active = host.binding();
        let mut closed = active.is_none();
        let deadline = limit.map_or_else(|| Instant::now() + CLEANUP, |stop| (Instant::now() + CLEANUP).min(stop));
        let now = clock::qpc().map_err(|_| Failure::Supervisor)?;
        let cleanup_deadline_qpc = add_time(now, self.frequency, deadline.saturating_duration_since(Instant::now()))?;
        host.send_stop(cutoff.min(now), cleanup_deadline_qpc)?;
        let frames = self.frames;
        let restrict = |visit, before| restrict_visit(frames, session, visit, before);
        let mut stopped = false;
        loop {
            for _ in 0..WORK_PER_POLL {
                if stopped { break; }
                let status = host.next_status()?;
                if let Some(status) = &status {
                    let now = clock::qpc().map_err(|_| Failure::Supervisor)?;
                    if matches!(status, Status::Invalidated { before_qpc, .. } | Status::Closed { before_qpc, .. }
                        if *before_qpc > now)
                    {
                        return Err(Failure::Protocol);
                    }
                    apply_status_restriction(status, now, self.ingress.as_ref(), &mut self.closed_before, restrict);
                }
                match status {
                    Some(Status::Stopped) => {
                        if !closed { return Err(Failure::Protocol); }
                        session.update_elevated_evidence(|evidence| evidence.stopped = true);
                        stopped = true;
                    }
                    Some(Status::Closed { visit, before_qpc })
                        if active.is_some_and(|active| same_grant(active, visit)) =>
                    {
                        keep_earlier(&mut self.closed_before, before_qpc);
                        closed = true;
                    }
                    Some(Status::Invalidated { visit, before_qpc, reason })
                        if active.is_some_and(|active| same_grant(active, visit)) =>
                    {
                        keep_earlier(&mut self.closed_before, before_qpc);
                        if let Some(failure) = revocation_failure(reason) {
                            session.source_finalization_failed(failure);
                        }
                    }
                    Some(Status::Configured) => session.update_elevated_evidence(|evidence| evidence.configured = true),
                    Some(Status::Failed(failure)) => return Err(Failure::NativeProducer(failure)),
                    Some(_) => {}
                    None => break,
                }
            }
            if Instant::now() >= deadline {
                // A terminal control record confirms host cleanup. The
                // received certificates still bound the independently read data.
                return if stopped { Ok(()) } else { Err(Failure::CleanupDeadline) };
            }
            let peer_exited = drain_closed_lanes(&host, &mut self.ingress, &mut self.closed_before,
                self.frames, self.session_status, stopped)?;
            if stopped && (peer_exited || self.closing_received(cutoff)) { return Ok(()); }
            thread::park_timeout(Duration::from_millis(2));
        }
    }

    fn prepare(
        &mut self, topology: &Topology, phase: SourcePreparation,
        candidate: Option<ForegroundApproval>, progress: Progress, ordinary: Option<QpcTick>,
    ) -> Result<()> {
        let session = self.session_status;
        if self.ticket.is_some() { return Ok(()); }
        let ready = self.host.as_mut().map(|host| host.armed(progress.now)).transpose()?.flatten();
        if let (Some((registration, focus)), Some(pending)) = (ready, &self.pending) {
            if !required_registration(registration, session.config().profile().pen_hid()) {
                return Err(Failure::RequiredRegistration);
            }
            session.source_ready(pending.epoch, pending.approval.clone(), Some(focus));
            return Ok(());
        }
        if self.host.as_ref().and_then(Host::binding).is_some() { return Ok(()); }
        if let Some(pending) = &mut self.pending && pending.epoch != phase.epoch {
            match self.host.as_ref() {
                Some(host) if host.configured() && phase.startup.is_some() => {
                    // Startup still needs the foreground identified before consent.
                    pending.epoch = phase.epoch;
                    pending.until = Instant::now() + READINESS;
                }
                // The consent/configuration phase is over. A new live
                // preparation independently selects the current foreground.
                Some(host) if host.configured() => self.pending = None,
                Some(_) => return Ok(()),
                None => self.pending = None,
            }
        }
        if self.pending.is_none() {
            let Some(approval) = phase.startup.or(candidate) else { return Ok(()); };
            // A lost collector is not relaunched (E4); this candidate waits.
            if approval.elevated().is_some() && self.collector_lost { return Ok(()); }
            if approval.elevated().is_none() {
                let Some(checked) = ordinary else {
                    self.stage = SourceStage::Readiness;
                    return Err(Failure::StandardHelperUnavailable);
                };
                // Returning from the collector needs a registration check of
                // the standard helper newer than this switch request (E4).
                if self.host.is_some() {
                    // The request belongs to this switch; a sleep restarts it.
                    let suspends = session.suspend_marks().sequence;
                    let current = self.helper_check.filter(|check| check.epoch == phase.epoch && check.suspends == suspends);
                    let check = match current {
                        Some(check) => check,
                        None => {
                            session.request_input_check();
                            *self.helper_check.insert(HelperCheck {
                                epoch: phase.epoch, suspends, requested: progress.now, until: Instant::now() + READINESS,
                            })
                        }
                    };
                    if checked <= check.requested {
                        if Instant::now() < check.until { return Ok(()); }
                        self.stage = SourceStage::Readiness;
                        return Err(Failure::StandardHelperUnavailable);
                    }
                }
                self.helper_check = None;
                session.source_ready(phase.epoch, approval, None);
                return Ok(());
            }
            self.pending = Some(Pending { epoch: phase.epoch, approval, until: Instant::now() + READINESS });
        }
        self.stage = SourceStage::Readiness;
        session.source_pending(phase.epoch, SourceStatus::AwaitingReadiness);
        let pending = self.pending.as_ref().ok_or(Failure::Protocol)?;
        let claim = pending.approval.elevated().ok_or(Failure::Protocol)?;
        if Instant::now() >= pending.until {
            return Err(Failure::ReadinessDeadline);
        }
        if !matches_target(claim)? { return Ok(()); }
        let authorization = session.source_dispatch(phase.epoch, claim);
        if let Some(host) = self.host.as_mut() {
            let revision = topology.revision();
            if let Some(geometry) = geometry(topology, session, pending.approval.window())
                && topology.revision() == revision
            {
                match host.prepare(&authorization, geometry, progress, pending.until) {
                    Ok(true) => self.geometry_revision = Some(revision),
                    Ok(false) => {}
                    Err(Failure::Cancelled) if !session.source_authorized(phase.epoch) => {}
                    Err(failure) => return Err(failure),
                }
            }
        } else if !self.launched {
            if session.stop_requested() || session.source_preparation().is_none_or(|current| current.epoch != phase.epoch) {
                return Ok(());
            }
            self.stage = SourceStage::Consent;
            session.update_elevated_evidence(|evidence| evidence.requested = true);
            let ticket = match Ticket::start(self.attempt.clone(), self.session.clone(), authorization) {
                Ok(ticket) => ticket,
                Err(Failure::Busy) if self.withdrawn_dispatch => {
                    self.stage = SourceStage::Readiness;
                    return Ok(());
                }
                Err(failure) => return Err(failure),
            };
            session.source_pending(phase.epoch, SourceStatus::AwaitingConsent);
            self.ticket = Some(ticket);
            self.launched = true;
            self.withdrawn_dispatch = false;
        } else {
            return Err(Failure::Transport);
        }
        Ok(())
    }

    fn poll_launch(&mut self) -> Result<()> {
        let session = self.session_status;
        let Some(ticket) = self.ticket.as_mut() else { return Ok(()); };
        let epoch = self.pending.as_ref().ok_or(Failure::Protocol)?.epoch;
        match ticket.poll() {
            LaunchStatus::AwaitingConsent => {}
            LaunchStatus::Authenticating => {
                self.stage = SourceStage::Readiness;
                session.source_pending(epoch, SourceStatus::AwaitingReadiness);
            }
            LaunchStatus::Complete { authenticating, result } => {
                if authenticating {
                    self.stage = SourceStage::Readiness;
                }
                let dispatched = ticket.was_dispatched();
                self.ticket.take();
                // The shell call returned when binding began or consent failed (E4).
                let returned = authenticating || matches!(&result, Err(Failure::ConsentDenied | Failure::ConsentLaunch));
                session.update_elevated_evidence(|evidence| {
                    evidence.consent_dispatched |= dispatched;
                    evidence.consent_returned |= returned;
                });
                if matches!(&result, Err(Failure::Cancelled)) && !dispatched
                    && self.pending.as_ref().is_some_and(|pending| !session.source_authorized(pending.epoch))
                {
                    self.withdraw_launch();
                    return Ok(());
                }
                let (authenticated, until) = result?;
                session.update_elevated_evidence(|evidence| evidence.authenticated = true);
                if session.stop_requested() { return Ok(()); }
                let pending = self.pending.as_mut().ok_or(Failure::Protocol)?;
                pending.until = until;
                self.host = Some(Host::new(Peer::start(authenticated, &self.attempt, &self.session, self.frequency,
                    session.config().profile(), until)?, self.frequency)?);
                self.mark_live();
                session.source_pending(epoch, SourceStatus::AwaitingReadiness);
            }
        }
        Ok(())
    }

    fn withdraw_unauthorized(&mut self, observed: QpcTick) -> Result<()> {
        let session = self.session_status;
        if self.host.as_ref().and_then(Host::binding).is_some_and(|visit| !session.source_authorized(visit.epoch)) {
            let before = session.paused_cutoff().map_or(observed, |closed| closed.qpc.min(observed));
            self.close(before)?;
        }
        if self.pending.as_ref().is_some_and(|pending| !session.source_authorized(pending.epoch))
            && self.ticket.as_ref().is_some_and(Ticket::withdraw_undispatched)
        {
            self.ticket.take();
            self.withdraw_launch();
        }
        Ok(())
    }

    /// Drop a launch that was withdrawn before its consent dispatch.
    fn withdraw_launch(&mut self) {
        self.pending = None;
        self.launched = false;
        self.withdrawn_dispatch = true;
        self.session_status.update_elevated_evidence(|evidence| evidence.withdrawn_launches = evidence.withdrawn_launches.saturating_add(1));
    }

    fn poll_host(&mut self, active: Option<AdmissionBoundary>) -> Result<()> {
        if self.host.is_none() { return Ok(()); }
        self.poll_status(active)?;
        self.poll_observations()?;
        self.poll_input(active)?;
        self.revoke_on_pressure()
    }

    /// Apply up to `WORK_PER_POLL` collector statuses.
    fn poll_status(&mut self, active: Option<AdmissionBoundary>) -> Result<()> {
        let (frames, session) = (self.frames, self.session_status);
        let restrict = |visit, before| restrict_visit(frames, session, visit, before);
        for _ in 0..WORK_PER_POLL {
            let Some(host) = self.host.as_mut() else { break; };
            let Some(status) = host.next_status()? else { break; };
            let now = clock::qpc().map_err(|_| Failure::Supervisor)?;
            apply_status_restriction(&status, now, self.ingress.as_ref(), &mut self.closed_before, restrict);
            let change = host.status(status, now)?;
            self.apply_change(change, active, now)?;
        }
        Ok(())
    }

    /// Apply the grant change of one collector status, applied at `now`.
    fn apply_change(&mut self, change: Change, active: Option<AdmissionBoundary>, now: QpcTick) -> Result<()> {
        let session = self.session_status;
        match change {
            Change::None => {}
            Change::Configured => session.update_elevated_evidence(|evidence| evidence.configured = true),
            Change::Registered { binding, at, registration, held } => {
                session.elevated_registration(binding.epoch, at, registration, held);
            }
            Change::ReadRejected { read, reason } => {
                if self.opened.is_some_and(|opened| opened.binding == read.visit) {
                    let frames = self.frames;
                    self.queue.push(now, ContextDelivery::Rejected(read, reason),
                        |visit, before| restrict_visit(frames, session, visit, before));
                }
            }
            Change::InputOpened { binding, focus, completed, snapshot, coverage, registration } => {
                self.queue.clear();
                self.opened = Some(OpenVisit { binding, completed, held: snapshot, focus, coverage, registration });
                self.ingress = Some(Ingress::new(binding, focus, completed, coverage.before_qpc));
                self.closed_before = None;
                self.focus_deadline = None;
                self.pending = None;
            }
            Change::InputClosed { binding, before, loss } => {
                self.retire_queue();
                self.opened = None;
                keep_earlier(&mut self.interrupted, before);
                keep_earlier(&mut self.closed_before, before);
                if let Some((cause, gap)) = loss {
                    self.loss = Some(CoverageLossReport { binding, before, cause, gap });
                }
            }
            Change::Invalidated { binding, before, reason } => {
                self.retire_queue();
                self.opened = None;
                keep_earlier(&mut self.interrupted, before);
                keep_earlier(&mut self.closed_before, before);
                self.focus_deadline = None;
                match reason {
                    Revocation::Requested | Revocation::Environment | Revocation::Stopping => {}
                    // D-8 A: a target change is a transition. Before admission the
                    // candidate is prepared again within its readiness budget.
                    Revocation::Foreground | Revocation::Focus | Revocation::LateNotice
                        if session.started() => session.require_source_change(binding.epoch),
                    Revocation::Foreground | Revocation::Focus | Revocation::LateNotice => {}
                    reason => return Err(revocation_failure(reason).ok_or(Failure::Protocol)?),
                }
            }
            Change::Closed { binding, before } => {
                keep_earlier(&mut self.interrupted, before);
                keep_earlier(&mut self.closed_before, before);
                self.focus_deadline = None;
                self.geometry_revision = None;
                // Before admission the startup candidate stays pending (D-8 A).
                if active.is_none() && session.started()
                    && self.pending.as_ref().is_some_and(|pending| pending.epoch == binding.epoch)
                {
                    session.abandon_source_preparation(binding.epoch);
                    self.pending = None;
                }
            }
            // D-8 A: a target change during preparation is a transition.
            // Before admission the candidate is prepared again within its
            // readiness budget. After admission the preparation is dropped,
            // the epoch stays closed and the foreground is classified fresh.
            Change::PrepareRejected { binding } => {
                if session.started() {
                    if active.is_some() || self.pending.as_ref().is_none_or(|pending| pending.epoch != binding.epoch) {
                        return Err(Failure::Protocol);
                    }
                    session.abandon_source_preparation(binding.epoch);
                    self.pending = None;
                }
            }
            Change::Stopped => return Err(Failure::PeerExited),
        }
        Ok(())
    }

    /// Hand up to `WORK_PER_POLL` collector observations to the ingress and
    /// stage the open visit's context deliveries.
    fn poll_observations(&mut self) -> Result<()> {
        let Some(host) = self.host.as_mut() else { return Ok(()); };
        let (frames, session) = (self.frames, self.session_status);
        let restrict = |visit, before| restrict_visit(frames, session, visit, before);
        for _ in 0..WORK_PER_POLL {
            let Some((arrived, observation)) = host.next_observation()? else { break; };
            if let Some(recovered) = host.observation(&observation, arrived)?
                && self.ingress.as_ref().is_some_and(|ingress| ingress.can_synchronize(recovered.binding, recovered.completed))
            {
                host.recovered();
                self.recovered = Some(recovered);
            }
            let same_visit = if let Some(ingress) = &mut self.ingress {
                ingress.observation(&observation, arrived, restrict)?
            } else { false };
            if same_visit && self.opened.is_none() {
                cut_at_notice(&observation, &mut self.closed_before, restrict);
            }
            if same_visit && self.opened.is_some()
                && !matches!(observation, Observation::Progress { .. } | Observation::Item { evidence: ContextEvidence::Held(_), .. })
            {
                self.queue.push(arrived, ContextDelivery::Observation(observation), restrict);
            }
        }
        Ok(())
    }

    /// Hand the collector's input to the ingress while a visit is open, or
    /// while the closing visit's input still drains before admission.
    fn poll_input(&mut self, active: Option<AdmissionBoundary>) -> Result<()> {
        let Some(host) = self.host.as_ref() else { return Ok(()); };
        if self.opened.is_some() || (active.is_none() && self.closed_before.is_some()
            && (host.awaiting_close() || host.binding().is_none()))
        {
            let (frames, session) = (self.frames, self.session_status);
            drain_input(host, &mut self.ingress, |visit, before| restrict_visit(frames, session, visit, before), false)?;
        }
        Ok(())
    }

    /// Staging pressure ends the open visit before its first unstaged
    /// record: the visit is restricted, interrupted and revoked.
    fn revoke_on_pressure(&mut self) -> Result<()> {
        let Some(host) = self.host.as_mut() else { return Ok(()); };
        let (frames, session) = (self.frames, self.session_status);
        let restrict = |visit, before| restrict_visit(frames, session, visit, before);
        let pressure = self.ingress.as_ref().and_then(Ingress::pressure_before);
        if let Some(before) = pressure { keep_earlier(&mut self.closed_before, before); }
        let local_cutoff = pressure.into_iter().chain(self.queue.pressure_first()).min();
        if let Some(before) = local_cutoff
            && let Some(opened) = self.opened.take()
        {
            restrict(opened.binding, before);
            keep_earlier(&mut self.interrupted, before);
            keep_earlier(&mut self.closed_before, before);
            self.queue.retire(&mut self.closed_before, restrict);
            session.require_source_change(opened.binding.epoch);
            host.revoke(before)?;
        }
        Ok(())
    }

    /// Drop the staged context deliveries (`ContextQueue::retire`).
    fn retire_queue(&mut self) {
        let (frames, session) = (self.frames, self.session_status);
        self.queue.retire(&mut self.closed_before, |visit, before| restrict_visit(frames, session, visit, before));
    }

    fn environment(&mut self, bits: u32, at: QpcTick, limit: Option<Instant>) -> Result<()> {
        let Some(host) = self.host.as_mut() else { return Ok(()); };
        let cleanup_deadline_qpc = limit.map(|limit| add_time(at, self.frequency, limit.saturating_duration_since(Instant::now())))
            .transpose()?;
        for change in [EnvironmentChange::SessionEnding, EnvironmentChange::Session, EnvironmentChange::Display, EnvironmentChange::Settings] {
            if bits & change.bit() == 0 { continue; }
            let sequence = self.environment_sequence.checked_add(1).and_then(NonZeroU64::new).ok_or(Failure::Protocol)?;
            host.environment(EnvironmentRevision { sequence, changed_qpc: at, change, cleanup_deadline_qpc })?;
            self.environment_sequence = sequence.get();
            if change == EnvironmentChange::SessionEnding { break; }
        }
        Ok(())
    }

    /// Record the suspends the live collector has outlived. A suspend notice
    /// without its resume may still precede the sleep itself, so the collector
    /// can yet die across it (D3).
    fn mark_live(&mut self) {
        let marks = self.session_status.suspend_marks();
        let outstanding = marks.sequence != 0 && marks.resume.is_none();
        self.live_suspends = marks.sequence - u64::from(outstanding);
    }

    fn progress(&self, pump: QpcTick) -> Result<Progress> {
        let controller = self.session_status.controller_qpc();
        let now = clock::qpc().map_err(|_| Failure::Supervisor)?;
        Ok(Progress { now, controller, pump })
    }

    fn interrupt(&mut self, before: QpcTick) {
        self.retire_queue();
        if self.opened.take().is_some() {
            keep_earlier(&mut self.interrupted, before);
        }
        if let Some(ingress) = &self.ingress {
            keep_earlier(&mut self.closed_before, before);
            restrict_visit(self.frames, self.session_status, ingress.visit(), before);
        }
    }
}

fn restrict_visit(frames: &FramePolicy, session: &WorkerSession, visit: VisitBinding, before: QpcTick) {
    frames.restrict_elevated(visit, before);
    session.restrict_source_coverage(visit, before);
}

fn drain_closed_lanes(
    host: &Host, ingress: &mut Option<Ingress>, cutoff: &mut Option<QpcTick>, frames: &FramePolicy,
    session: &WorkerSession, stopped: bool,
) -> Result<bool> {
    let restrict = |visit, before| restrict_visit(frames, session, visit, before);
    let mut context_exited = false;
    for _ in 0..WORK_PER_POLL {
        let (arrived, record) = match host.next_observation() {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(Failure::PeerExited) if stopped => { context_exited = true; break; }
            Err(failure) => return Err(failure),
        };
        let Some(ingress) = ingress.as_mut() else { continue; };
        if ingress.observation(&record, arrived, restrict)? {
            cut_at_notice(&record, cutoff, restrict);
        }
    }
    let input_exited = drain_input(host, ingress, restrict, stopped)?;
    if let Some(before) = ingress.as_ref().and_then(Ingress::pressure_before) {
        keep_earlier(cutoff, before);
    }
    Ok(context_exited && input_exited)
}

/// Hand up to `WORK_PER_POLL` input records to the `ingress`; without one a
/// record is dropped. Returns whether the input lane exited, which ends the
/// drain only once the collector `stopped`.
fn drain_input(
    host: &Host, ingress: &mut Option<Ingress>, restrict: impl Fn(VisitBinding, QpcTick) + Copy, stopped: bool,
) -> Result<bool> {
    for _ in 0..WORK_PER_POLL {
        let record = match host.next_input() {
            Ok(Some(record)) => record,
            Ok(None) => break,
            Err(Failure::PeerExited) if stopped => return Ok(true),
            Err(failure) => return Err(failure),
        };
        let Some(ingress) = ingress.as_mut() else { continue; };
        let now = clock::qpc().map_err(|_| Failure::Supervisor)?;
        match record {
            Some(record) => ingress.input(record, now, restrict)?,
            None => ingress.malformed(restrict),
        }
    }
    Ok(false)
}

/// A native notice of a visit whose input closed ends its coverage, and the
/// closing `cutoff`, before the notice's first possible receipt.
fn cut_at_notice(observation: &Observation, cutoff: &mut Option<QpcTick>, restrict: impl Fn(VisitBinding, QpcTick)) {
    if let Observation::Item { evidence: ContextEvidence::Notice(notice), .. } = observation {
        keep_earlier(cutoff, notice.first_qpc);
        restrict(notice.visit, notice.first_qpc);
    }
}

/// Restrict the visit that `status` closed, invalidated, suspended or sent
/// to a resync, before the stamp it names; the closing `cutoff` of the
/// staging `ingress` visit falls to that stamp too.
fn apply_status_restriction(
    status: &Status, now: QpcTick, ingress: Option<&Ingress>, cutoff: &mut Option<QpcTick>,
    restrict: impl Fn(VisitBinding, QpcTick),
) {
    if let Some((visit, before)) = status_restriction(status, now) {
        restrict(visit, before);
        if ingress.is_some_and(|ingress| ingress.visit() == visit) {
            keep_earlier(cutoff, before);
        }
    }
}

fn revocation_failure(reason: Revocation) -> Option<Failure> {
    match reason {
        Revocation::RequiredAccess => Some(Failure::TargetAccess),
        Revocation::NativeFailure => Some(Failure::NativeOwnerLost),
        Revocation::WitnessLost => Some(Failure::Witness),
        Revocation::Expired => Some(Failure::LeaseExpired),
        _ => None,
    }
}

fn status_restriction(status: &Status, now: QpcTick) -> Option<(VisitBinding, QpcTick)> {
    let (visit, before) = match status {
        Status::Invalidated { visit, before_qpc, .. } | Status::Closed { visit, before_qpc }
        | Status::FocusSuspended { visit, before_qpc } => (*visit, *before_qpc),
        Status::ResyncRequired { previous, first_uncovered, resync_after_qpc, .. } if *resync_after_qpc <= now =>
            (*previous, *first_uncovered),
        _ => return None,
    };
    (before <= now).then_some((visit, before))
}

fn same_grant(left: VisitBinding, right: VisitBinding) -> bool {
    left.epoch == right.epoch && left.source_generation == right.source_generation && left.grant == right.grant
        && left.foreground == right.foreground && left.witness_revision == right.witness_revision
}

fn required_registration(registration: InputRegistration, pen_hid: bool) -> bool {
    registration.required_ready() && (!pen_hid || registration.pen_hid)
}

fn geometry(topology: &Topology, session: &WorkerSession, foreground: WindowProcess) -> Option<GeometryId> {
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
    let monitor = unsafe { MonitorFromWindow(foreground.hwnd().0, MONITOR_DEFAULTTONEAREST) };
    let snapshot = topology.snapshot()?;
    let target = snapshot.outputs.as_ref()?.iter().find(|output| output.identity == monitor as usize);
    let selected = session.config().monitors();
    target.and_then(|target| selected.iter().find(|monitor| monitor.key == target.key))
        .and_then(|monitor| topology.geometry(monitor.id).ok().flatten())
        .or_else(|| selected.iter().find_map(|monitor| topology.geometry(monitor.id).ok().flatten()))
}

struct Classifier {
    request: SyncSender<WindowProcess>,
    completed: Receiver<(WindowProcess, Foreground)>,
    current: Option<(WindowProcess, Foreground)>,
    pending: Option<WindowProcess>,
    checked_at: Option<Instant>,
}

impl Classifier {
    fn start(config: &ResolvedCaptureConfig) -> Result<Self> {
        let owner = StaticLease::claim(&CLASSIFIER_ACTIVE).ok_or(Failure::Busy)?;
        let config = config.clone();
        let (request, incoming) = mpsc::sync_channel(1);
        let (done, completed) = mpsc::sync_channel(1);
        thread::Builder::new().name("recording-foreground-classification".into()).spawn(move || {
            let _owner = owner;
            for proof in incoming {
                let value = classify_expected_foreground(&config, Some(proof));
                if done.try_send((proof, value)).is_err() { break; }
            }
        }).map_err(|_| Failure::Supervisor)?;
        Ok(Self { request, completed, current: None, pending: None, checked_at: None })
    }

    fn poll(&mut self) -> Result<Option<&Foreground>> {
        let foreground = WindowProcess::foreground();
        if self.current.as_ref().is_some_and(|(proof, _)| Some(*proof) != foreground) {
            self.current = None;
        }
        match self.completed.try_recv() {
            Ok((proof, value)) => {
                self.pending = None;
                let consistent = match &value {
                    Foreground::Approved(approval) => approval.window() == proof,
                    _ => true,
                };
                if consistent && Some(proof) == foreground {
                    self.current = Some((proof, value));
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Err(Failure::Supervisor),
        }
        if let Some(proof) = foreground && self.pending.is_none()
            && (self.current.is_none() || self.checked_at.is_none_or(|when| when.elapsed() >= ACCESS_RECHECK))
            && self.request.try_send(proof).is_ok()
        {
            self.pending = Some(proof);
            self.checked_at = Some(Instant::now());
        }
        Ok(self.current.as_ref().map(|(_, value)| value))
    }
}
