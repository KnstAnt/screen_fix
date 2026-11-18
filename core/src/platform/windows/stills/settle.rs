//! Settled stills: a settling slot takes the first frame of a run that
//! stayed quiet for `QUIET_US` after its trigger.

use std::rc::Rc;

use crate::{
    capture::observation::ProvisionalInputKind,
    protocol::{FrameId, SessionTimeUs},
    util::raise,
};

use super::super::{context::FrameApproval, native::{HeldFrame, NativeImages}, still_png, still_time::Interval};
use super::{
    SETTLED_US, Step, Stills,
    slot::{DropReason, Public, Selected, State},
};

const QUIET_US: i64 = 500_000;
const CHANGE_LIMIT: f64 = 0.002;

/// The quiet tracker of the settling slots.
#[derive(Default)]
pub(super) struct Quiet {
    /// Only frames after the latest invalidating action or scroll end can be
    /// quiet; none before one, or after a reset.
    after: Option<SessionTimeUs>,
    candidate: Option<Candidate>,
}

/// The first frame of the current quiet run, held for the settling slots.
pub(super) struct Candidate {
    interval: Interval,
    last: FrameId,
    pub(super) since: SessionTimeUs,
    pub(super) frame: Rc<HeldFrame>,
}

impl Quiet {
    /// An action or a scroll end at `at` restarts the quiet run after it.
    pub(super) fn invalidate(&mut self, at: SessionTimeUs) {
        raise(&mut self.after, at);
        self.candidate = None;
    }

    /// Forget the quiet run and its start.
    pub(super) fn reset(&mut self) {
        self.candidate = None;
        self.after = None;
    }

    /// Release the candidate's hold; the quiet run starts again.
    pub(super) fn release(&mut self) {
        self.candidate = None;
    }

    pub(super) fn candidate(&self) -> Option<&Candidate> {
        self.candidate.as_ref()
    }
}

impl Stills {
    pub(super) fn settle_step(&mut self, step: Step<'_>, approval: Option<FrameApproval>,
        input_before: Option<SessionTimeUs>,
    ) {
        let current = approval.map(|approval| step.interval(&approval));
        self.expire_settling(current, step.now);
        let Some(interval) = current else {
            self.quiet.release();
            return;
        };
        if !self.public.iter().any(|slot| matches!(slot.state, State::Settling) && slot.interval == current) {
            self.quiet.reset();
            return;
        }
        let Some(after) = self.quiet.after else { return };
        if self.track_quiet(step.images, interval, after) {
            self.confirm_quiet(interval, step.now, input_before);
        }
    }

    /// End the settling slots that cannot be confirmed in `current`.
    fn expire_settling(&mut self, current: Option<Interval>, now: SessionTimeUs) {
        for slot in &mut self.public {
            if !matches!(slot.state, State::Settling) { continue; }
            if slot.interval.is_none() {
                slot.state = State::dropped(DropReason::NoApprovedSettledInterval);
            } else if now.0.saturating_sub(slot.at.0) >= SETTLED_US {
                slot.state = State::dropped(DropReason::SettledDeadline);
            } else if current.is_some() && slot.interval != current {
                slot.state = State::dropped(DropReason::CaptureIntervalChanged);
            }
        }
    }

    /// Follow the quiet candidate of `interval` through the frames after
    /// `after`: a run of consecutive frames each changing less than
    /// `CHANGE_LIMIT`. False while a frame still awaits its change score.
    fn track_quiet(&mut self, images: &NativeImages, interval: Interval, after: SessionTimeUs) -> bool {
        for frame in images.history().iter().filter(|frame| interval.contains(&frame.metadata) && frame.metadata.source_t_us > after) {
            let meta = &frame.metadata;
            if let Some(quiet) = &mut self.quiet.candidate {
                if quiet.interval == interval && meta.id.sequence.0 <= quiet.last.sequence.0 {
                    continue;
                }
                if !frame.change_sampled {
                    return false;
                }
                if quiet.interval == interval && quiet.last.sequence.0.checked_add(1) == Some(meta.id.sequence.0)
                    && frame.change.is_some_and(|change| change < CHANGE_LIMIT)
                {
                    quiet.last = meta.id;
                    continue;
                }
            }
            // Release the previous candidate before claiming another distinct hold.
            self.quiet.release();
            let Selected::Retained(candidate) = self.retain(Ok(frame), still_png::Priority::Settled) else {
                self.drop_settling(DropReason::SettledRetentionPressure, |slot| slot.interval == Some(interval));
                return true;
            };
            self.quiet.candidate = Some(Candidate { interval, last: meta.id, since: meta.source_t_us, frame: candidate });
        }
        true
    }

    /// Select the quiet candidate for the settling slots of `interval` once
    /// it stayed quiet for `QUIET_US`.
    fn confirm_quiet(&mut self, interval: Interval, now: SessionTimeUs, input_before: Option<SessionTimeUs>) {
        let Some(quiet) = &self.quiet.candidate else { return };
        let current = Some(interval);
        let eligible_at = SessionTimeUs(quiet.since.0.saturating_add(QUIET_US));
        // Wait for the existing input certification prefix and any
        // in-owner handoff which can still invalidate this candidate.
        let unresolved_action = self.pending.iter().any(|entry| {
            entry.interval == current && entry.received() <= now
                && entry.opening.request.kind != ProvisionalInputKind::PreMotion
        });
        if now < eligible_at || input_before.is_none_or(|before| before <= eligible_at) || unresolved_action {
            return;
        }
        for slot in &mut self.public {
            if slot.interval == current && matches!(slot.state, State::Settling) {
                slot.settled_confirmed_t_us = Some(now);
                slot.state = State::Selected(Selected::Retained(Rc::clone(&quiet.frame)));
            }
        }
        self.quiet.reset();
    }

    /// End the settling slots that `scope` selects with `reason`.
    pub(super) fn drop_settling(&mut self, reason: DropReason, scope: impl Fn(&Public) -> bool) {
        for slot in &mut self.public {
            if matches!(slot.state, State::Settling) && scope(slot) {
                slot.state = State::dropped(reason);
            }
        }
    }
}
