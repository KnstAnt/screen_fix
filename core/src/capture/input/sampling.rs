use std::collections::{BTreeMap, VecDeque};

use crate::protocol::{
    DeviceId, SessionTimeUs,
    ipc::{HidReport, HidReportResult, HidUsage, HidValue, InputSample},
};

use super::{Action, HeldInput};

const MOTION_IDLE_US: i64 = 16_667;
const MAX_PENDING: usize = 1_024;
const MAX_PEN_LANES: usize = 256;

pub(crate) enum Sampled {
    Retained(HeldInput),
    Thinned(HeldInput),
    Dropped(HeldInput),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Lane {
    Pointer,
    Pen(DeviceId, u16),
}

#[derive(Clone, Copy)]
struct Movement {
    at: SessionTimeUs,
    switches: u16,
}

struct Slot {
    input: HeldInput,
    pending: Option<Lane>,
    thinned: bool,
}

#[derive(Default)]
pub(crate) struct MotionSampler {
    movement: BTreeMap<Lane, Movement>,
    prefix: VecDeque<Slot>,
}

impl MotionSampler {
    pub fn push(&mut self, input: HeldInput, mut resolve: impl FnMut(Sampled)) {
        let first = &input.normalized.correlated.first;
        let t = first.time.recv_t_us;
        self.expire(t);
        self.emit_ready(&mut resolve);
        if self.prefix.len() == MAX_PENDING {
            resolve(Sampled::Dropped(input));
            return;
        }
        let sample = match &first.packet.sample {
            InputSample::Hid(report) if report.is_pen() => {
                let capabilities = hover(report);
                if capabilities.is_none() {
                    self.close_pen(report.device);
                }
                capabilities
            }
            InputSample::Hid(report) => {
                if report.pen_continuity_uncertain() {
                    self.close_pen(report.device);
                }
                None
            }
            _ if matches!(first.action, Action::Motion(_)) => {
                // Hardware delta evidence alone does not open/close a burst
                // while the pointer hook is healthy.
                input.normalized.cursor_motion.then_some((Lane::Pointer, 0))
            }
            _ => {
                self.close_lanes();
                None
            }
        };
        let pending = if let Some((lane, switches)) = sample.filter(|_| !input.held_motion) {
            let within_limit = lane == Lane::Pointer || self.movement.contains_key(&lane)
                || self.movement.len() < MAX_PEN_LANES;
            if within_limit {
                let previous = self.movement.insert(lane, Movement { at: t, switches });
                let continuous = previous.is_some_and(|previous| previous.switches == switches);
                if let Some(old) = self.prefix.iter_mut().rev().find(|slot| slot.pending == Some(lane)) {
                    old.pending = None;
                    old.thinned = continuous && previous.is_some_and(|previous| {
                        sample_window(previous.at) == sample_window(t)
                    });
                }
                continuous.then_some(lane)
            } else {
                None
            }
        } else {
            if input.held_motion {
                self.close_lanes();
            }
            None
        };
        self.prefix.push_back(Slot { input, pending, thinned: false });
        self.emit_ready(&mut resolve);
    }

    pub fn advance(&mut self, bound: SessionTimeUs, mut resolve: impl FnMut(Sampled)) {
        self.expire(bound);
        self.emit_ready(&mut resolve);
    }

    pub fn finish(&mut self, resolve: impl FnMut(Sampled)) {
        self.flush(resolve);
    }

    pub fn flush(&mut self, mut resolve: impl FnMut(Sampled)) {
        self.close_lanes();
        self.emit_ready(&mut resolve);
    }

    fn expire(&mut self, bound: SessionTimeUs) {
        self.movement.retain(|_, last| bound.0.saturating_sub(last.at.0) < MOTION_IDLE_US);
        for slot in &mut self.prefix {
            if slot.pending.is_some_and(|lane| !self.movement.contains_key(&lane)) {
                slot.pending = None;
            }
        }
    }

    fn close_pen(&mut self, device: DeviceId) {
        let is_device = |lane| matches!(lane, Lane::Pen(id, _) if id == device);
        self.movement.retain(|lane, _| !is_device(*lane));
        for slot in &mut self.prefix {
            if slot.pending.is_some_and(is_device) {
                slot.pending = None;
            }
        }
    }

    fn close_lanes(&mut self) {
        self.movement.clear();
        for slot in &mut self.prefix {
            slot.pending = None;
        }
    }

    fn emit_ready(&mut self, resolve: &mut impl FnMut(Sampled)) {
        while self.prefix.front().is_some_and(|slot| slot.pending.is_none()) {
            let Some(slot) = self.prefix.pop_front() else { break };
            resolve(if slot.thinned { Sampled::Thinned(slot.input) } else { Sampled::Retained(slot.input) });
        }
    }

    pub fn discard_pending(&mut self) -> impl Iterator<Item = HeldInput> + '_ {
        self.movement.clear();
        self.prefix.drain(..).map(|slot| slot.input)
    }

    pub fn unresolved_before(&self) -> Option<SessionTimeUs> {
        self.prefix.front().map(|slot| slot.input.normalized.correlated.first.time.recv_t_us)
    }
}

fn hover(report: &HidReport) -> Option<(Lane, u16)> {
    let (link, false) = report.pen_tip()? else { return None };
    let HidReportResult::Report(decoded) = &report.result else { return None };
    let mut coordinates = 0;
    let mut switches = 0;
    for field in decoded.fields.as_slice() {
        if matches!(field.value, HidValue::Unavailable
            | HidValue::Number { raw: None, .. } | HidValue::Angle { raw: None, .. })
        {
            return None;
        }
        match (field.usage, field.value) {
            (HidUsage::X, HidValue::Number { raw: Some(_), .. }) if field.link == link => coordinates |= 1,
            (HidUsage::Y, HidValue::Number { raw: Some(_), .. }) if field.link == link => coordinates |= 2,
            (usage, HidValue::Switch(active)) => {
                let shift = match usage {
                    HidUsage::Tip => 0,
                    HidUsage::InRange => 2,
                    HidUsage::Barrel => 4,
                    HidUsage::SecondaryBarrel => 6,
                    HidUsage::Invert => 8,
                    HidUsage::Eraser => 10,
                    _ => return None,
                };
                if field.link != link {
                    return None;
                }
                switches |= (1 | ((active as u16) << 1)) << shift;
            }
            _ => {}
        }
    }
    let in_range = switches & (3 << 2) == (3 << 2);
    let controls_down = switches & ((1 << 5) | (1 << 7) | (1 << 11)) != 0;
    (coordinates == 3 && in_range && !controls_down).then_some((Lane::Pen(report.device, link), switches))
}

fn sample_window(t: SessionTimeUs) -> i128 {
    (i128::from(t.0) * 60).div_euclid(1_000_000)
}
