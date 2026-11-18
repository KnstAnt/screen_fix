//! Native observation selection runs only on the capture owner.
//!
//! - `slot`: public slots and the still rows they write;
//! - `private`: private openings, held until their action is approved;
//! - `scroll`: scroll bursts;
//! - `settle`: the quiet tracker of settling slots;
//! - `work`: the shared readback and PNG work of held frames.

mod private;
mod scroll;
mod settle;
mod slot;
mod work;

use std::{
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, atomic::{AtomicI64, Ordering}, mpsc::{Receiver, SyncSender, sync_channel}},
    thread::{self, Thread},
};

use crate::{
    capture::{frame::FrameMetadata, observation::ProvisionalInputObservation, save::{SaveKind, SaveObservation}},
    config::ResolvedCaptureConfig,
    package::metadata::MonitorCapture,
    protocol::{AdmissionEpoch, FrameId, GeometryId, SessionTimeUs, ipc::{ForegroundAtReceipt, elevated::VisitBinding}},
    session::AdmissionBoundary,
};

use super::{
    context::{ELEVATED_FRAME_WAIT_US, FrameApproval, FramePolicy, PointerLink},
    native::{HeldFrame, Holds, NativeFrame, NativeImages},
    readback, still_png, still_rows,
    still_time::{self, Interval, Missing},
};

pub(super) use private::{EarlyRows, Permission, trigger_rows};
pub(super) use slot::{DropReason, Role, StillRow, Trigger, fallback_row, save_slot};
#[cfg(test)]
pub(super) use slot::tests::saved_row;

use private::{Opening, Private, PrivateSelection};
use scroll::{MAX_SCROLLS, Scroll};
use settle::Quiet;
use slot::{Public, Selected, SlotOutcome, State};

const PRIVATE_SLOTS: usize = 128;
const CHANNEL: usize = 64;
/// Confirmed saves queued for this owner; one step drains at most this many.
const SAVE_QUEUE: usize = 8;
const PUBLIC_SLOTS: usize = 128;
const NEXT_US: i64 = 1_000_000;
const SETTLED_US: i64 = 10_000_000;
const OBSERVE_US: i64 = 2_000_000;

pub(super) struct Rest {
    pub reservation: still_rows::Reservation,
    pub at: SessionTimeUs,
    pub admission: AdmissionEpoch,
    pub geometry: GeometryId,
    pub proof: ForegroundAtReceipt,
    pub visit: Option<VisitBinding>,
    pub pointer: Option<PointerLink>,
}

pub(super) struct Save {
    pub reservation: still_rows::Reservation,
    pub confirmation: SaveObservation,
    pub geometry: GeometryId,
    pub admission: AdmissionEpoch,
    pub proof: ForegroundAtReceipt,
    pub visit: Option<VisitBinding>,
}

enum Command {
    Opening(Opening),
    Rest(Rest),
}

/// The input owner's progress, read by the still owner at each step.
#[derive(Default)]
struct Signals {
    /// The latest input activity; it postpones the next observe still.
    last_activity: AtomicI64,
    /// The input certification prefix (`Requests::input_progress`).
    input_before: AtomicI64,
    /// The latest approved action; it restarts the quiet run.
    last_action: AtomicI64,
}

#[derive(Clone)]
pub(super) struct Requests {
    send: SyncSender<Command>,
    saves: SyncSender<Save>,
    signals: Arc<Signals>,
    owner: Thread,
}

impl Requests {
    pub fn open(&self, request: ProvisionalInputObservation, visit: Option<VisitBinding>) -> Option<SyncSender<Permission>> {
        let (decision, resolved) = sync_channel(1);
        self.send.try_send(Command::Opening(Opening { request, visit, decision: resolved })).ok()?;
        self.owner.unpark();
        Some(decision)
    }

    pub fn input_progress(&self, before: SessionTimeUs) {
        self.signals.input_before.fetch_max(before.0, Ordering::Release);
    }

    pub fn action(&self, at: SessionTimeUs) {
        self.signals.last_action.fetch_max(at.0, Ordering::Release);
        self.owner.unpark();
    }

    pub fn activity(&self, at: SessionTimeUs) {
        self.signals.last_activity.fetch_max(at.0, Ordering::Relaxed);
    }

    pub fn confirmed_save(&self, save: Save) -> bool {
        let queued = self.saves.try_send(save).is_ok();
        if queued {
            self.owner.unpark();
        }
        queued
    }

