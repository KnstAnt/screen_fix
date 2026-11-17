use std::{collections::VecDeque, num::NonZeroU64, time::{Duration, Instant}};

use screen_fix_elevated::{Failure, READINESS, Result, foreground_access, ForegroundAccess};
use screen_fix_protocol::{AdmissionEpoch, FocusGeneration, GeometryId, QpcFrequency, QpcTick, RequestId, ipc::{
    FocusAtReceipt, InputRegistration, ProducerGap,
    elevated::{ContextEvidence, HeldSnapshot, Query, ReadBinding, ReadOutcome, ReadUnavailable, VisitBinding,
        runtime::{Activation, ContextCommand, Control, CoverageLoss, CoverageReason, GrantRequest, HeldReadiness,
            IdentityClaim, Observation, Revocation, Status}},
}};

use super::peer::{LeaseState, Peer, Progress};
use super::super::context::identity::WindowProcess;
use crate::session::windows::SourceDispatch;

const HELD_BUDGET: Duration = Duration::from_millis(250);
const HELD_RETRY: Duration = Duration::from_millis(100);
const OPTIONAL_READ_CAPACITY: usize = 32;
const REJECT_RETENTION: Duration = Duration::from_secs(1);

pub(super) struct Host {
    pub(super) peer: Peer,
    lease: LeaseState,
    frequency: QpcFrequency,
    configured: bool,
    witness: Option<Witness>,
    sequence: u64,
    request: u64,
    focus_generation: u64,
    grant: Option<Grant>,
    last_closed: Option<VisitBinding>,
    retry: Option<ReadBinding>,
    retry_after: Option<QpcTick>,
    optional_reads: VecDeque<ReadBinding>,
}

#[derive(Clone, Copy)]
struct Witness {
    revision: u64,
    focus_revision: u64,
    safe: bool,
    observed: QpcTick,
}

struct Grant {
    claim: IdentityClaim,
    binding: VisitBinding,
    requested: QpcTick,
    deadline: Instant,
    phase: Phase,
    lower: QpcTick,
    focus: Option<FocusAtReceipt>,
    registration: Option<InputRegistration>,
    closing_before: Option<QpcTick>,
}

#[derive(Clone, Copy)]
enum Phase {
    Preparing,
    Armed(InputRegistration),
    Activating,
    HeldPending(ReadBinding),
    Accepting { completed: QpcTick, snapshot: Option<HeldSnapshot> },
    Open,
    FocusSuspended,
    Refreshing { next: VisitBinding },
    Closing,
}

pub(super) enum Change {
    Configured,
    Registered { binding: VisitBinding, at: QpcTick, registration: InputRegistration },
    None,
    ReadRejected { read: ReadBinding, reason: ReadUnavailable },
    InputOpened { binding: VisitBinding, focus: FocusAtReceipt, completed: QpcTick,
        snapshot: Option<HeldSnapshot>, coverage: CoverageLoss, registration: InputRegistration },
    InputClosed { binding: VisitBinding, before: QpcTick, loss: Option<(CoverageReason, Option<ProducerGap>)> },
    Invalidated { binding: VisitBinding, before: QpcTick, reason: Revocation },
    Closed { binding: VisitBinding, before: QpcTick },
    Stopped,
}

impl Host {
    pub(super) fn new(peer: Peer, frequency: QpcFrequency) -> Result<Self> {
        Ok(Self {
            peer, lease: LeaseState::new(frequency)?, frequency, configured: false, witness: None,
            sequence: 0, request: 0, focus_generation: 0, grant: None, last_closed: None, retry: None, retry_after: None, optional_reads: VecDeque::new(),
        })
    }

    pub(super) fn binding(&self) -> Option<VisitBinding> {
        self.grant.as_ref().map(|grant| grant.binding)
    }

    pub(super) fn configured(&self) -> bool { self.configured }

    pub(super) fn foreground_current(&self) -> bool {
        self.grant.as_ref().is_some_and(|grant| foreground_is(&grant.claim))
    }

    pub(super) fn activating(&self) -> bool {
        self.grant.as_ref().is_some_and(|grant| matches!(grant.phase, Phase::Armed(_)))
    }

    pub(super) fn awaiting_close(&self) -> bool {
        self.grant.as_ref().is_some_and(|grant| matches!(grant.phase, Phase::Closing))
    }

