//! Scroll bursts (ALG §8.3): approved scroll ticks of one device and axis
//! join one burst, which ends `SCROLL_US` after its last tick with a next and
//! a settled still.

use crate::{
    capture::{input::WheelAxis, observation::ProvisionalInputKind},
    protocol::{DeviceId, EventRef, Evidence, SessionTimeUs},
};

use super::super::{native::NativeImages, still_time::Interval};
use super::{
    NEXT_US, Stills,
    private::{Permission, Private},
    slot::{DropReason, Public, Role, Selected, SlotOutcome, State, Trigger},
};

/// The quiet time after a scroll tick that ends its burst.
pub(super) const SCROLL_US: i64 = 300_000;
pub(super) const MAX_SCROLLS: usize = 32;

pub(super) struct Scroll {
    device: Option<DeviceId>,
    axis: WheelAxis,
    interval: Interval,
    pub(super) end: SessionTimeUs,
    event: EventRef,
    pub(super) next: Selected,
}

impl Scroll {
    /// The next and settled slots of this burst, in `states` order.
    fn observations(&self, states: [State; 2]) -> [Public; 2] {
        let device = Evidence::from_option(self.device, crate::protocol::UnavailableReason::Ambiguous);
        let [next, settled] = states;
        [(Trigger::Next, next), (Trigger::Settled, settled)].map(|(trigger, state)| Public {
            event: Some(self.event), interval: Some(self.interval), scroll_device: Some(device),
            ..Public::new(trigger, Role::Image, self.end, self.interval.geometry, self.interval.admission, state)
        })
    }

    fn dropped_observations(&self, reason: DropReason) -> [Public; 2] {
        self.observations([State::dropped(reason), State::dropped(reason)])
    }
}

impl Stills {
    pub(super) fn scroll_step(&mut self, images: &NativeImages, now: SessionTimeUs, input_before: Option<SessionTimeUs>) {
        let mut index = 0;
        while index < self.scrolls.len() {
            let burst = &self.scrolls[index];
            let newer_pending = self.pending.iter().any(|entry| {
                let request = entry.opening.request;
                request.kind == (ProvisionalInputKind::Scroll { axis: burst.axis })
                    && entry.interval == Some(burst.interval) && request.device == burst.device
                    && request.time.recv_t_us < burst.end
                    && request.time.recv_t_us.0 > burst.end.0.saturating_sub(SCROLL_US)

            });
            let pending_prefix = input_before.is_none_or(|before| before <= burst.end);
            let can_wait = now.0.saturating_sub(burst.end.0) < NEXT_US;
            if burst.end > now || can_wait && (newer_pending || pending_prefix) {
                if burst.end <= now && matches!(burst.next, Selected::Waiting) {
                    let (interval, end) = (burst.interval, burst.end);
                    let next = self.select_next(images, interval, end);
                    self.scrolls[index].next = next;
                }
                index += 1;
                continue;
            }
            let scroll = self.scrolls.remove(index);
            if pending_prefix || newer_pending {
                self.drop_scroll(&scroll, DropReason::ScrollEndUncertified);
            } else {
                self.finish_scroll(scroll, images);
            }
        }
    }

    pub(super) fn drop_scroll(&mut self, scroll: &Scroll, reason: DropReason) {
        for slot in scroll.dropped_observations(reason) {
            self.push_public(slot);
        }
    }

    fn finish_scroll(&mut self, mut scroll: Scroll, images: &NativeImages) {
        let next = match std::mem::replace(&mut scroll.next, Selected::Waiting) {
            Selected::Waiting => self.select_next(images, scroll.interval, scroll.end),
            selected => selected,
        };
        self.quiet.invalidate(scroll.end);
        for slot in scroll.observations([State::Selected(next), State::Settling]) {
            self.push_public(slot);
        }
    }

    /// Join an approved scroll tick to its burst (ALG §8.3). False when the
    /// tick only extends a running burst, whose slots stand for it.
    pub(super) fn stage_scroll(&mut self, entry: &mut Private, permission: &Permission, axis: WheelAxis,
        interval: Interval, images: &NativeImages,
    ) -> bool {
        let at = permission.at;
        // Ticks without a device form one unknown-device burst per monitor and axis;
        // known devices are never merged with each other (ALG §8.3).
        let existing = self.scrolls.iter().position(|scroll| {
            scroll.interval == interval && scroll.device == permission.device && scroll.axis == axis
        });
        let end = SessionTimeUs(at.0.saturating_add(SCROLL_US));
        let next = if at == entry.received() {
            std::mem::replace(&mut entry.scroll_next, Selected::Waiting)
        } else {
            Selected::Waiting
        };
        let burst = Scroll { device: permission.device, axis, interval, end, event: permission.event, next };
        if let Some(index) = existing {
            let old = std::mem::replace(&mut self.scrolls[index], burst);
            if at < old.end {
                self.quiet.invalidate(at);
                return false;
            }
            self.finish_scroll(old, images);
        } else if self.scrolls.len() < MAX_SCROLLS {
            self.scrolls.push(burst);
        } else {
            // Terminal rows need journal reservations, but no still job slots.
            let reason = DropReason::ScrollBurstCapacity;
            for slot in burst.dropped_observations(reason) {
                if let Some(reservation) = self.rows.reserve_public(self.receipt, slot.fallback(reason)) {
                    reservation.finish(slot.row(&SlotOutcome::dropped(reason)));
                }
            }
        }
        true
    }
}
