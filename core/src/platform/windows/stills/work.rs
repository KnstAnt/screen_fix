//! The shared readback and PNG work of the held frames that public slots
//! selected, and the hand-off of each resolved slot's row.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use crate::protocol::{FrameId, SessionTimeUs};

use super::super::{ProbeError, native::NativeImages, readback, still_png, still_rows, still_time::Missing};
use super::{
    NEXT_US, Stills,
    slot::{DropReason, Public, Role, Selected, SlotOutcome, State},
};

const READBACK_WAIT: Duration = Duration::from_secs(3);

/// The readback and PNG job of one held frame, shared by every slot that
/// selected it (C2). Bytes are charged once per frame. The last attached slot
/// releases the work; a slot resolved while attached only detaches, and
/// dropping an unfinished PNG ticket cancels its job.
pub(super) struct FrameWork {
    frame: FrameId,
    /// The PNG job takes the highest priority, the rate exemption and the
    /// least restrictive coverage among the slots attached before it starts.
    /// Each row is still checked against its own coverage.
    priority: still_png::Priority,
    exempt: bool,
    coverage: Option<still_rows::StillCoverage>,
    stage: WorkStage,
}

enum WorkStage {
    Readback(readback::Ticket),
    Encoding(still_png::Ticket),
    /// The slot outcome; `withdrawn` clears each slot's frame evidence.
    Done { outcome: SlotOutcome, withdrawn: bool },
}

impl FrameWork {
    /// Advance the work. Without the PNG inbox only a finished job is collected.
    fn advance(&mut self, png: Option<&still_png::Inbox>) {
        let done = |outcome: SlotOutcome, withdrawn: bool| WorkStage::Done { outcome, withdrawn };
        let png_dropped = |reason: still_png::Dropped| SlotOutcome::dropped(DropReason::Png(reason));
        self.stage = match &mut self.stage {
            WorkStage::Readback(ticket) => {
                let Some(png) = png else { return };
                let Some(result) = ticket.try_result() else { return };
                match result {
                    Ok(pixels) if pixels.metadata.covered() => match png.request(pixels, self.priority, self.exempt, self.coverage.clone()) {
                        Ok(ticket) => WorkStage::Encoding(ticket),
                        Err(reason) => done(png_dropped(reason), false),
                    },
                    Ok(_) => done(SlotOutcome::dropped(DropReason::ElevatedFrameUncertified), true),
                    Err(reason) => done(SlotOutcome::dropped(reason.into()), false),
                }
            }
            WorkStage::Encoding(ticket) => match ticket.poll() {
                None => return,
                Some(still_png::Outcome::Saved { reference, reused }) => done(SlotOutcome::Saved {
                    image: reference.journal_file(), deduplicated: reused,
                }, false),
                Some(still_png::Outcome::Dropped(reason)) => done(png_dropped(reason),
                    matches!(reason, still_png::Dropped::ApprovalSuppressed)),
                Some(still_png::Outcome::Failed { operation, native_code }) => {
                    done(SlotOutcome::Failed { operation, native_code }, false)
                }
            },
            WorkStage::Done { .. } => return,
        };
    }

    fn outcome(&self) -> Option<(SlotOutcome, bool)> {
        match &self.stage {
            WorkStage::Done { outcome, withdrawn } => Some((outcome.clone(), *withdrawn)),
            _ => None,
        }
    }
}

/// Attach a slot that retained the work's frame: its frame evidence is
/// recorded and its hold is released, because the readback copy is submitted.
fn attach(slot: &mut Public, work: &Rc<RefCell<FrameWork>>) {
    let State::Selected(Selected::Retained(frame)) = &slot.state else { return };
    let frame = Rc::clone(frame);
    slot.record_frame(&frame.metadata);
    let mut shared = work.borrow_mut();
    shared.priority = shared.priority.max(slot.priority());
    shared.exempt |= matches!(slot.role, Role::Before);
    if let Some(coverage) = slot.coverage.as_ref()
        && shared.coverage.as_ref().is_none_or(|old| coverage.last < old.last)
    {
        shared.coverage = Some(coverage.clone());
    }
    drop(shared);
    slot.state = State::Work(Rc::clone(work));
}

impl Stills {
    pub fn expire_native_epoch(&mut self) {
        for row in &mut self.public {
            let reading = matches!(&row.state, State::Work(work) if matches!(work.borrow().stage, WorkStage::Readback(_)));
            if reading || matches!(row.state, State::Selected(Selected::Retained(_))) {
                row.state = State::dropped(DropReason::NativeCaptureEpochClosed);
            }
        }
    }

    /// Resolve every open slot at the drain deadline. A PNG job that already
    /// finished keeps its result; dropping an unfinished ticket cancels the job
    /// before its write or publication.
    pub fn expire_drain(&mut self, stopping: bool) {
        let reason = DropReason::at_boundary(stopping, DropReason::StopDrainDeadline, DropReason::PauseDrainDeadline);
        for slot in &mut self.public {
            let finished = match &slot.state {
                State::Done(_) => continue,
                State::Work(work) => {
                    let mut work = work.borrow_mut();
                    work.advance(None);
                    work.outcome()
                }
                _ => None,
            };
            if finished.as_ref().is_some_and(|(_, withdrawn)| *withdrawn) {
                slot.withdraw_frame();
            }
            slot.state = State::Done(finished.map_or_else(|| SlotOutcome::dropped(reason), |(outcome, _)| outcome));
        }
    }