    pub(super) fn environment(&mut self, revision: screen_fix_protocol::ipc::elevated::runtime::EnvironmentRevision) -> Result<()> {
        // Configuration is ordered ahead of every other control record.
        self.peer.send(Control::Environment(revision))?;
        self.mark_closing(revision.changed_qpc);
        Ok(())
    }

    fn mark_closing(&mut self, before: QpcTick) {
        self.retry = None;
        self.retry_after = None;
        if let Some(grant) = self.grant.as_mut() {
            grant.closing_before = Some(grant.closing_before.map_or(before, |previous| previous.min(before)));
            if matches!(grant.phase, Phase::Closing) { return; }
            grant.phase = Phase::Closing;
            grant.deadline = Instant::now() + Duration::from_millis(750);
        }
    }

    pub(super) fn lease_epoch(&self) -> Option<AdmissionEpoch> {
        self.grant.as_ref().filter(|grant| !matches!(grant.phase, Phase::Preparing | Phase::Armed(_) | Phase::Closing))
            .map(|grant| grant.binding.epoch)
    }

    pub(super) fn armed(&self) -> Result<Option<(InputRegistration, FocusAtReceipt)>> {
        let Some(grant) = self.grant.as_ref() else { return Ok(None); };
        match grant.phase {
            Phase::Armed(registration) => {
                let focus = grant.focus.ok_or(Failure::Protocol)?;
                Ok((grant.current(self.witness)? && focus_current(grant.binding, focus)).then_some((registration, focus)))
            }
            _ => Ok(None),
        }
    }

    pub(super) fn prepare(
        &mut self, authorization: &SourceDispatch, geometry: GeometryId, progress: Progress, deadline: Instant,
    ) -> Result<bool> {
        let claim = authorization.target();
        let epoch = authorization.epoch();
        if !self.configured || self.grant.is_some() { return Ok(false); }
        let Some(witness) = self.witness.filter(|witness| witness.safe && witness.observed <= progress.now) else {
            return Ok(false);
        };
        if self.last_closed.is_some_and(|closed| closed.foreground != claim.target
            && witness.revision <= closed.witness_revision)
        {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Err(Failure::ReadinessDeadline);
        }
        if !matches_target(claim)? { return Ok(false); }
        let sequence = next(self.sequence)?;
        let focus_generation = next(self.focus_generation)?;
        let binding = VisitBinding {
            epoch, source_generation: sequence, grant: sequence, witness_revision: witness.revision,
            foreground: claim.target, focus_generation: FocusGeneration(focus_generation.get()),
            focus_revision: witness.focus_revision, resync_revision: NonZeroU64::MIN,
        };
        let expires_qpc = add_time(progress.now, self.frequency, Duration::from_secs(1))?;
        let sent = authorization.with_closed_phase(|| self.lease.renew(&self.peer, progress, None,
            Some(|lease: screen_fix_protocol::ipc::elevated::runtime::Lease| {
                Control::Prepare(GrantRequest { foreground: claim.clone(), visit: binding, geometry, lease: lease.sequence, expires_qpc })
            })))?;
        if sent {
            self.sequence = sequence.get();
            self.focus_generation = focus_generation.get();
            self.grant = Some(Grant { claim: claim.clone(), binding, requested: progress.now,
                deadline, phase: Phase::Preparing, lower: progress.now, focus: None, registration: None, closing_before: None });
        }
        Ok(sent)
    }

    pub(super) fn activate(&mut self, epoch: AdmissionEpoch, opening: QpcTick, progress: Progress) -> Result<()> {
        let Some(grant) = self.grant.as_mut() else { return Err(Failure::Protocol); };
        if grant.binding.epoch != epoch || opening < grant.lower || opening > progress.now
            || !matches!(grant.phase, Phase::Armed(_)) || !grant.current(self.witness)?
        {
            return Err(Failure::TargetChanged);
        }
        let binding = grant.binding;
        if self.lease.renew(&self.peer, progress, Some(epoch), Some(|lease: screen_fix_protocol::ipc::elevated::runtime::Lease| {
            Control::Activate(Activation { visit: binding, lease: lease.sequence, opening_qpc: opening })
        }))? {
            grant.lower = opening;
            grant.phase = Phase::Activating;
            grant.deadline = Instant::now() + READINESS;
        }
        Ok(())
    }

