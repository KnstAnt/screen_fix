use std::collections::VecDeque;

use serde_json::{Value, json};
use screen_fix_native_observation::LossSpan;

use crate::{
    config::{ClipboardMode, ResolvedCaptureConfig},
    package::outcome::LossReason,
    platform::windows::controller::Receipt,
    protocol::{ClockOrigin, CoarseTime, FocusGeneration, QpcTick},
};

use super::{Losses, Visit, uia::FocusGuard};
use super::super::{ProbeError, clipboard, clock};

/// Approved input stamps kept to attribute an ownerless clipboard change.
const APPROVED_ACTIVITY_LIMIT: usize = 64;
/// Approved input attributes an ownerless clipboard change at most this long after it.
const CLIPBOARD_ACTIVITY_US: i64 = 1_000_000;
const CLIPBOARD_STAGING_LIMIT: usize = 8;

/// The opt-in clipboard owner, its notices waiting for a read grant, its
/// reads completed after the current poll and the approved input that can
/// attribute an ownerless change.
pub(super) struct ClipboardStaging {
    worker: Option<clipboard::Worker>,
    notices: VecDeque<clipboard::Notice>,
    future_reads: Vec<clipboard::Read>,
    activity: VecDeque<(FocusGeneration, QpcTick)>,
}

impl ClipboardStaging {
    pub fn start(config: &ResolvedCaptureConfig) -> Result<Self, ProbeError> {
        let worker = (config.profile().clipboard() == ClipboardMode::OptIn)
            .then(clipboard::Worker::start).transpose()?;
        Ok(Self {
            worker,
            notices: VecDeque::with_capacity(CLIPBOARD_STAGING_LIMIT),
            future_reads: Vec::with_capacity(CLIPBOARD_STAGING_LIMIT),
            activity: VecDeque::with_capacity(APPROVED_ACTIVITY_LIMIT),
        })
    }

    /// Drop the staged notices, reads and input stamps at a pause. The owner keeps running.
    pub fn reset(&mut self) {
        self.notices.clear();
        self.future_reads.clear();
        self.activity.clear();
    }

    pub fn worker(&self) -> Option<&clipboard::Worker> {
        self.worker.as_ref()
    }

    /// Whether the owner exited since the last call (§6.2 row 17). It is
    /// dropped with its staged notices and never replaced, so no later
    /// clipboard events are produced.
    pub fn take_exited(&mut self) -> bool {
        if !self.worker.as_ref().is_some_and(clipboard::Worker::exited) {
            return false;
        }
        self.worker = None;
        self.notices.clear();
        true
    }

    /// Discard the owner's pending notices; returns how many there were.
    pub fn discard_notices(&self) -> u64 {
        self.worker.as_ref().map_or(0, |worker| worker.notices().count() as u64)
    }

    /// Remember approved input of visit `generation` at `at`, while the owner runs.
    pub fn note_activity(&mut self, generation: FocusGeneration, at: QpcTick) {
        if self.worker.is_none() {
            return;
        }
        if self.activity.len() == APPROVED_ACTIVITY_LIMIT {
            self.activity.pop_front();
        }
        self.activity.push_back((generation, at));
    }

    /// Queue the owner's notices, at most `CLIPBOARD_STAGING_LIMIT`; every
    /// notice the owner or this queue could not hold is a `clipboard_queue`
    /// loss (C3). Returns false without an owner.
    pub fn stage_notices(&mut self, losses: &mut Losses) -> bool {
        let Some(worker) = &self.worker else { return false; };
        if let Some(span) = worker.take_loss() {
            losses.note(LossReason::ClipboardQueue, span);
        }
        for notice in worker.notices() {
            if self.notices.len() < CLIPBOARD_STAGING_LIMIT {
                self.notices.push_back(notice);
            } else {
                losses.note(LossReason::ClipboardQueue, LossSpan::at(notice.received));
            }
        }
        true
    }

    pub fn retain_notices(&mut self, keep: impl FnMut(&clipboard::Notice) -> bool) {
        self.notices.retain(keep);
    }