    pub fn pointer_rest(&self, rest: Rest) {
        if self.send.try_send(Command::Rest(rest)).is_ok() {
            self.owner.unpark();
        }
    }
}

/// The observe timer: an idle still of a changed screen, at most once per
/// `OBSERVE_US` and not before `OBSERVE_US` after the latest input activity.
struct Observer {
    activity: SessionTimeUs,
    at: SessionTimeUs,
    frame: Option<FrameId>,
    /// The interval and source time of the latest changed frame.
    change: Option<(Interval, SessionTimeUs)>,
}

impl Observer {
    /// Forget the observed frame and the latest change.
    fn reset(&mut self) {
        self.frame = None;
        self.change = None;
    }
}

/// The inputs of one owner step.
#[derive(Clone, Copy)]
struct Step<'a> {
    images: &'a NativeImages,
    mode: &'a MonitorCapture,
    policy: &'a FramePolicy,
    config: &'a ResolvedCaptureConfig,
    admission: AdmissionEpoch,
    now: SessionTimeUs,
}

impl Step<'_> {
    /// The current capture interval under `approval`.
    fn interval(&self, approval: &FrameApproval) -> Interval {
        Interval::approved(self.mode.geometry.id, self.mode.capture_epoch, self.admission, approval)
    }
}

pub(super) struct Stills {
    input: Receiver<Command>,
    rows: still_rows::Rows,
    receipt: SessionTimeUs,
    saves: Receiver<Save>,
    deferred: Option<Command>,
    deferred_save: Option<Save>,
    signals: Arc<Signals>,
    seen_action: SessionTimeUs,
    observer: Observer,
    scrolls: Vec<Scroll>,
    holds: Holds,
    pending: VecDeque<Private>,
    public: VecDeque<Public>,
    start: Option<Public>,
    quiet: Quiet,
}

impl Stills {
    pub fn new(rows: still_rows::Rows, budget: Arc<readback::Budget>) -> (Requests, Self) {
        let (send, input) = sync_channel(CHANNEL);
        let (save_send, saves) = sync_channel(SAVE_QUEUE);
        let signals = Arc::new(Signals::default());
        let observer = Observer { activity: SessionTimeUs(0), at: SessionTimeUs(OBSERVE_US), frame: None, change: None };
        (Requests { send, saves: save_send, signals: Arc::clone(&signals), owner: thread::current() },
         Self { input, rows, receipt: SessionTimeUs(0), saves, deferred: None, deferred_save: None, signals,
             seen_action: SessionTimeUs(0), observer, scrolls: Vec::with_capacity(MAX_SCROLLS), holds: Holds::new(budget),
             pending: VecDeque::with_capacity(PRIVATE_SLOTS), public: VecDeque::with_capacity(PUBLIC_SLOTS), start: None,
             quiet: Quiet::default() })
    }

    pub fn arm_start(&mut self, boundary: AdmissionBoundary, geometry: GeometryId) {
        let mut start = Public::new(Trigger::Start, Role::Image, boundary.time, geometry, boundary.epoch,
            State::Selected(Selected::Waiting));
        start.reservation = self.rows.reserve_public(boundary.time, start.fallback(DropReason::StartFrameUnavailable));
        if start.reservation.is_some() {
            self.start = Some(start);
        }
    }

    pub fn step(
        &mut self, images: &NativeImages, mode: &MonitorCapture, policy: &FramePolicy,
        config: &ResolvedCaptureConfig, admission: AdmissionEpoch, now: SessionTimeUs,
    ) {
        let step = Step { images, mode, policy, config, admission, now };
        let input_before = SessionTimeUs(self.signals.input_before.load(Ordering::Acquire));
        self.receipt = self.receipt.max(now);
        let action = SessionTimeUs(self.signals.last_action.load(Ordering::Acquire));
        if action > self.seen_action && action <= now {
            self.seen_action = action;
            self.quiet.invalidate(action);
        }
        self.start_step(images, self.receipt);
        self.drain_saves(step);
        let openings_drained = self.drain_commands(step);
        self.select_private(images, now);
        self.resolve_private(images, config, now);
        let approval = policy.current(admission, now);
        let input_before = openings_drained.then_some(input_before);
        self.scroll_step(images, now, input_before);
        self.settle_step(step, approval.clone(), input_before);
        self.observe(step, approval);
        self.rows.advance(self.receipt);
    }