    pub(super) fn maintain(&mut self, progress: Progress) -> Result<()> {
        if !self.configured { return Ok(()); }
        let epoch = self.lease_epoch();
        if let Some(grant) = self.grant.as_ref() {
            if !matches!(grant.phase, Phase::Open) && Instant::now() >= grant.deadline {
                return Err(Failure::ReadinessDeadline);
            }
            if self.lease.due(progress, epoch) && !matches!(grant.phase, Phase::Closing) && !matches_target(&grant.claim)? {
                return Err(Failure::TargetChanged);
            }
        }
        self.lease.renew(&self.peer, progress, epoch, None::<fn(_) -> _>)?;
        Ok(())
    }

    pub(super) fn revoke(&mut self, before: QpcTick) -> Result<()> {
        let Some(grant) = self.grant.as_ref() else { return Ok(()); };
        if !matches!(grant.phase, Phase::Closing) {
            self.peer.send(Control::Revoke { visit: grant.binding, before_qpc: before })?;
            self.mark_closing(before);
        }
        Ok(())
    }

    pub(super) fn refresh_focus(&mut self, progress: Progress) -> Result<bool> {
        let Some(grant) = self.grant.as_mut() else { return Ok(false); };
        if !matches!(grant.phase, Phase::FocusSuspended) { return Ok(false); }
        let Some(witness) = self.witness.filter(|witness| witness.safe
            && witness.revision == grant.binding.witness_revision && witness.focus_revision > grant.binding.focus_revision
            && witness.observed <= progress.now) else { return Ok(false); };
        if !matches_target(&grant.claim)? { return Err(Failure::TargetChanged); }
        let previous = grant.binding;
        let generation = next(self.focus_generation)?;
        let next = VisitBinding {
            focus_generation: FocusGeneration(generation.get()), focus_revision: witness.focus_revision, ..previous
        };
        let sent = self.lease.renew(&self.peer, progress, Some(previous.epoch), Some(|lease: screen_fix_protocol::ipc::elevated::runtime::Lease| {
            Control::RefreshFocus { previous, next, lease: lease.sequence }
        }))?;
        if sent {
            self.focus_generation = generation.get();
            grant.phase = Phase::Refreshing { next };
            grant.deadline = Instant::now() + READINESS;
        }
        Ok(sent)
    }

    pub(super) fn current(&self) -> bool {
        self.grant.as_ref().is_some_and(|grant| {
            matches!(grant.phase, Phase::Open)
                && grant.focus.is_some_and(|focus| focus_current(grant.binding, focus))
                && grant.witness_current(self.witness)
                && screen_fix_native_observation::held::on_input_desktop()
        })
    }

