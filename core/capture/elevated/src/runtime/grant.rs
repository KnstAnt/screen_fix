use std::{
    num::NonZeroU64,
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering::{Acquire, AcqRel, Release}}},
    time::Duration,
};

use screen_fix_protocol::{
    QpcTick, RequestId,
    ipc::{FocusAtReceipt, ProducerGap, elevated::{
        VisitBinding,
        runtime::{Activation, ApprovalKey, CoverageReason, GrantRequest, IdentityClaim, Lease, Revocation},
    }},
};

use crate::{deadline::Cancellation, process::Process, win::{Failure, Result}};
use super::{lifetime::{LEASE, Lifetime, qpc}, witness::{Foreground, Snapshot, Target, View}};

pub(super) const INPUT_HOLDBACK: Duration = Duration::from_millis(32);
const ESSENTIAL_PROGRESS: Duration = Duration::from_millis(500);

const PREPARING: u8 = 0;
const ARMED: u8 = 1;
const OPENING: u8 = 2;
const ACTIVE: u8 = 3;
const SUSPENDED: u8 = 4;
const CLOSED: u8 = 5;

struct Focus {
    binding: VisitBinding,
    native: Option<FocusAtReceipt>,
    abort: Arc<Cancellation>,
    progress: Coverage,
    delivered_held: Option<(RequestId, QpcTick)>,
}

#[derive(Default)]
struct Coverage {
    input: [i64; 4],
    context: i64,
    uncertain: Option<(Revocation, QpcTick)>,
    input_loss: Option<InputLoss>,
}

#[derive(Clone, Copy)]
pub(super) struct InputLoss {
    pub(super) before: QpcTick,
    pub(super) reason: CoverageReason,
    pub(super) gap: Option<ProducerGap>,
}

pub(super) struct Grant {
    pub(super) target: Target,
    pub(super) arm: QpcTick,
    pub(super) parent: Arc<Process>,
    pub(super) lifetime: Arc<Lifetime>,
    pub(super) stop: Cancellation,
    witness: View,
    focus: Mutex<Focus>,
    session_stop: Arc<Cancellation>,
    phase: AtomicU8,
    lower_qpc: AtomicI64,
    expires_qpc: AtomicI64,
    hook_revision: AtomicU64,
    scope_serial: AtomicU64,
    input_paused: AtomicBool,
}

impl Grant {
    pub(super) fn prepare(
        request: GrantRequest, approved: &[ApprovalKey], lease: Lease, parent: Arc<Process>,
        witness: View, lifetime: Arc<Lifetime>, session_stop: Arc<Cancellation>,
    ) -> Result<Arc<Self>> {
        let now = qpc()?;
        if request.lease != lease.sequence || lease.epoch.is_some()
            || request.expires_qpc <= now
            || request.expires_qpc.0 > now.0.checked_add(lifetime.ticks(LEASE)?).ok_or(Failure::Supervisor)?
            || request.visit.foreground != request.foreground.target
            || request.visit.epoch.0 == 0 || request.visit.focus_generation.0 == 0
            || request.visit.resync_revision != NonZeroU64::MIN
            || !approved.contains(&request.foreground.approval)
        {
            return Err(Failure::Protocol);
        }
        let observed = witness.read()?.ok_or(Failure::Witness)?;
        let target = Target::prove(observed.foreground.ok_or(Failure::TargetChanged)?, &request.foreground, &parent)?;
        let expires_qpc = AtomicI64::new(request.expires_qpc.0);
        let hook_revision = AtomicU64::new(request.visit.focus_revision);
        let focus = Focus {
            binding: request.visit, native: None, abort: Arc::new(Cancellation::new()?), progress: Coverage::default(), delivered_held: None,
        };
        let grant = Arc::new(Self {
            target, expires_qpc, hook_revision, focus: Mutex::new(focus), arm: qpc()?, parent, witness, lifetime, session_stop,
            stop: Cancellation::new()?, phase: AtomicU8::new(PREPARING), lower_qpc: AtomicI64::new(0),
            scope_serial: AtomicU64::new(1), input_paused: AtomicBool::new(false),
        });
        grant.check(true)?;
        Ok(grant)
    }