    /// Take the confirmed saves received by `now`. An elevated save waits for
    /// its frame approval up to `ELEVATED_FRAME_WAIT_US`.
    fn drain_saves(&mut self, step: Step<'_>) {
        let now = step.now;
        for _ in 0..SAVE_QUEUE {
            let Some(save) = self.deferred_save.take().or_else(|| self.saves.try_recv().ok()) else { break };
            if save.confirmation.time().recv_t_us > now {
                self.deferred_save = Some(save);
                break;
            }
            let at = save.confirmation.time().recv_t_us;
            let approval = step.policy.for_receipt(save.proof, step.admission, save.visit, at, now);
            if approval.is_err() && now.0.saturating_sub(at.0) < ELEVATED_FRAME_WAIT_US {
                self.deferred_save = Some(save);
                break;
            }
            self.save_step(save, step, approval.ok().flatten());
        }
    }

    /// Take the openings and pointer rests received by `now`, while private
    /// slots remain. True when every command received by `now` was taken.
    fn drain_commands(&mut self, step: Step<'_>) -> bool {
        let now = step.now;
        for _ in 0..CHANNEL {
            if self.pending.len() == PRIVATE_SLOTS {
                return false;
            }
            let Some(command) = self.deferred.take().or_else(|| self.input.try_recv().ok()) else {
                return true;
            };
            let at = match &command {
                Command::Opening(opening) => opening.request.time.recv_t_us,
                Command::Rest(rest) => rest.at,
            };
            if at > now {
                self.deferred = Some(command);
                return true;
            }
            let (proof, visit) = match &command {
                Command::Opening(opening) => (opening.request.foreground_at_receipt, opening.visit),
                Command::Rest(rest) => (Some(rest.proof), rest.visit),
            };
            let approval = proof.map_or(Ok(None), |proof| step.policy.for_receipt(proof, step.admission, visit, at, now));
            // A contended or newer-than-`now` read is transient for every source;
            // an elevated one also waits for its certificate. Retry it later.
            if approval.is_err() && now.0.saturating_sub(at.0) < ELEVATED_FRAME_WAIT_US {
                self.deferred = Some(command);
                return false;
            }
            let approval = approval.ok().flatten();
            match command {
                Command::Opening(opening) => self.open_private(opening, step, approval),
                Command::Rest(rest) => self.rest(rest, step, approval),
            }
        }
        false
    }

    fn start_step(&mut self, images: &NativeImages, now: SessionTimeUs) {
        let Some(start) = &self.start else { return };
        let eligible = |frame: &FrameMetadata| frame.covered() && frame.admission == start.admission
            && frame.geometry.id == start.geometry && frame.source_t_us >= start.at;
        // An evicted eligible frame preceded every frame still in history, so
        // a later frame is never presented as the first one (ALG §8.2).
        let first = if images.last_evicted().is_some_and(|frame| eligible(&frame)) {
            Some(Err(Missing::HistoryLost))
        } else {
            images.history().iter().find(|frame| eligible(&frame.metadata)).map(Ok)
        };
        if first.is_none() && now.0.saturating_sub(start.at.0) < SETTLED_US {
            return;
        }
        let Some(mut start) = self.start.take() else { return };
        let selected = self.retain(first.unwrap_or(Err(Missing::NoEligibleFrame)), still_png::Priority::Start);
        start.state = State::Selected(selected);
        self.push_public(start);
    }

    fn save_step(&mut self, command: Save, step: Step<'_>, approval: Option<FrameApproval>) {
        let save = command.confirmation;
        let at = save.time().recv_t_us;
        let admission = step.admission;
        let interval = approval.filter(|_| command.admission == admission && step.mode.geometry.id == command.geometry
            && save.monitor() == command.geometry.monitor)
            .map(|approval| Interval::approved(command.geometry, step.mode.capture_epoch, admission, &approval));
        let (trigger, role) = save_slot(save.kind());
        let state = match (save.kind(), interval) {
            (SaveKind::Checkpoint, Some(interval)) => {
                let image = still_time::latest(step.images, interval, at);
                State::Selected(self.retain(image.ok_or(Missing::NoEligibleFrame), still_png::Priority::Checkpoint))
            }
            (SaveKind::Final, Some(_)) => {
                self.quiet.invalidate(at);
                State::Settling
            }
            (_, None) => State::dropped(if command.admission != admission {
                DropReason::AdmissionClosed
            } else {
                DropReason::SaveIntervalChanged
            }),
        };
        self.push_public(Public { interval, save: Some(save), reservation: Some(command.reservation),
            ..Public::new(trigger, role, at, command.geometry, command.admission, state) });
    }