    pub(super) fn status(&mut self, status: Status, now: QpcTick) -> Result<Change> {
        match status {
            Status::Configured if !self.configured => {
                self.configured = true;
                return Ok(Change::Configured);
            }
            Status::Witness { revision, focus_revision, safe, observed_qpc } => {
                if !self.configured || observed_qpc > now || self.witness.is_some_and(|old| {
                    old.revision > revision || old.focus_revision > focus_revision || old.observed > observed_qpc
                }) {
                    return Err(Failure::Witness);
                }
                self.witness = Some(Witness { revision, focus_revision, safe, observed: observed_qpc });
                return Ok(Change::None);
            }
            Status::ReadRejected { read, reason } if self.optional_reads.contains(&read) => {
                self.optional_reads.retain(|pending| *pending != read);
                return Ok(Change::ReadRejected { read, reason });
            }
            Status::Stopped => return Ok(Change::Stopped),
            Status::Failed(failure) => return Err(Failure::NativeProducer(failure)),
            Status::TransitionRejected { visit, transition: screen_fix_protocol::ipc::elevated::runtime::Transition::Revoke,
                reason: ReadUnavailable::Stale } if self.last_closed == Some(visit) => return Ok(Change::None),
            _ => {}
        }
        let grant = self.grant.as_mut().ok_or(Failure::Protocol)?;
        let binding = grant.binding;
        if matches!(grant.phase, Phase::Closing) {
            let replacement = match &status {
                Status::ResyncRequired { previous, visit, .. } if *previous == binding
                    && (VisitBinding { resync_revision: visit.resync_revision, ..binding }) == *visit
                    && next(binding.resync_revision.get())? == visit.resync_revision => Some(*visit),
                Status::FocusRefreshed { visit, .. } if (VisitBinding {
                    focus_generation: visit.focus_generation, focus_revision: visit.focus_revision, ..binding
                }) == *visit && visit.focus_revision > binding.focus_revision
                    && visit.focus_generation.0 == self.focus_generation => Some(*visit),
                _ => None,
            };
            if let Some(visit) = replacement {
                let before_qpc = grant.closing_before.ok_or(Failure::Protocol)?;
                self.last_closed = Some(binding);
                grant.binding = visit;
                self.peer.send(Control::Revoke { visit, before_qpc })?;
                return Ok(Change::None);
            }
            match &status {
                Status::Armed { visit, .. } | Status::Activated { visit, .. }
                | Status::ResyncAccepted { visit, .. } | Status::FocusSuspended { visit, .. }
                | Status::TransitionRejected { visit, .. } if *visit == binding => return Ok(Change::None),
                Status::ReadRejected { read, .. } if read.visit == binding => return Ok(Change::None),
                _ => {}
            }
        }
        match status {
            Status::Armed { visit, arm_qpc, registration, focus, .. }
                if matches!(grant.phase, Phase::Preparing) && visit == binding
                    && grant.requested <= arm_qpc && arm_qpc <= now =>
            {
                if !grant.current(self.witness)? || !focus_current(binding, focus) {
                    return Err(Failure::TargetChanged);
                }
                grant.lower = arm_qpc;
                grant.focus = Some(focus);
                grant.registration = Some(registration);
                grant.phase = Phase::Armed(registration);
                return Ok(Change::Registered { binding, at: arm_qpc, registration });
            }
            Status::Activated { visit, activation_qpc }
                if matches!(grant.phase, Phase::Activating) && visit == binding
                    && grant.lower <= activation_qpc && activation_qpc <= now =>
            {
                grant.lower = activation_qpc;
                self.request_held(now)?;
            }
            Status::ResyncAccepted { visit, after_qpc, held, input_coverage }
                if visit == binding && after_qpc <= now =>
            {
                let Phase::Accepting { completed, snapshot } = grant.phase else { return Err(Failure::Protocol); };
                if completed != after_qpc || input_coverage.before_qpc < completed || input_coverage.first_qpc > completed
                    || (held == HeldReadiness::Known) != snapshot.is_some()
                {
                    return Err(Failure::Protocol);
                }
                let focus = grant.focus.ok_or(Failure::Protocol)?;
                if !grant.current(self.witness)? || !focus_current(binding, focus) {
                    return Err(Failure::TargetChanged);
                }
                grant.lower = input_coverage.before_qpc;
                grant.phase = Phase::Open;
                self.retry = None;
                self.retry_after = snapshot.is_none().then(|| add_time(after_qpc, self.frequency, HELD_RETRY)).transpose()?;
                let registration = grant.registration.ok_or(Failure::Protocol)?;
                return Ok(Change::InputOpened { binding, focus, completed, snapshot, coverage: input_coverage, registration });
            }
            Status::FocusSuspended { visit, before_qpc } if visit == binding && before_qpc <= now => {
                grant.phase = Phase::FocusSuspended;
                grant.deadline = Instant::now() + READINESS;
                grant.focus = None;
                self.retry = None;
                self.retry_after = None;
                return Ok(Change::InputClosed { binding, before: before_qpc, loss: None });
            }
            Status::FocusRefreshed { visit, focus, resync_after_qpc }
                if matches!(grant.phase, Phase::Refreshing { next } if next == visit) && resync_after_qpc <= now =>
            {
                if !focus_current(visit, focus) { return Err(Failure::TargetChanged); }
                grant.binding = visit;
                grant.lower = resync_after_qpc;
                grant.focus = Some(focus);
                self.request_held(now)?;
            }
            Status::ResyncRequired { previous, visit, first_uncovered, resync_after_qpc, cause, native_gap }
                if matches!(grant.phase, Phase::Open) && previous == binding
                    && next(binding.resync_revision.get())? == visit.resync_revision
                    && (VisitBinding { resync_revision: visit.resync_revision, ..binding }) == visit
                    && first_uncovered <= resync_after_qpc && resync_after_qpc <= now =>
            {
                grant.binding = visit;
                grant.lower = resync_after_qpc;
                grant.deadline = Instant::now() + READINESS;
                self.retry = None;
                self.retry_after = None;
                self.request_held(now)?;
                return Ok(Change::InputClosed { binding, before: first_uncovered, loss: Some((cause, native_gap)) });
            }
            Status::Invalidated { visit, before_qpc, reason } if visit == binding && before_qpc <= now => {
                grant.closing_before = Some(grant.closing_before.map_or(before_qpc, |previous| previous.min(before_qpc)));
                if !matches!(grant.phase, Phase::Closing) {
                    grant.deadline = Instant::now() + Duration::from_millis(750);
                }
                grant.phase = Phase::Closing;
                return Ok(Change::Invalidated { binding, before: before_qpc, reason });
            }
            Status::Closed { visit, before_qpc } if visit == binding && before_qpc <= now => {
                self.grant = None;
                self.last_closed = Some(visit);
                return Ok(Change::Closed { binding, before: before_qpc });
            }
            Status::ReadRejected { read, .. } if matches!(grant.phase, Phase::HeldPending(expected) if expected == read) =>
                return Err(Failure::TargetAccess),
            Status::ReadRejected { read, .. } if self.retry == Some(read) => self.retry = None,
            Status::ReadRejected { read, .. } if matches!(grant.phase, Phase::Open)
                && read.visit == binding && read.request.0 <= self.request => {},
            Status::TransitionRejected { visit, reason, .. } if visit == binding
                || matches!(grant.phase, Phase::Refreshing { next } if next == visit) =>
            {
                return Err(if reason == ReadUnavailable::TimedOut { Failure::ReadinessDeadline } else { Failure::TargetChanged });
            }
            _ => return Err(Failure::Protocol),
        }
        Ok(Change::None)
    }

