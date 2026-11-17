use std::{
    collections::VecDeque,
    num::NonZeroU64,
    sync::{Arc, Mutex, mpsc::{self, Receiver, SyncSender, TrySendError}},
};

use screen_fix_protocol::{
    QpcTick,
    ipc::elevated::{
        ContextEvidence, VisitBinding,
        runtime::{ContextGap, ContextProducer, Observation},
    },
};

use crate::win::{Failure, Result};
use super::{context::Delivery, grant::Grant};

const CAPACITY: usize = 1024;
const PRODUCERS: usize = 5;

pub(super) struct Item {
    pub(super) grant: Arc<Grant>,
    pub(super) visit: VisitBinding,
    pub(super) evidence: Observation,
    pub(super) allow_opening: bool,
    pub(super) delivery: Option<Delivery>,
}

#[derive(Clone)]
pub(super) struct Outbox {
    send: SyncSender<Item>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    visit: Option<VisitBinding>,
    positions: [u64; PRODUCERS],
    lost: VecDeque<Item>,
}

impl Outbox {
    pub(super) fn new() -> (Self, Receiver<Item>) {
        let (send, receive) = mpsc::sync_channel(CAPACITY);
        (Self { send, state: Arc::new(Mutex::new(State::default())) }, receive)
    }

    pub(super) fn publish(
        &self, grant: &Arc<Grant>, visit: VisitBinding, evidence: ContextEvidence, allow_opening: bool,
    ) -> Result<bool> {
        self.publish_checked(grant, visit, evidence, allow_opening, None)
    }

    pub(super) fn publish_checked(
        &self, grant: &Arc<Grant>, visit: VisitBinding, evidence: ContextEvidence, allow_opening: bool,
        delivery: Option<Delivery>,
    ) -> Result<bool> {
        let (producer, time) = producer_and_time(&evidence);
        let mut state = self.state.lock().map_err(|_| Failure::Supervisor)?;
        if !state.select(grant, visit) { return Ok(false); }
        self.flush(&mut state)?;
        let first = state.reserve(producer, NonZeroU64::MIN)?.0;
        let gap = ContextGap {
            visit, producer, first_position: first, last_position: first, first_qpc: time, last_qpc: time,
        };
        if !state.lost.is_empty() {
            state.lost(grant, gap, allow_opening);
            return Ok(false);
        }
        let evidence = Observation::Item { producer, position: first, evidence };
        let item = Item { grant: Arc::clone(grant), visit, evidence, allow_opening, delivery };
        match self.send.try_send(item) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => {
                state.lost(grant, gap, allow_opening);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => Err(Failure::Transport),
        }
    }

    pub(super) fn lost(
        &self, grant: &Arc<Grant>, visit: VisitBinding, producer: ContextProducer,
        first_qpc: QpcTick, last_qpc: QpcTick, count: NonZeroU64,
    ) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Failure::Supervisor)?;
        if !state.select(grant, visit) { return Ok(()); }
        self.flush(&mut state)?;
        let (first_position, last_position) = state.reserve(producer, count)?;
        state.lost(grant, ContextGap {
            visit, producer, first_position, last_position, first_qpc, last_qpc,
        }, false);
        self.flush(&mut state)
    }

    pub(super) fn progress(&self, grant: &Arc<Grant>, visit: VisitBinding, before_qpc: QpcTick, witness_qpc: QpcTick) -> Result<bool> {
        let mut state = self.state.lock().map_err(|_| Failure::Supervisor)?;
        if !state.select(grant, visit) { return Ok(false); }
        self.flush(&mut state)?;
        if !state.lost.is_empty() { return Ok(false); }
        let item = Item {
            grant: Arc::clone(grant), visit, evidence: Observation::Progress { visit, before_qpc, witness_qpc },
            allow_opening: false, delivery: None,
        };
        match self.send.try_send(item) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(Failure::Transport),
        }
    }

    pub(super) fn flush_losses(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Failure::Supervisor)?;
        self.flush(&mut state)
    }

    fn flush(&self, state: &mut State) -> Result<()> {
        while let Some(item) = state.lost.pop_front() {
            if item.grant.evidence_cancel(item.visit).is_none() { continue; }
            match self.send.try_send(item) {
                Ok(()) => {}
                Err(TrySendError::Full(item)) => {
                    state.lost.push_front(item);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => return Err(Failure::Transport),
            }
        }
        Ok(())
    }
}

impl State {
    fn select(&mut self, grant: &Grant, visit: VisitBinding) -> bool {
        if grant.evidence_cancel(visit).is_none() { return false; }
        if self.visit != Some(visit) {
            self.visit = Some(visit);
            self.positions = [0; PRODUCERS];
            self.lost.clear();
        }
        true
    }

    fn reserve(&mut self, producer: ContextProducer, count: NonZeroU64) -> Result<(NonZeroU64, NonZeroU64)> {
        let slot = &mut self.positions[match producer {
            ContextProducer::Notices => 0,
            ContextProducer::NativeRead => 1,
            ContextProducer::Automation => 2,
            ContextProducer::Ime => 3,
            ContextProducer::TextEdit => 4,
        }];
        let last = slot.checked_add(count.get()).and_then(NonZeroU64::new).ok_or(Failure::Protocol)?;
        let first = NonZeroU64::new(*slot + 1).ok_or(Failure::Protocol)?;
        *slot = last.get();
        Ok((first, last))
    }

    fn lost(&mut self, grant: &Arc<Grant>, gap: ContextGap, allow_opening: bool) {
        if let Some(item) = self.lost.iter_mut().find(|item| {
            matches!(&item.evidence, Observation::Gap(old) if old.producer == gap.producer)
        }) {
            if let Observation::Gap(old) = &mut item.evidence {
                old.last_position = gap.last_position;
                old.first_qpc = old.first_qpc.min(gap.first_qpc);
                old.last_qpc = old.last_qpc.max(gap.last_qpc);
            }
            return;
        }
        self.lost.push_back(Item {
            grant: Arc::clone(grant), visit: gap.visit, evidence: Observation::Gap(gap), allow_opening, delivery: None,
        });
    }
}

fn producer_and_time(evidence: &ContextEvidence) -> (ContextProducer, QpcTick) {
    let completed = |read: &screen_fix_protocol::ipc::elevated::ReadBinding, end: Option<QpcTick>|
        end.unwrap_or(read.requested_qpc);
    match evidence {
        ContextEvidence::Notice(notice) => (ContextProducer::Notices, notice.observed_qpc),
        ContextEvidence::Window(read) => (ContextProducer::NativeRead, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::NativeFocus(read) => (ContextProducer::NativeRead, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::HungRead(read) => (ContextProducer::NativeRead, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::ClipboardFence(read) => (ContextProducer::NativeRead, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::Held(read) => (ContextProducer::NativeRead, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::Rejected(read) => (ContextProducer::NativeRead, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::FocusUia(read) | ContextEvidence::PointerUia { read, .. } =>
            (ContextProducer::Automation, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::ImeFocus(read) | ContextEvidence::ImeKey { read, .. } =>
            (ContextProducer::Ime, completed(&read.binding, read.completed_qpc)),
        ContextEvidence::Commit(commit) => (ContextProducer::TextEdit, commit.receipt_qpc),
    }
}
