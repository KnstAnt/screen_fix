//! Private openings: the frames selected at an input's receipt, held until
//! its action is approved or abandoned.

use std::{
    rc::Rc,
    sync::mpsc::{Receiver, TryRecvError},
};

use crate::{
    capture::observation::{ProvisionalInputKind, ProvisionalInputObservation},
    config::{DeviceCalibration, ResolvedCaptureConfig},
    package::outcome::LossReason,
    protocol::{
        AdmissionEpoch, DeviceId, EventRef, Evidence, GeometryId, SessionTimeUs,
        ipc::{ForegroundAtReceipt, elevated::VisitBinding},
    },
};

use super::super::{
    context::FrameApproval,
    native::{HeldFrame, NativeFrame, NativeImages, PRIVATE_HOLDS},
    still_png, still_rows,
    still_time::{self, Adjustment, Interval, Missing},
};
use super::{
    Step, Stills,
    scroll::SCROLL_US,
    slot::{DropReason, Public, Role, Selected, State, Trigger, fallback_row},
};

/// How long a pending entry waits for its action's decision.
const PRIVATE_US: i64 = 2_000_000;

pub(in crate::platform::windows) struct Permission {
    pub event: EventRef,
    pub geometry: GeometryId,
    pub proof: ForegroundAtReceipt,
    pub device: Option<DeviceId>,
    pub at: SessionTimeUs,
    pub rows: EarlyRows,
}

/// The still rows reserved for one action: a pre-motion or pre-action trigger
/// reserves one row whose before and seen halves are `rows` (IMG-2); a next
/// trigger reserves its next and settled rows.
pub(in crate::platform::windows) struct EarlyRows {
    lane: still_rows::Rows,
    paired: bool,
    rows: [EarlyRow; 2],
}

/// The journal rows an action of this kind reserves.
pub(in crate::platform::windows) fn trigger_rows(kind: ProvisionalInputKind) -> usize {
    if Trigger::from(kind).paired() { 1 } else { 2 }
}

/// A private still-row reservation stays uncounted until its action is approved.
enum EarlyRow {
    Reserved(still_rows::Reservation),
    Unavailable(LossReason),
    HandedOff,
}

impl EarlyRow {
    fn reserve(lane: &still_rows::Rows, at: SessionTimeUs) -> Self {
        match lane.reserve(at, None) {
            Ok(reservation) => Self::Reserved(reservation),
            Err(reason) => Self::Unavailable(reason),
        }
    }
}

impl EarlyRows {
    pub fn reserve(lane: &still_rows::Rows, at: SessionTimeUs, kind: ProvisionalInputKind) -> Self {
        let paired = Trigger::from(kind).paired();
        let rows = if paired {
            match lane.reserve_pair(at) {
                Ok([before, seen]) => [EarlyRow::Reserved(before), EarlyRow::Reserved(seen)],
                Err(reason) => [EarlyRow::Unavailable(reason), EarlyRow::Unavailable(reason)],
            }
        } else {
            [EarlyRow::reserve(lane, at), EarlyRow::reserve(lane, at)]
        };
        Self { lane: lane.clone(), paired, rows }
    }

    pub fn approve(&mut self, kind: ProvisionalInputKind, event: EventRef, geometry: Evidence<GeometryId>,
        admission: AdmissionEpoch, at: SessionTimeUs, missing: DropReason,
    ) {
        let trigger = Trigger::from(kind);
        let roles = if trigger.paired() {
            [(trigger, Role::Before); 2]
        } else {
            [(Trigger::Next, Role::Image), (Trigger::Settled, Role::Image)]
        };
        // Both halves of a paired row share its reservation and fallback.
        for (index, (trigger, role)) in roles.into_iter().enumerate().take(if self.paired { 1 } else { 2 }) {
            let fallback = fallback_row(trigger, role, at, geometry, admission, Some(event), missing);
            let lost = match &self.rows[index] {
                // The ordered still deadline expired the slot before approval.
                EarlyRow::Reserved(row) => (!row.approve(fallback)).then_some(LossReason::ProducerStall),
                EarlyRow::Unavailable(reason) => Some(*reason),
                EarlyRow::HandedOff => None,
            };
            if let Some(reason) = lost {
                self.rows[index] = EarlyRow::Unavailable(reason);
                if self.paired {
                    self.rows[1] = EarlyRow::Unavailable(reason);
                }
                self.lane.note_loss(at, reason);
            }
        }
    }