    pub(super) fn observation(&mut self, observation: &Observation, now: QpcTick) -> Result<Option<(VisitBinding, QpcTick, HeldSnapshot)>> {
        let Observation::Item { evidence, position, .. } = observation else { return Ok(None); };
        if let Some(read) = evidence.read_binding() {
            self.optional_reads.retain(|pending| *pending != read);
        }
        let ContextEvidence::Held(read) = evidence else { return Ok(None); };
        let Some(grant) = self.grant.as_mut() else { return Ok(None); };
        let expected = match grant.phase {
            Phase::HeldPending(expected) => expected,
            Phase::Open if self.retry == Some(read.binding) => {
                self.retry = None;
                if let (Some(started), Some(completed), ReadOutcome::Observed(snapshot)) =
                    (read.started_qpc, read.completed_qpc, &read.outcome)
                    && read.binding.requested_qpc <= started && started <= completed
                    && completed <= now && now < read.binding.deadline_qpc
                    && grant.current(self.witness)? && grant.focus.is_some_and(|focus| focus_current(grant.binding, focus))
                {
                    return Ok(Some((grant.binding, completed, *snapshot)));
                }
                return Ok(None);
            }
            _ => return Ok(None),
        };
        if read.binding != expected { return Ok(None); }
        if *position != NonZeroU64::MIN { return Err(Failure::Protocol); }
        if now >= expected.deadline_qpc { return Err(Failure::ReadinessDeadline); }
        let (Some(start), Some(completed)) = (read.started_qpc, read.completed_qpc) else {
            return Err(Failure::TargetAccess);
        };
        if start < expected.requested_qpc || completed < start || completed >= expected.deadline_qpc || completed > now
            || !grant.current(self.witness)?
        {
            return Err(Failure::TargetChanged);
        }
        let snapshot = match read.outcome {
            ReadOutcome::Observed(snapshot) => Some(snapshot),
            ReadOutcome::Unavailable(ReadUnavailable::Unavailable) => None,
            ReadOutcome::Unavailable(_) => return Err(Failure::TargetAccess),
        };
        self.peer.send(Control::AcceptResync { visit: expected.visit, request: expected.request, completed_qpc: completed })?;
        grant.phase = Phase::Accepting { completed, snapshot };
        Ok(None)
    }

    pub(super) fn retry_held(&mut self, now: QpcTick) -> Result<()> {
        let Some(grant) = self.grant.as_ref().filter(|grant| matches!(grant.phase, Phase::Open)) else { return Ok(()); };
        if self.retry.is_some_and(|read| read.deadline_qpc > now)
            || self.retry_after.is_none_or(|after| after > now) || now < grant.lower
        {
            return Ok(());
        }
        let request = next(self.request)?;
        let binding = ReadBinding { visit: grant.binding, request: RequestId(request.get()), target: grant.binding.foreground,
            requested_qpc: now, deadline_qpc: add_time(now, self.frequency, HELD_BUDGET)? };
        if let Some(previous) = self.retry {
            self.peer.query(ContextCommand::Cancel { visit: previous.visit, request: previous.request })?;
        }
        self.peer.query(ContextCommand::Read { query: Query::Held(binding), secondary: None })?;
        self.request = request.get();
        self.retry = Some(binding);
        self.retry_after = Some(add_time(now, self.frequency, HELD_RETRY)?);
        Ok(())
    }