    /// Grant a read for each staged notice of the `safe` ordinary visit
    /// through `now`; without a safe visit, keep only the notices of the
    /// `current` visit. Returns the rows of the reads completed through `now`
    /// for the safe visit.
    pub fn poll(
        &mut self, safe: Option<Visit>, current: Option<Visit>, guard: impl Fn() -> FocusGuard, receipt: Receipt,
        now: QpcTick, losses: &mut Losses,
    ) -> Vec<Value> {
        let Some(worker) = &self.worker else { return Vec::new() };
        if let Some(visit) = safe {
            let mut pending = VecDeque::with_capacity(CLIPBOARD_STAGING_LIMIT);
            for notice in self.notices.drain(..) {
                if notice.observed.elapsed() >= clipboard::MAX_AGE || notice.foreground != visit.foreground
                    || notice.received < visit.started_at || notice.received < receipt.boundary.qpc
                {
                    continue;
                }
                if notice.received > now {
                    pending.push_back(notice);
                    continue;
                }
                if !change_after_visit(notice, visit, receipt) {
                    continue;
                }
                if notice.owner.is_none() && !recent_activity(&self.activity, notice, visit, receipt.clock) {
                    pending.push_back(notice);
                    continue;
                }
                if !worker.grant(notice, visit, guard()) {
                    losses.note(LossReason::ClipboardQueue, LossSpan::at(notice.received));
                }
            }
            self.notices = pending;
        } else {
            self.notices.retain(|notice| notice.observed.elapsed() < clipboard::MAX_AGE
                && current.is_some_and(|visit| visit.foreground == notice.foreground
                    && notice.received >= visit.started_at));
        }
        let mut output = Vec::new();
        for read in self.due_reads(now, losses) {
            if safe.is_some_and(|visit| visit.generation == read.visit.generation
                && visit.foreground == read.notice.foreground)
            {
                output.push(clipboard_row(read, receipt.clock));
            }
        }
        output
    }

    /// The staged and newly completed reads completed through `now`. A later
    /// read stays staged, at most `CLIPBOARD_STAGING_LIMIT`; every other one
    /// is a `clipboard_queue` loss (C3).
    pub fn due_reads(&mut self, now: QpcTick, losses: &mut Losses) -> Vec<clipboard::Read> {
        let Some(worker) = &self.worker else { return Vec::new() };
        let mut reads = std::mem::take(&mut self.future_reads);
        reads.extend(worker.results());
        let mut due = Vec::with_capacity(reads.len());
        for read in reads {
            if read.completed <= now {
                due.push(read);
            } else if self.future_reads.len() < CLIPBOARD_STAGING_LIMIT {
                self.future_reads.push(read);
            } else {
                losses.note(LossReason::ClipboardQueue, LossSpan::at(read.completed));
            }
        }
        due
    }

    /// Take the latest notice through `at` that an elevated `visit` may read:
    /// changed after the visit started, attributable to an owner or approved
    /// input, and still current.
    pub fn take_eligible(&mut self, at: QpcTick, visit: Visit, receipt: Receipt) -> Option<clipboard::Notice> {
        let eligible = self.notices.iter().rposition(|notice| {
            notice.received <= at && change_after_visit(*notice, visit, receipt)
                && (notice.owner.is_some() || recent_activity(&self.activity, *notice, visit, receipt.clock))
                && clipboard::notice_current(*notice, visit)
        })?;
        self.notices.remove(eligible)
    }
}

/// Whether approved input of `visit` shortly before `notice` attributes its change.
fn recent_activity(
    activity: &VecDeque<(FocusGeneration, QpcTick)>, notice: clipboard::Notice, visit: Visit, clock: ClockOrigin,
) -> bool {
    let received = clock.time(notice.received).ok();
    activity.iter().rev().any(|(generation, at)| {
        *generation == visit.generation && *at <= notice.received
            && received.zip(clock.time(*at).ok())
                .is_some_and(|(change, input)| change.0.saturating_sub(input.0) <= CLIPBOARD_ACTIVITY_US)
    })
}

fn change_after_visit(notice: clipboard::Notice, visit: Visit, receipt: Receipt) -> bool {
    let Ok(started) = receipt.clock.time(visit.started_at) else { return false };
    clock::message_tick_estimate(receipt.clock, receipt.tick_anchor, CoarseTime::windows_message(notice.source_tick))
        .is_some_and(|estimate| estimate.not_before(started))
}

pub(super) fn clipboard_row(read: clipboard::Read, clock: ClockOrigin) -> Value {
    json!({
        "kind": "clipboard_change", "approved": true,
        "reason": read.text.is_none().then_some("too_large"), "text": read.text,
        "sequence": read.notice.sequence, "owner_verified": read.notice.owner.is_some(),
        "notification_receipt_t_us": clock.time(read.notice.received).ok(),
        "completed_t_us": clock.time(read.completed).ok(),
    })
}