    fn take(&mut self, index: usize) -> Option<still_rows::Reservation> {
        match std::mem::replace(&mut self.rows[index], EarlyRow::HandedOff) {
            EarlyRow::Reserved(reservation) => Some(reservation),
            other => {
                self.rows[index] = other;
                None
            }
        }
    }

    fn suppress(self) {
        for row in self.rows {
            if let EarlyRow::Reserved(reservation) = row {
                reservation.suppress();
            }
        }
    }
}

pub(super) struct Opening {
    pub(super) request: ProvisionalInputObservation,
    pub(super) visit: Option<VisitBinding>,
    pub(super) decision: Receiver<Permission>,
}

pub(super) enum PrivateSelection {
    Preceding { adjustment: Adjustment, before: Selected, seen: Selected },
    Next(Selected),
    /// No still can be selected for the approved action, with the reason.
    Unavailable(DropReason),
}

/// One pending opening and the frames selected for it.
pub(super) struct Private {
    pub(super) opening: Opening,
    pub(super) interval: Option<Interval>,
    pub(super) selection: PrivateSelection,
    pub(super) scroll_next: Selected,
    refresh: (u32, u32),
}

impl Private {
    /// The frames this pending entry holds.
    fn held(&self) -> impl Iterator<Item = &Rc<HeldFrame>> {
        let selection = match &self.selection {
            PrivateSelection::Preceding { before, seen, .. } => [Some(before), Some(seen)],
            PrivateSelection::Next(selected) => [Some(selected), None],
            PrivateSelection::Unavailable(_) => [None, None],
        };
        selection.into_iter().flatten().chain([&self.scroll_next]).filter_map(Selected::retained)
    }

    /// The receipt of this entry's opening.
    pub(super) fn received(&self) -> SessionTimeUs {
        self.opening.request.time.recv_t_us
    }
}

impl Stills {
    /// Select the frames of one opening at its receipt and keep it pending
    /// until its action is approved or abandoned.
    pub(super) fn open_private(&mut self, opening: Opening, step: Step<'_>, approval: Option<FrameApproval>) {
        let request = opening.request;
        let at = request.time.recv_t_us;
        let mode = step.mode;
        let interval = approval.filter(|_| request.geometry == Evidence::Observed(mode.geometry.id)
            && request.admission == step.admission).map(|approval| step.interval(&approval));
        // An unread refresh rate selects no seen boundary (`UnknownRefresh`).
        let refresh = mode.refresh_numerator.zip(mode.refresh_denominator).map_or((0, 0), |(numerator, denominator)| (numerator, denominator.get()));
        // The Receiver opens a pre-motion envelope only for the first
        // motion after the IMG-2 idle period.
        let selection = if let Some(interval) = interval {
            match request.kind {
                ProvisionalInputKind::PreMotion | ProvisionalInputKind::PreAction | ProvisionalInputKind::Scroll { .. } => {
                    match Adjustment::from_receipt(at, calibration(step.config, request.device, interval)) {
                        Ok(adjustment) => {
                            let pair = still_time::preceding(step.images, interval, adjustment, refresh);
                            let before = self.retain_private(pair.before, still_png::Priority::BeforeSeen, None);
                            let seen = self.retain_private(pair.seen, still_png::Priority::BeforeSeen, Some(&before));
                            PrivateSelection::Preceding { adjustment, before, seen }
                        }
                        Err(_) => PrivateSelection::Unavailable(DropReason::AdjustmentFailed),
                    }
                }
                ProvisionalInputKind::Next => PrivateSelection::Next(self.private_next(step.images, interval, at)),
            }
        } else {
            PrivateSelection::Unavailable(DropReason::NoApprovedInterval)
        };
        self.pending.push_back(Private { opening, interval, selection, scroll_next: Selected::Waiting, refresh });
    }