    pub(super) fn visit(&self) -> VisitBinding {
        self.focus.lock().expect("focus binding lock").binding
    }

    pub(super) fn expected_focus(&self, visit: VisitBinding) -> Option<FocusAtReceipt> {
        let focus = self.focus.lock().ok()?;
        (focus.binding == visit).then_some(focus.native).flatten()
    }

    pub(super) fn qualify_egress(&self, visit: VisitBinding, held: bool) -> bool {
        if self.evidence_cancel(visit).is_none() { return false; }
        let checked = self.read_floor(held).and_then(|_| self.check(true));
        if matches!(checked, Err(Failure::TargetChanged | Failure::TokenIdentity(_) | Failure::ProcessIdentity))
            && !self.witness.dirty()
            && let Ok(now) = qpc()
            && self.loss(now).is_none()
        {
            self.mark_uncertain(visit, self.arm);
        }
        checked.is_ok() && self.evidence_cancel(visit).is_some()
    }

    pub(super) fn evidence_cancel(&self, visit: VisitBinding) -> Option<Arc<Cancellation>> {
        let focus = self.focus.lock().ok()?;
        (focus.binding == visit && focus.abort.check().is_ok()).then(|| Arc::clone(&focus.abort))
    }

    pub(super) fn native_focus(&self) -> Option<FocusAtReceipt> {
        let current = self.target.window;
        let proof = screen_fix_native_observation::identity::WindowProcess {
            window: current.window, process: self.target.process.instance(), thread: current.thread,
        };
        screen_fix_native_observation::windows::FocusSnapshot::read(proof).map(|snapshot| snapshot.focus)
    }

    pub(super) fn loss(&self, now: QpcTick) -> Option<(Revocation, QpcTick)> {
        let (binding, uncertain) = match self.focus.lock() {
            Ok(focus) => (focus.binding, focus.progress.uncertain),
            Err(_) => return Some((Revocation::WitnessLost, self.arm)),
        };
        if uncertain.is_some() { return uncertain; }
        let expiry = QpcTick(self.expires_qpc.load(Acquire));
        if now >= expiry {
            return Some((Revocation::Expired, expiry));
        }
        let view = match self.witness.read() {
            Ok(Some(view)) if self.lifetime.witness_healthy(now) => view,
            _ => return Some((Revocation::WitnessLost, self.arm)),
        };
        let cut = || {
            self.witness.boundary_since(binding.focus_revision).unwrap_or(QpcTick(0)).min(now).max(self.arm)
        };
        if !view.safe {
            return Some((Revocation::Environment, cut()));
        }
        if view.foreground != Some(self.target.window) || view.revision != binding.witness_revision {
            return Some((Revocation::Foreground, cut()));
        }
        if view.focus_revision != binding.focus_revision {
            return Some((Revocation::Focus, cut()));
        }
        None
    }

    pub(super) fn check(&self, detailed: bool) -> Result<QpcTick> {
        let now = qpc()?;
        self.session_stop.check()?;
        self.stop.check()?;
        self.lifetime.check(now)?;
        if self.witness.dirty() { return Err(Failure::ScopeClosed); }
        if self.loss(now).is_some() || Foreground::read() != Some(self.target.window) {
            return Err(Failure::TargetChanged);
        }
        if detailed {
            self.target.recheck(&self.parent)?;
            let expected = self.focus.lock().map_err(|_| Failure::Supervisor)?.native;
            if expected.is_some_and(|focus| self.native_focus() != Some(focus)) {
                return Err(Failure::TargetChanged);
            }
            return self.check(false);
        }
        Ok(now)
    }

    pub(super) fn prove_other(
        &self, local: Foreground, claim: &IdentityClaim, approved: &[ApprovalKey],
    ) -> Result<Target> {
        self.check(false)?;
        if !approved.contains(&claim.approval) {
            return Err(Failure::TargetChanged);
        }
        let target = Target::prove(local, claim, &self.parent)?;
        self.check(false)?;
        Ok(target)
    }

    pub(super) fn observe_readiness_focus(&self) -> Result<FocusAtReceipt> {
        self.check(true)?;
        let native = self.native_focus().ok_or(Failure::TargetChanged)?;
        self.focus.lock().map_err(|_| Failure::Supervisor)?.native = Some(native);
        self.check(true)?;
        Ok(native)
    }