    fn rest(&mut self, rest: Rest, step: Step<'_>, approval: Option<FrameApproval>) {
        let admission = step.admission;
        let changed = if rest.admission != admission {
            Some(DropReason::AdmissionChanged)
        } else if rest.geometry != step.mode.geometry.id {
            Some(DropReason::GeometryChanged)
        } else {
            None
        };
        let (None, Some(approval)) = (changed, approval) else {
            let outcome = SlotOutcome::dropped(changed.unwrap_or(DropReason::FrameNotApproved));
            let slot = Public::new(Trigger::PointerRest, Role::Image, rest.at, rest.geometry, rest.admission,
                State::Done(outcome.clone()));
            rest.reservation.finish(slot.row(&outcome));
            return;
        };
        let interval = Interval::approved(rest.geometry, step.mode.capture_epoch, admission, &approval);
        let candidate = still_time::latest(step.images, interval, rest.at).ok_or(Missing::NoEligibleFrame);
        let selected = self.retain(candidate, still_png::Priority::Idle);
        self.push_public(Public { interval: Some(interval), pointer: rest.pointer, reservation: Some(rest.reservation),
            ..Public::new(Trigger::PointerRest, Role::Image, rest.at, rest.geometry, admission, State::Selected(selected)) });
    }

    pub fn changed(&mut self, frame: FrameMetadata) {
        let interval = Interval { geometry: frame.geometry.id, capture: frame.id.epoch,
            admission: frame.admission, approval: frame.approval_generation, elevated_visit: frame.elevated_visit() };
        self.observer.change = Some((interval, frame.source_t_us));
    }

    fn observe(&mut self, step: Step<'_>, approval: Option<FrameApproval>) {
        let now = step.now;
        let activity = SessionTimeUs(self.signals.last_activity.load(Ordering::Relaxed));
        if activity > self.observer.activity {
            self.observer.activity = activity;
            self.observer.at = SessionTimeUs(activity.0.saturating_add(OBSERVE_US));
        }
        if now < self.observer.at { return; }
        self.observer.at = SessionTimeUs(now.0.saturating_add(OBSERVE_US));
        let Some(approval) = approval else { return };
        let interval = step.interval(&approval);
        let Some(frame) = still_time::latest(step.images, interval, now) else { return };
        let changed = self.observer.change.is_some_and(|(scope, at)| scope == interval && now.0.saturating_sub(at.0) <= OBSERVE_US);
        if Some(frame.metadata.id) == self.observer.frame || !changed {
            return;
        }
        self.observer.frame = Some(frame.metadata.id);
        let selected = self.retain(Ok(frame), still_png::Priority::Idle);
        self.push_public(Public { interval: Some(interval),
            ..Public::new(Trigger::Observe, Role::Image, now, step.mode.geometry.id, step.admission, State::Selected(selected)) });
    }

    fn retain(&mut self, frame: Result<&NativeFrame, Missing>, priority: still_png::Priority) -> Selected {
        let frame = match frame { Ok(frame) => frame, Err(reason) => return Selected::Missing(reason) };
        loop {
            if let Some(held) = self.holds.retain(frame) {
                return Selected::Retained(held);
            }
            if !self.reclaim(priority, false) {
                return Selected::Capacity;
            }
        }
    }

    /// The first frame after `end`, retained at the next priority, which may
    /// release lower-priority holds (ALG §4, IMG-7).
    fn select_next(&mut self, images: &NativeImages, interval: Interval, end: SessionTimeUs) -> Selected {
        match still_time::next(images, interval, end) {
            Ok(None) => Selected::Waiting,
            Ok(Some(frame)) => self.retain(Ok(frame), still_png::Priority::Next),
            Err(reason) => Selected::Missing(reason),
        }
    }

