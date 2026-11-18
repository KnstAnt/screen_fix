//! The two evidence egress lanes: granted input and context observations.

use std::{
    sync::{Arc, mpsc::Receiver as Queue},
    time::Instant,
};

use screen_fix_protocol::ipc::elevated::{
    ContextEvidence, GrantedInput, InputEvidence, VisitBinding,
    runtime::{CoverageReason, Observation, Record},
};

use crate::{deadline::Cancellation, win::{Failure, Result}};
use super::{
    Sender,
    grant::{Grant, InputLoss},
    input,
    lifetime::{ProviderKind, qpc},
    outbox::{self, Outbox},
    timing::{ESSENTIAL_PROGRESS, GATE, POLL},
};

/// A queued egress record and its grant.
trait Outgoing {
    type Record: Record;

    fn grant(&self) -> &Arc<Grant>;

    /// The binding of the record, and whether it may leave while the grant
    /// is still opening.
    fn scope(&self) -> (VisitBinding, bool);

    fn record(&self) -> &Self::Record;

    /// Rechecks the record before qualification.
    fn gate(&mut self) -> Result<()> { Ok(()) }

    /// Credits the coverage of a completely written record.
    fn sent(self);
}

impl Outgoing for input::Item {
    type Record = GrantedInput;

    fn grant(&self) -> &Arc<Grant> { &self.grant }

    fn scope(&self) -> (VisitBinding, bool) { (self.record.visit, false) }

    fn record(&self) -> &GrantedInput { &self.record }

    fn sent(self) {
        let visit = self.record.visit;
        match self.record.evidence {
            InputEvidence::Progress(progress) => self.grant.input_progress(visit, progress),
            // A gap carried from the previous binding only counts; a gap
            // reaching past the floor needs a resync (E3).
            InputEvidence::Gap(gap) => if let Ok(floor) = self.grant.read_floor(false) && gap.last_qpc > floor {
                self.grant.input_loss(visit, InputLoss {
                    before: gap.first_qpc.max(floor), reason: CoverageReason::NativeGap, gap: Some(gap),
                });
            },
            _ => {}
        }
    }
}

impl Outgoing for outbox::Item {
    type Record = Observation;

    fn grant(&self) -> &Arc<Grant> { &self.grant }

    fn scope(&self) -> (VisitBinding, bool) { (self.visit, self.allow_opening) }

    fn record(&self) -> &Observation { &self.evidence }

    /// A pending read or commit is rechecked under a provider-backed budget.
    fn gate(&mut self) -> Result<()> {
        if let Some(delivery) = &self.delivery
            && self.visit == self.grant.visit() && self.grant.read_floor(false).is_ok()
        {
            let lifetime = &self.grant.session().lifetime;
            let deadline = lifetime.after(qpc()?, GATE)?;
            let Some(_provider) = lifetime.provider(ProviderKind::Egress, deadline) else {
                return Err(Failure::Supervisor);
            };
            delivery.gate(&self.grant, &mut self.evidence)?;
        }
        Ok(())
    }

    fn sent(self) {
        match self.evidence {
            Observation::Progress { before_qpc, .. } => self.grant.context_progress(self.visit, before_qpc),
            Observation::Item { evidence: ContextEvidence::Held(read), .. } => {
                if let Some(end) = read.completed_qpc {
                    self.grant.deliver_held(self.visit, read.binding.request, end);
                }
            }
            _ => {}
        }
    }
}

/// The input egress lane.
pub(super) fn input_egress(
    cancel: &Cancellation, queue: Queue<input::Item>, mut writer: Sender<GrantedInput>,
) -> Result<()> {
    egress(cancel, &queue, &mut writer, || Ok(()))
}

/// The context egress lane. Pending losses are flushed before each pass.
pub(super) fn context_egress(
    cancel: &Cancellation, queue: Queue<outbox::Item>, mut writer: Sender<Observation>, losses: Outbox,
) -> Result<()> {
    egress(cancel, &queue, &mut writer, || losses.flush_losses())
}

fn egress<I: Outgoing>(
    cancel: &Cancellation, queue: &Queue<I>, writer: &mut Sender<I::Record>, mut pass: impl FnMut() -> Result<()>,
) -> Result<()> {
    while cancel.check().is_ok() {
        pass()?;
        let mut verified = None;
        for mut item in batch(queue) {
            verify_once(&mut verified, item.grant());
            item.gate()?;
            let (visit, held) = item.scope();
            // One outstanding write; later records wait in the bounded
            // queue, whose producers drop newest with loss accounting. Only
            // a write stalled past the essential coverage budget fails the
            // lane (E3).
            if item.grant().qualify_egress(visit, held)
                && writer.send_scoped(item.record(), Instant::now() + ESSENTIAL_PROGRESS, item.grant(), visit)?
            {
                item.sent();
            }
        }
    }
    Ok(())
}

/// The records of one egress pass: the first waits up to a poll interval, the
/// rest are already queued.
fn batch<T>(queue: &Queue<T>) -> impl Iterator<Item = T> + '_ {
    queue.recv_timeout(POLL).ok().into_iter().chain(std::iter::from_fn(move || queue.try_recv().ok())).take(input::SLOTS)
}

/// Token facts are queried once per egress batch and grant; per-record checks
/// read the result latched in the grant (E3).
fn verify_once(verified: &mut Option<Arc<Grant>>, grant: &Arc<Grant>) {
    if verified.as_ref().is_none_or(|last| !Arc::ptr_eq(last, grant)) {
        // A failure is latched; `qualify_egress` then refuses the records.
        let _ = grant.verify_token();
        *verified = Some(Arc::clone(grant));
    }
}