    /// Make the waiting next selections of pending entries. They are made in
    /// place, so the private hold cap counts every pending entry.
    pub(super) fn select_private(&mut self, images: &NativeImages, now: SessionTimeUs) {
        for index in 0..self.pending.len() {
            let entry = &self.pending[index];
            let Some(interval) = entry.interval else { continue };
            let at = entry.received();
            let end = SessionTimeUs(at.0.saturating_add(SCROLL_US));
            let next = matches!(entry.selection, PrivateSelection::Next(Selected::Waiting));
            let scroll = matches!(entry.opening.request.kind, ProvisionalInputKind::Scroll { .. })
                && matches!(entry.scroll_next, Selected::Waiting) && now >= end;
            if next {
                let selected = self.private_next(images, interval, at);
                self.pending[index].selection = PrivateSelection::Next(selected);
            }
            if scroll {
                let selected = self.private_next(images, interval, end);
                self.pending[index].scroll_next = selected;
            }
        }
    }

    /// Publish the pending entries whose action was approved; drop those
    /// abandoned or older than `PRIVATE_US`.
    pub(super) fn resolve_private(&mut self, images: &NativeImages, config: &ResolvedCaptureConfig, now: SessionTimeUs) {
        self.take_decided(
            |entry| now.0.saturating_sub(entry.received().0) < PRIVATE_US,
            |stills, entry, permission| stills.publish_private(entry, permission, images, config),
        );
    }

    /// Rotate the pending queue once, oldest first: an entry whose action was
    /// decided goes to `decided`, an undecided entry that `wait` keeps returns
    /// to the queue, and any other is dropped. Each entry is handled before
    /// the next one is taken, so the entries not yet taken stay pending.
    pub(super) fn take_decided(
        &mut self, wait: impl Fn(&Private) -> bool, mut decided: impl FnMut(&mut Self, Private, Permission),
    ) {
        for _ in 0..self.pending.len() {
            let Some(entry) = self.pending.pop_front() else { break };
            match entry.opening.decision.try_recv() {
                Ok(permission) => decided(self, entry, permission),
                Err(TryRecvError::Empty) if wait(&entry) => self.pending.push_back(entry),
                Err(_) => {}
            }
        }
    }

    pub(super) fn publish_private(
        &mut self, mut entry: Private, mut permission: Permission, images: &NativeImages,
        config: &ResolvedCaptureConfig,
    ) {
        let request = entry.opening.request;
        // The Receiver sends a permission only for the request with its
        // geometry and foreground proof.
        // The approval guard stays at run time in case that contract breaks.
        let matches = request.geometry == Evidence::Observed(permission.geometry)
            && request.foreground_at_receipt == Some(permission.proof);
        debug_assert!(matches);
        if !matches {
            return;
        }
        let at = permission.at;
        if let Some(interval) = entry.interval {
            self.reselect_at_approval(&mut entry, interval, &permission, images, config);
        }
        let trigger = Trigger::from(request.kind);
        if let ProvisionalInputKind::Scroll { axis } = request.kind && let Some(interval) = entry.interval
            && !self.stage_scroll(&mut entry, &permission, axis, interval, images)
        {
            permission.rows.suppress();
            return;
        }
        if matches!(trigger, Trigger::PreAction | Trigger::Next) {
            self.quiet.invalidate(at);
        }
        let slots = match entry.selection {
            PrivateSelection::Preceding { adjustment, before, seen } => [
                (trigger, Role::Before, Some(adjustment), State::Selected(before)),
                (trigger, Role::Seen, Some(adjustment), State::Selected(seen)),
            ],
            PrivateSelection::Next(selected) => [
                (trigger, Role::Image, None, State::Selected(selected)),
                (Trigger::Settled, Role::Image, None, State::Settling),
            ],
            PrivateSelection::Unavailable(reason) if matches!(trigger, Trigger::Next) => [
                (trigger, Role::Image, None, State::dropped(reason)),
                (Trigger::Settled, Role::Image, None, State::dropped(reason)),
            ],
            PrivateSelection::Unavailable(reason) => [
                (trigger, Role::Before, None, State::dropped(reason)),
                (trigger, Role::Seen, None, State::dropped(reason)),
            ],
        };
        for (index, (trigger, role, adjustment, state)) in slots.into_iter().enumerate() {
            let Some(reservation) = permission.rows.take(index) else { continue };
            self.push_public(Public {
                event: Some(permission.event), interval: entry.interval, adjustment, reservation: Some(reservation),
                ..Public::new(trigger, role, at, permission.geometry, request.admission, state)
            });
        }
    }