    pub(super) fn arm(&self) -> Result<()> {
        self.check(true)?;
        self.transition(PREPARING, ARMED)
    }

    pub(super) fn suspend_focus(&self) -> Result<bool> {
        let focus = self.focus.lock().map_err(|_| Failure::Supervisor)?;
        if self.phase.load(Acquire) == SUSPENDED { return Ok(false); }
        self.transition(ACTIVE, SUSPENDED)?;
        focus.abort.cancel();
        Ok(true)
    }

    pub(super) fn refresh_focus(
        &self, previous: VisitBinding, next: VisitBinding, lease: Lease,
    ) -> Result<(FocusAtReceipt, QpcTick)> {
        if self.phase.load(Acquire) != SUSPENDED || previous != self.visit()
            || next != (VisitBinding { focus_generation: next.focus_generation, focus_revision: next.focus_revision, ..previous })
            || next.focus_generation.0 <= previous.focus_generation.0 || next.focus_revision <= previous.focus_revision
            || lease.epoch != Some(next.epoch)
        {
            return Err(Failure::Protocol);
        }
        self.session_stop.check()?;
        self.stop.check()?;
        self.lifetime.check(qpc()?)?;
        self.target.recheck(&self.parent)?;
        let view = self.witness.read()?.filter(|view| view.safe && view.foreground == Some(self.target.window)
            && view.revision == next.witness_revision && view.focus_revision == next.focus_revision)
            .ok_or(Failure::TargetChanged)?;
        let native = self.native_focus().ok_or(Failure::TargetChanged)?;
        let now = qpc()?;
        if now >= QpcTick(self.expires_qpc.load(Acquire)) || view.observed > now {
            return Err(Failure::TargetChanged);
        }
        let mut focus = self.focus.lock().map_err(|_| Failure::Supervisor)?;
        let abort = Arc::new(Cancellation::new()?);
        if focus.binding != previous || self.witness.dirty() || self.witness.revision() != next.focus_revision {
            return Err(Failure::TargetChanged);
        }
        self.transition(SUSPENDED, OPENING)?;
        self.lower_qpc.store(now.0, Release);
        *focus = Focus { binding: next, native: Some(native), abort, progress: Coverage::default(), delivered_held: None };
        self.hook_revision.store(next.focus_revision, Release);
        self.input_paused.store(false, Release);
        self.bump_scope()?;
        Ok((native, now))
    }

    pub(super) fn activate(&self, command: Activation, lease: Lease) -> Result<QpcTick> {
        let now = self.check(true)?;
        if command.visit != self.visit() || lease.epoch != Some(command.visit.epoch)
            || command.lease != lease.sequence || command.opening_qpc < self.arm || command.opening_qpc > now
        {
            return Err(Failure::Protocol);
        }
        self.lower_qpc.store(now.0, Release);
        self.transition(ARMED, OPENING)?;
        Ok(now)
    }

    pub(super) fn read_floor(&self, held: bool) -> Result<QpcTick> {
        let phase = self.phase.load(Acquire);
        if phase != ACTIVE && !(held && phase == OPENING) {
            return Err(Failure::TargetChanged);
        }
        self.check(false)?;
        if self.input_paused.load(Acquire) { return Err(Failure::ScopeClosed); }
        Ok(QpcTick(self.lower_qpc.load(Acquire)))
    }

    pub(super) fn opening(&self) -> bool { self.phase.load(Acquire) == OPENING }

    pub(super) fn deliver_held(&self, visit: VisitBinding, request: RequestId, sample: QpcTick) {
        if let Ok(mut focus) = self.focus.lock() && focus.binding == visit
            && self.opening() && focus.abort.check().is_ok()
        {
            focus.delivered_held = Some((request, sample));
        }
    }

    pub(super) fn held_delivered(&self, visit: VisitBinding, request: RequestId, sample: QpcTick) -> bool {
        self.focus.lock().is_ok_and(|focus| {
            focus.binding == visit && focus.abort.check().is_ok()
                && focus.delivered_held == Some((request, sample))
        })
    }