    pub(super) fn optional_read(
        &mut self, mut binding: ReadBinding, build: impl FnOnce(ReadBinding) -> Query,
        secondary: Option<screen_fix_protocol::ipc::elevated::runtime::IdentityClaim>,
    ) -> Result<Option<ReadBinding>> {
        let recent = self.frequency.0.get();
        self.optional_reads.retain(|read| {
            let age = binding.requested_qpc.0.saturating_sub(read.deadline_qpc.0);
            age <= 0 || u64::try_from(age).is_ok_and(|age| age <= recent * REJECT_RETENTION.as_secs())
        });
        if self.optional_reads.len() == OPTIONAL_READ_CAPACITY { return Ok(None); }
        let request = next(self.request)?;
        binding.request = RequestId(request.get());
        if !self.peer.try_query(ContextCommand::Read { query: build(binding), secondary })? {
            return Ok(None);
        }
        self.request = request.get();
        self.optional_reads.push_back(binding);
        Ok(Some(binding))
    }

    pub(super) fn recovered(&mut self) { self.retry_after = None; }

    fn request_held(&mut self, now: QpcTick) -> Result<()> {
        let grant = self.grant.as_mut().ok_or(Failure::Protocol)?;
        if now < grant.lower || !grant.current(self.witness)? { return Err(Failure::TargetChanged); }
        let request = next(self.request)?;
        let binding = ReadBinding {
            visit: grant.binding, request: RequestId(request.get()), target: grant.binding.foreground,
            requested_qpc: now, deadline_qpc: add_time(now, self.frequency, HELD_BUDGET)?,
        };
        self.peer.query(ContextCommand::Read { query: Query::Held(binding), secondary: None })?;
        self.request = request.get();
        grant.phase = Phase::HeldPending(binding);
        Ok(())
    }
}

impl Grant {
    fn current(&self, witness: Option<Witness>) -> Result<bool> {
        if !self.witness_current(witness) { return Ok(false); }
        matches_target(&self.claim)
    }

    fn witness_current(&self, witness: Option<Witness>) -> bool {
        witness.is_some_and(|witness| witness.safe && witness.revision == self.binding.witness_revision
            && witness.focus_revision == self.binding.focus_revision)
    }
}

pub(super) fn matches_target(claim: &IdentityClaim) -> Result<bool> {
    use screen_fix_elevated::normal_capture_desktop;
    if !normal_capture_desktop(claim.target.thread) || !foreground_is(claim) { return Ok(false); }
    let access = foreground_access(claim.target.process, claim.image.as_str())?;
    Ok(access == ForegroundAccess::Elevated && foreground_is(claim) && normal_capture_desktop(claim.target.thread))
}

fn foreground_is(claim: &IdentityClaim) -> bool {
    WindowProcess::foreground().map(screen_fix_protocol::ipc::elevated::WindowInstance::from) == Some(claim.target)
}

fn focus_current(binding: VisitBinding, focus: FocusAtReceipt) -> bool {
    use screen_fix_native_observation::windows::FocusSnapshot;
    use screen_fix_protocol::ipc::elevated::WindowInstance;
    let Some(window) = WindowProcess::foreground().filter(|window| WindowInstance::from(*window) == binding.foreground)
        else { return false; };
    FocusSnapshot::read(window).is_some_and(|observed| observed.matches_identity(focus)
        && observed.focus.password_style.is_none_or(|local| focus.password_style == Some(local)))
}

fn next(previous: u64) -> Result<NonZeroU64> {
    previous.checked_add(1).and_then(NonZeroU64::new).ok_or(Failure::Protocol)
}

pub(super) fn add_time(at: QpcTick, frequency: QpcFrequency, duration: Duration) -> Result<QpcTick> {
    let delta = duration.as_nanos().checked_mul(u128::from(frequency.0.get())).ok_or(Failure::Protocol)? / 1_000_000_000;
    let delta = i64::try_from(delta).map_err(|_| Failure::Protocol)?;
    at.0.checked_add(delta.max(1)).map(QpcTick).ok_or(Failure::Protocol)
}