    /// Reselect a pending entry's frames at its approved action time, where
    /// that differs from the receipt.
    fn reselect_at_approval(&mut self, entry: &mut Private, interval: Interval, permission: &Permission,
        images: &NativeImages, config: &ResolvedCaptureConfig,
    ) {
        let at = permission.at;
        let received = entry.received();
        let refresh = entry.refresh;
        match &mut entry.selection {
            PrivateSelection::Preceding { adjustment, before, seen } => {
                let Ok(adjusted) = Adjustment::from_receipt(at, calibration(config, permission.device, interval)) else {
                    entry.selection = PrivateSelection::Unavailable(DropReason::AdjustmentFailed);
                    return;
                };
                if adjusted.boundary != adjustment.boundary {
                    let pair = still_time::preceding(images, interval, adjusted, refresh);
                    *before = Selected::Waiting;
                    *seen = Selected::Waiting;
                    *before = self.retain(pair.before, still_png::Priority::BeforeSeen);
                    *seen = self.retain(pair.seen, still_png::Priority::BeforeSeen);
                }
                *adjustment = adjusted;
            }
            PrivateSelection::Next(selected) if at != received => {
                *selected = self.select_next(images, interval, at);
            }
            _ => {}
        }
    }

    /// A private selection takes a new hold only while the pending private
    /// entries hold fewer than `PRIVATE_HOLDS` distinct frames, so one extra
    /// hold stays available to public requests. `extra` is a selection of the
    /// entry being built, which is not pending yet.
    fn retain_private(&mut self, frame: Result<&NativeFrame, Missing>, priority: still_png::Priority,
        extra: Option<&Selected>,
    ) -> Selected {
        let frame = match frame { Ok(frame) => frame, Err(reason) => return Selected::Missing(reason) };
        while !self.holds.holds(frame.metadata.id) && self.private_holds(extra) >= PRIVATE_HOLDS {
            if !self.reclaim(priority, true) {
                return Selected::Capacity;
            }
        }
        self.retain(Ok(frame), priority)
    }

    fn private_next(&mut self, images: &NativeImages, interval: Interval, end: SessionTimeUs) -> Selected {
        match still_time::next(images, interval, end) {
            Ok(None) => Selected::Waiting,
            Ok(Some(frame)) => self.retain_private(Ok(frame), still_png::Priority::Next, None),
            Err(reason) => Selected::Missing(reason),
        }
    }

    fn private_holds(&self, extra: Option<&Selected>) -> usize {
        let mut frames: Vec<*const HeldFrame> = Vec::new();
        for frame in self.pending.iter().flat_map(Private::held).chain(extra.and_then(Selected::retained)) {
            if !frames.contains(&Rc::as_ptr(frame)) {
                frames.push(Rc::as_ptr(frame));
            }
        }
        frames.len()
    }
}

/// The receipt-delay calibration of an ordinary input device. Input of an
/// elevated visit has none.
fn calibration(config: &ResolvedCaptureConfig, device: Option<DeviceId>, interval: Interval) -> Option<DeviceCalibration> {
    device.filter(|_| interval.elevated_visit.is_none()).and_then(|device| config.calibration(device).observed())
}