    pub(super) fn finish_opening(&self, sample: QpcTick) -> Result<()> {
        let floor = self.read_floor(true)?;
        if sample < floor || sample > self.check(true)? || self.phase.load(Acquire) != OPENING {
            return Err(Failure::TargetChanged);
        }
        self.lower_qpc.store(sample.0, Release);
        self.transition(OPENING, ACTIVE)
    }

    pub(super) fn renew(&self, lease: Lease) -> Result<()> {
        if lease.epoch.is_some_and(|epoch| epoch != self.visit().epoch)
            || self.phase.load(Acquire) >= OPENING && lease.epoch != Some(self.visit().epoch)
        {
            return Err(Failure::TargetChanged);
        }
        let now = qpc()?;
        if self.phase.load(Acquire) == SUSPENDED {
            if self.loss(now).is_some_and(|(reason, _)| reason != Revocation::Focus) {
                return Err(Failure::TargetChanged);
            }
            self.target.recheck(&self.parent)?;
        } else {
            self.check(true)?;
        }
        let bound = lease.controller_qpc.min(lease.pump_qpc).0
            .checked_add(self.lifetime.ticks(LEASE)?).ok_or(Failure::Supervisor)?;
        if bound <= now.0 { return Err(Failure::LeaseExpired); }
        self.expires_qpc.store(bound, Release);
        Ok(())
    }

    pub(super) fn hook_open(&self) -> bool {
        self.phase.load(Acquire) == ACTIVE && !self.input_paused.load(Acquire)
            && !self.witness.dirty() && self.witness.revision() == self.hook_revision.load(Acquire)
            && self.stop.check().is_ok() && self.session_stop.check().is_ok()
            && qpc().is_ok_and(|now| now.0 < self.expires_qpc.load(Acquire)
                && self.lifetime.lease_current(now) && self.lifetime.witness_healthy(now))
    }

    pub(super) fn hook_stamp(&self) -> Option<u64> {
        let serial = self.scope_serial();
        (self.hook_open() && self.scope_serial() == serial).then_some(serial)
    }

    pub(super) fn scope_serial(&self) -> u64 { self.scope_serial.load(Acquire) }

    fn bump_scope(&self) -> Result<()> {
        self.scope_serial.fetch_update(AcqRel, Acquire, |serial| serial.checked_add(1))
            .map(|_| ()).map_err(|_| Failure::Supervisor)
    }

    pub(super) fn input_loss(&self, visit: VisitBinding, loss: InputLoss) {
        if let Ok(mut focus) = self.focus.lock() && focus.binding == visit
            && self.phase.load(Acquire) == ACTIVE && focus.abort.check().is_ok()
        {
            focus.progress.input_loss = Some(loss);
            self.input_paused.store(true, Release);
            focus.abort.cancel();
        }
    }

    pub(super) fn reopen_input(&self) -> Result<Option<(VisitBinding, VisitBinding, InputLoss, QpcTick)>> {
        if !self.input_paused.load(Acquire) { return Ok(None); }
        let now = self.check(true)?;
        let mut focus = self.focus.lock().map_err(|_| Failure::Supervisor)?;
        let Some(loss) = focus.progress.input_loss else { return Ok(None); };
        let previous = focus.binding;
        let revision = previous.resync_revision.get().checked_add(1).and_then(NonZeroU64::new).ok_or(Failure::Supervisor)?;
        let next = VisitBinding { resync_revision: revision, ..previous };
        let abort = Arc::new(Cancellation::new()?);
        self.transition(ACTIVE, OPENING)?;
        self.lower_qpc.store(now.0, Release);
        *focus = Focus { binding: next, native: focus.native, abort, progress: Coverage::default(), delivered_held: None };
        self.bump_scope()?;
        self.input_paused.store(false, Release);
        Ok(Some((previous, next, loss, now)))
    }

    pub(super) fn essential_stalled(&self, now: QpcTick) -> Result<bool> {
        if self.phase.load(Acquire) != ACTIVE || self.input_paused.load(Acquire) {
            return Ok(false);
        }
        let floor = self.lower_qpc.load(Acquire);
        let focus = self.focus.lock().map_err(|_| Failure::Supervisor)?;
        let least = focus.progress.input.into_iter().min().unwrap_or(0)
            .min(focus.progress.context).max(floor);
        Ok(now.0.saturating_sub(least) >= self.lifetime.ticks(ESSENTIAL_PROGRESS)?)
    }