    /// Release the holds of the lowest IMG-7 priority below `priority`; among
    /// equal priority the newest goes first (REL-2). Private pending
    /// selections take part; with `private_only` only they do. Preceding
    /// private selections have the highest priority and are never released.
    ///
    /// Slots that select the same frame share one hold, so releasing only
    /// some of them frees nothing (ALG §13.12). Outside `private_only` a frame
    /// is released only when every holder is a victim; a holder outside the
    /// victims, such as the frame a readback is being fitted for, keeps it.
    pub(super) fn reclaim(&mut self, priority: still_png::Priority, private_only: bool) -> bool {
        use still_png::Priority::{Next, Settled};
        enum Victim { Quiet, Public(usize), Scroll(usize), PrivateNext(usize), PrivateScroll(usize) }
        /// The victims holding one frame: their highest priority, newest
        /// receipt, the frame's holder count and the victims.
        struct Group { priority: still_png::Priority, at: SessionTimeUs, frame: *const HeldFrame, holders: usize, victims: Vec<Victim> }
        let mut groups: Vec<Group> = Vec::new();
        let mut add = |low: still_png::Priority, at: SessionTimeUs, frame: &Rc<HeldFrame>, victim: Victim| {
            if low >= priority { return; }
            let pointer = Rc::as_ptr(frame);
            match groups.iter_mut().find(|group| group.frame == pointer) {
                Some(group) => {
                    group.priority = group.priority.max(low);
                    group.at = group.at.max(at);
                    group.victims.push(victim);
                }
                None => groups.push(Group { priority: low, at, frame: pointer, holders: Rc::strong_count(frame), victims: vec![victim] }),
            }
        };
        if !private_only {
            for (index, slot) in self.public.iter().enumerate() {
                if let State::Selected(Selected::Retained(frame)) = &slot.state {
                    add(slot.priority(), slot.at, frame, Victim::Public(index));
                }
            }
            if let Some(quiet) = self.quiet.candidate() {
                add(Settled, quiet.since, &quiet.frame, Victim::Quiet);
            }
            for (index, scroll) in self.scrolls.iter().enumerate() {
                if let Some(frame) = scroll.next.retained() {
                    add(Next, scroll.end, frame, Victim::Scroll(index));
                }
            }
        }
        for (index, entry) in self.pending.iter().enumerate() {
            let at = entry.received();
            if let PrivateSelection::Next(selected) = &entry.selection && let Some(frame) = selected.retained() {
                add(Next, at, frame, Victim::PrivateNext(index));
            }
            if let Some(frame) = entry.scroll_next.retained() {
                add(Next, at, frame, Victim::PrivateScroll(index));
            }
        }
        let Some(group) = groups.into_iter().filter(|group| private_only || group.victims.len() == group.holders)
            .min_by_key(|group| (group.priority, std::cmp::Reverse(group.at))) else { return false };
        for victim in group.victims {
            match victim {
                Victim::Quiet => {
                    self.quiet.release();
                    self.drop_settling(DropReason::SettledRetentionPressure, |_| true);
                }
                Victim::Public(index) => self.public[index].state = State::dropped(DropReason::RetentionLowerPriority),
                Victim::Scroll(index) => self.scrolls[index].next = Selected::Capacity,
                Victim::PrivateNext(index) => self.pending[index].selection = PrivateSelection::Next(Selected::Capacity),
                Victim::PrivateScroll(index) => self.pending[index].scroll_next = Selected::Capacity,
            }
        }
        true
    }

    fn push_public(&mut self, mut slot: Public) {
        let Some(reservation) = slot.reservation.take()
            .or_else(|| self.rows.reserve_public(self.receipt, slot.fallback(DropReason::StillOwnerClosed))) else { return };
        if self.public.len() >= PUBLIC_SLOTS {
            let lowest = self.public.iter().enumerate()
                .filter(|(_, old)| old.priority() < slot.priority()
                    && matches!(old.state, State::Settling | State::Selected(_)))
                .min_by_key(|(index, old)| (old.priority(), std::cmp::Reverse(*index)))
                .map(|(index, _)| index);
            if let Some(mut evicted) = lowest.and_then(|index| self.public.remove(index)) {
                let row = evicted.row(&SlotOutcome::dropped(DropReason::LowerPriority));
                if let Some(ticket) = evicted.reservation.take() {
                    ticket.finish(row);
                }
            } else {
                reservation.finish(slot.row(&SlotOutcome::dropped(DropReason::StillJobCapacity)));
                return;
            }
        }
        slot.reservation = Some(reservation);
        self.public.push_back(slot);
    }

    pub fn advance_idle(&mut self, through: SessionTimeUs) {
        self.receipt = self.receipt.max(through);
        self.rows.advance(self.receipt);
    }

    /// End the unselected slots that `scope` selects with `slot_reason`, and
    /// every running scroll burst with `scroll_reason`; the quiet run restarts.
    fn abandon_unselected(&mut self, scope: impl Fn(&Public) -> bool, slot_reason: DropReason, scroll_reason: DropReason) {
        self.quiet.reset();
        for slot in &mut self.public {
            if scope(slot) {
                slot.state = State::dropped(slot_reason);
            }
        }
        for scroll in std::mem::take(&mut self.scrolls) {
            self.drop_scroll(&scroll, scroll_reason);
        }
    }