    pub fn work(
        &mut self, device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        images: &NativeImages, readbacks: &mut readback::Readbacks, png: &still_png::Inbox,
        now: SessionTimeUs, ticking: Instant,
    ) -> Result<(), ProbeError> {
        self.select_waiting(images, now);
        self.advance_slots(png);
        self.attach_shared_work();
        self.start_readback(device, images, readbacks, ticking)?;
        self.finish_rows();
        Ok(())
    }

    /// Select the next frame of each waiting public slot, up to `NEXT_US`
    /// after its trigger.
    fn select_waiting(&mut self, images: &NativeImages, now: SessionTimeUs) {
        for index in 0..self.public.len() {
            let slot = &self.public[index];
            let (Some(interval), State::Selected(Selected::Waiting)) = (slot.interval, &slot.state) else { continue };
            let at = slot.at;
            let selected = if now.0.saturating_sub(at.0) >= NEXT_US {
                Selected::Missing(Missing::NoEligibleFrame)
            } else {
                self.select_next(images, interval, at)
            };
            self.public[index].state = State::Selected(selected);
        }
    }

    /// Resolve slots without a usable frame and collect finished work.
    fn advance_slots(&mut self, png: &still_png::Inbox) {
        for slot in &mut self.public {
            match &mut slot.state {
                State::Selected(Selected::Retained(frame)) if !frame.metadata.covered() => {
                    slot.state = State::dropped(DropReason::ElevatedFrameUncertified);
                }
                State::Selected(Selected::Capacity) => slot.state = State::dropped(DropReason::RetentionCapacity),
                State::Selected(Selected::Missing(reason)) => {
                    slot.state = State::Done(SlotOutcome::Unavailable { reason: *reason });
                }
                State::Work(work) => {
                    let work = Rc::clone(work);
                    let mut work = work.borrow_mut();
                    work.advance(Some(png));
                    if let Some((outcome, withdrawn)) = work.outcome() {
                        if withdrawn { slot.withdraw_frame(); }
                        slot.state = State::Done(outcome);
                    }
                }
                _ => {}
            }
        }
    }

    /// A slot whose held frame already has shared work attaches to it.
    fn attach_shared_work(&mut self) {
        for index in 0..self.public.len() {
            let State::Selected(Selected::Retained(frame)) = &self.public[index].state else { continue };
            let id = frame.metadata.id;
            let work = self.public.iter().find_map(|slot| match &slot.state {
                State::Work(work) if work.borrow().frame == id => Some(Rc::clone(work)),
                _ => None,
            });
            if let Some(work) = work {
                attach(&mut self.public[index], &work);
            }
        }
    }

    /// Start the readback of the highest-priority held frame when no readback
    /// is pending. One readback and one PNG job serve every slot of the frame (C2).
    fn start_readback(&mut self, device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        images: &NativeImages, readbacks: &mut readback::Readbacks, ticking: Instant,
    ) -> Result<(), ProbeError> {
        if readbacks.has_pending() {
            return Ok(());
        }
        let ready = self.public.iter().enumerate()
            .filter(|(_, slot)| matches!(slot.state, State::Selected(Selected::Retained(_))))
            .min_by_key(|(index, slot)| (std::cmp::Reverse(slot.priority()), *index));
        let Some((index, slot)) = ready else { return Ok(()) };
        let State::Selected(Selected::Retained(frame)) = &slot.state else { return Ok(()) };
        // This handle keeps the frame's own hold out of `reclaim`.
        let frame = Rc::clone(frame);
        let priority = slot.priority();
        while !readbacks.fits(&frame) && self.reclaim(priority, false) {}
        let meta = &frame.metadata;
        let request = readback::Request { frame: meta.id,
            geometry: meta.geometry.id, admission: meta.admission, deadline: ticking + READBACK_WAIT };
        match readbacks.try_retained(request, &frame, device, images)? {
            Some(Ok(ticket)) => {
                let work = Rc::new(RefCell::new(FrameWork { frame: meta.id, priority, exempt: false, coverage: None,
                    stage: WorkStage::Readback(ticket) }));
                for slot in &mut self.public {
                    if matches!(&slot.state, State::Selected(Selected::Retained(held)) if held.metadata.id == meta.id) {
                        attach(slot, &work);
                    }
                }
            }
            Some(Err(reason)) => {
                let slot = &mut self.public[index];
                slot.record_frame(meta);
                slot.state = State::dropped(reason.into());
            }
            None => {}
        }
        Ok(())
    }

    /// Hand each resolved slot's row to its reservation.
    fn finish_rows(&mut self) {
        self.public.retain_mut(|slot| {
            let State::Done(outcome) = &slot.state else { return true };
            let row = slot.row(outcome);
            if let Some(ticket) = slot.reservation.take() {
                ticket.finish_frame(row, slot.coverage.take());
            }
            false
        });
    }
}