    pub(super) fn input_progress(&self, visit: VisitBinding, progress: screen_fix_protocol::ipc::ProducerProgress) {
        use screen_fix_protocol::ipc::Producer;
        let index = match progress.producer {
            Producer::RawInput => 0,
            Producer::KeyboardHook => 1,
            Producer::PointerHook => 2,
            Producer::DeviceNotifications => 3,
        };
        self.credit(visit, progress.before_qpc, |coverage| &mut coverage.input[index]);
    }

    pub(super) fn context_progress(&self, visit: VisitBinding, before_qpc: QpcTick) {
        self.credit(visit, before_qpc, |coverage| &mut coverage.context);
    }

    fn credit(&self, visit: VisitBinding, before: QpcTick, slot: impl FnOnce(&mut Coverage) -> &mut i64) {
        // A completed write can belong to a closed visit. Qualification is a
        // separate operation, after a fresh witness and exact binding check.
        let Ok(now) = self.check(true) else { return };
        let Ok(Some(witness)) = self.witness.read() else { return };
        if let Ok(mut focus) = self.focus.lock()
            && focus.binding == visit && focus.abort.check().is_ok()
            && self.phase.load(Acquire) == ACTIVE && !self.witness.dirty() && self.witness.revision() == visit.focus_revision
            && witness.revision == visit.witness_revision
            && witness.focus_revision == visit.focus_revision
            && self.lifetime.check(now).is_ok()
        {
            let previous = slot(&mut focus.progress);
            *previous = (*previous).max(before.min(now).min(witness.observed).0);
        }
    }

    pub(super) fn certified(&self, visit: VisitBinding, known_cut: QpcTick) -> QpcTick {
        let Ok(focus) = self.focus.lock() else { return self.arm.min(known_cut) };
        if focus.binding != visit {
            return self.arm.min(known_cut);
        }
        let input = focus.progress.input.into_iter().min().unwrap_or(0);
        let proven = QpcTick(input.min(focus.progress.context)).max(self.arm);
        known_cut.min(proven).min(focus.progress.uncertain.map_or(proven, |(_, cut)| cut))
            .min(focus.progress.input_loss.map_or(proven, |loss| loss.before))
    }

    pub(super) fn mark_uncertain(&self, visit: VisitBinding, before: QpcTick) {
        self.invalidate(visit, Revocation::WitnessLost, before);
    }

    pub(super) fn invalidate(&self, visit: VisitBinding, reason: Revocation, before: QpcTick) {
        if let Ok(mut focus) = self.focus.lock() && focus.binding == visit {
            if focus.progress.uncertain.is_none_or(|(_, old)| before < old) {
                focus.progress.uncertain = Some((reason, before));
            }
            focus.abort.cancel();
        }
    }

    pub(super) fn witnessed_at(&self, visit: VisitBinding) -> Option<QpcTick> {
        if self.witness.dirty() || self.visit() != visit { return None; }
        let witness = self.witness.read().ok().flatten()?;
        (witness.safe && witness.revision == visit.witness_revision && witness.focus_revision == visit.focus_revision
            && witness.foreground == Some(self.target.window)).then_some(witness.observed)
    }

    pub(super) fn stopped(&self) -> bool {
        self.stop.check().is_err() || self.session_stop.check().is_err()
    }

    pub(super) fn revoke(&self) {
        if let Ok(focus) = self.focus.lock() {
            self.phase.store(CLOSED, Release);
            focus.abort.cancel();
        }
        self.stop.cancel();
    }

    fn transition(&self, from: u8, to: u8) -> Result<()> {
        self.phase.compare_exchange(from, to, AcqRel, Acquire).map(|_| ()).map_err(|_| Failure::TargetChanged)
    }
}

pub(super) fn await_witness(
    view: &View, cancellation: &Cancellation, until: std::time::Instant,
) -> Result<Snapshot> {
    while std::time::Instant::now() < until {
        cancellation.check()?;
        if let Some(view) = view.read()? {
            return Ok(view);
        }
        std::thread::park_timeout(Duration::from_millis(4));
    }
    Err(Failure::Witness)
}