    pub fn geometry_changed(&mut self, geometry: GeometryId) {
        if let Some(mut start) = self.start.take() {
            start.state = State::dropped(DropReason::GeometryChangedBeforeStartFrame);
            self.push_public(start);
        }
        self.abandon_unselected(|slot| slot.geometry != geometry && slot.unselected(),
            DropReason::GeometryChangedBeforeSelection, DropReason::GeometryChangedBeforeScrollEnd);
        self.observer.reset();
    }

    pub fn finish_boundary(
        &mut self, prefix: AdmissionBoundary, cutoff: AdmissionBoundary,
        images: &NativeImages, mode: &MonitorCapture, stopping: bool, capture_available: bool,
    ) {
        let at_boundary = |stop, pause| DropReason::at_boundary(stopping, stop, pause);
        self.receipt = self.receipt.max(cutoff.time);
        if stopping {
            self.select_final(prefix, cutoff, images, mode, capture_available);
        }
        self.drop_queued_saves(at_boundary(DropReason::StopBeforeSaveSelection, DropReason::PauseBeforeSaveSelection));
        self.abandon_unselected(
            |slot| slot.unselected() || slot.settled_confirmed_t_us.is_some_and(|confirmed| confirmed >= cutoff.time),
            at_boundary(DropReason::StopBeforeSelection, DropReason::PauseBeforeSelection),
            at_boundary(DropReason::StopBeforeScrollEnd, DropReason::PauseBeforeScrollEnd),
        );
        if let Some(mut start) = self.start.take() {
            start.state = State::dropped(at_boundary(DropReason::StopBeforeStartFrame, DropReason::PauseBeforeStartFrame));
            self.push_public(start);
        }
        self.observer.reset();
        self.observer.at = SessionTimeUs(cutoff.time.0.saturating_add(OBSERVE_US));
    }

    /// The final still at Stop: the latest covered frame of the capture epoch
    /// before the admitted prefix.
    fn select_final(&mut self, prefix: AdmissionBoundary, cutoff: AdmissionBoundary, images: &NativeImages,
        mode: &MonitorCapture, capture_available: bool,
    ) {
        let latest = capture_available.then(|| images.history().iter().rev().find(|frame| {
            frame.metadata.covered()
                && frame.metadata.geometry.id == mode.geometry.id && frame.metadata.id.epoch == mode.capture_epoch
                && frame.metadata.admission == prefix.epoch && frame.metadata.source_t_us < prefix.time
                && frame.metadata.recv_t_us < prefix.time
        })).flatten();
        let state = latest.map_or_else(
            || State::dropped(DropReason::NoEligibleFinalFrameInCaptureEpoch),
            |frame| State::Selected(self.retain(Ok(frame), still_png::Priority::Final)),
        );
        self.push_public(Public::new(Trigger::Final, Role::Image, cutoff.time, mode.geometry.id, prefix.epoch, state));
    }

    /// End every queued confirmed save with `reason`.
    fn drop_queued_saves(&mut self, reason: DropReason) {
        while let Some(command) = self.deferred_save.take().or_else(|| self.saves.try_recv().ok()) {
            let save = command.confirmation;
            let (trigger, role) = save_slot(save.kind());
            let at = save.time().recv_t_us;
            self.push_public(Public { save: Some(save), reservation: Some(command.reservation),
                ..Public::new(trigger, role, at, command.geometry, command.admission, State::dropped(reason)) });
        }
    }

    pub fn finish_private_prefix(
        &mut self, cutoff: AdmissionBoundary, images: &NativeImages,
        config: &ResolvedCaptureConfig, terminal: bool,
    ) {
        // Deliver only already approved requests with a reserved input
        // reference. At a boundary we never advance a new quiet window.
        self.take_decided(|_| !terminal, |stills, entry, permission| {
            if permission.at <= cutoff.time {
                stills.publish_private(entry, permission, images, config);
            }
        });
        if terminal {
            self.deferred = None;
            for _ in 0..CHANNEL { if self.input.try_recv().is_err() { break; } }
        }
        self.abandon_unselected(Public::unselected,
            DropReason::AdmissionClosedBeforeSelection, DropReason::AdmissionClosedBeforeScrollEnd);
    }

    pub fn advance_boundary(&self, at: SessionTimeUs) {
        self.rows.advance(SessionTimeUs(at.0.saturating_add(1)));
    }

    pub fn drained(&self) -> bool {
        self.public.is_empty()
    }
}
