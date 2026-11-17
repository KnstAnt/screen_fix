use crate::{
    capture::context::ControllerHint,
    protocol::{DeviceId, Evidence, PhysicalPoint, UnavailableReason, ipc::InputSample},
};

use super::{
    Action, Button, CorrelatedInput, Correlation, InputOrigin, Key, Motion, PendingInput,
    PointerEvidence, matching::{injected_hook, promoted},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OriginEvidence {
    pub origin: InputOrigin,
    pub device: Evidence<DeviceId>,
    pub cursor_id: Evidence<u8>,
    /// INP-7: an AutoHotkey extra-information value is a hint, never proof.
    pub injector_hint: Option<ControllerHint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PointerDetails {
    pub points: PointerEvidence,
    pub raw_relative: Evidence<(i64, i64)>,
    pub raw_absolute: Evidence<PhysicalPoint>,
    /// The absolute packet's raw x/y (0..65535) and move flags (INP-5).
    pub raw_absolute_packet: Option<([i32; 2], u16)>,
    pub raw_packets: u16,
    pub raw_wheel: Evidence<i16>,
    pub hook_wheel: Evidence<i16>,
    pub physical_button: Evidence<Button>,
    pub warp: Evidence<bool>,
}

pub(crate) struct NormalizedInput {
    pub correlated: CorrelatedInput,
    pub origin: OriginEvidence,
    pub key_name: Option<String>,
    pub pointer: Option<PointerDetails>,
    /// A cursor sample: hook motion, or raw motion while the pointer hook is
    /// lost (INP-6). Other raw-only motion is device-delta evidence, never a
    /// cursor position (stage I2).
    pub cursor_motion: bool,
}

impl NormalizedInput {
    pub fn new(
        correlated: CorrelatedInput,
        pointer_hook_lost: bool,
        layout_name: impl Fn(Key, Option<u64>) -> Option<String>,
    ) -> Self {
        let all = || std::iter::once(&correlated.first).chain(&correlated.partners);
        let raw = all().find(|slot| matches!(slot.packet.sample,
            InputSample::RawKey(_) | InputSample::RawPointer(_) | InputSample::Hid(_)));
        let hook = all().find(|slot| matches!(slot.packet.sample,
            InputSample::HookKey(_) | InputSample::HookPointer(_)));
        let reason = match correlated.correlation {
            Correlation::Ambiguous | Correlation::MissingScan => UnavailableReason::Ambiguous,
            Correlation::Discontinuous => UnavailableReason::Blind,
            _ => UnavailableReason::NotObserved,
        };
        let origin = origin(&correlated, raw, hook, pointer_hook_lost, reason);
        let key_name = match correlated.first.action {
            Action::Key(key) => layout_name(key, raw.and_then(|slot| slot.packet.foreground_at_receipt?.focus.map(|focus| focus.keyboard_layout))),
            _ => None,
        };
        let cursor_motion = matches!(hook.map(|slot| slot.action), Some(Action::Motion(_)))
            || pointer_hook_lost && matches!(correlated.first.action, Action::Motion(_));
        let pointer = matches!(correlated.first.action,
            Action::Button { .. } | Action::Wheel { .. } | Action::Motion(_))
            .then(|| {
                let event = hook.and_then(|slot| match slot.packet.sample {
                    InputSample::HookPointer(pointer) => Some(pointer.point),
                    _ => None,
                });
                let received = raw.and_then(|slot| slot.packet.position_at_receipt);
                let geometry = match (event, received) {
                    (Some(_), _) => hook.map_or(Evidence::Unavailable(reason), |slot| slot.geometry),
                    (None, Some(_)) => match raw.map_or(Evidence::Unavailable(reason), |slot| slot.geometry) {
                        Evidence::Observed(value) => Evidence::Uncertain { value, reason },
                        other => other,
                    },
                    _ => Evidence::Unavailable(reason),
                };
                let mut delta = None;
                let mut raw_count = 0;
                let mut raw_absolute = Evidence::Unavailable(reason);
                let mut raw_absolute_packet = None;
                for slot in all().filter(|slot| matches!(slot.packet.sample, InputSample::RawPointer(_))) {
                    raw_count += 1;
                    match slot.action {
                        Action::Motion(Motion::Relative { x, y }) => {
                            let (sum_x, sum_y) = delta.unwrap_or((0_i64, 0_i64));
                            delta = Some((sum_x + i64::from(x), sum_y + i64::from(y)));
                        }
                        Action::Motion(Motion::Absolute { x, y, desktop_point }) => {
                            raw_absolute = desktop_point;
                            if let InputSample::RawPointer(pointer) = slot.packet.sample {
                                raw_absolute_packet = Some(([x, y], pointer.move_flags));
                            }
                        }
                        _ => {}
                    }
                }
                let wheel = |slot: &PendingInput| match slot.action {
                    Action::Wheel { value, .. } => Some(value),
                    _ => None,
                };
                let physical = raw.and_then(|slot| match slot.action {
                    Action::Button { physical, .. } => physical,
                    _ => None,
                });
                let warp = match origin.origin {
                    InputOrigin::PenPromoted | InputOrigin::TouchPromoted => Evidence::Observed(false),
                    _ if event.is_some() && delta.is_some() => Evidence::Unavailable(UnavailableReason::MissingCalibration),
                    _ => Evidence::Unavailable(reason),
                };
                PointerDetails {
                    points: PointerEvidence {
                        origin: origin.origin,
                        device: origin.device,
                        monitor: match geometry {
                            Evidence::Observed(id) => Evidence::Observed(id.monitor),
                            Evidence::Uncertain { value, reason } => Evidence::Uncertain { value: value.monitor, reason },
                            Evidence::Unavailable(reason) => Evidence::Unavailable(reason),
                        },
                        geometry,
                        event_position: event.map_or(Evidence::Unavailable(reason), Evidence::Observed),
                        receipt_position: received.map_or(Evidence::Unavailable(UnavailableReason::NotObserved), Evidence::Observed),
                        event_position_receipt_t_us: event.and(hook).map(|slot| slot.time.recv_t_us),
                        receipt_position_t_us: received.and(raw).map(|slot| slot.time.recv_t_us),
                    },
                    raw_relative: delta.map_or(Evidence::Unavailable(reason), Evidence::Observed),
                    raw_absolute,
                    raw_absolute_packet,
                    raw_packets: raw_count,
                    raw_wheel: raw.and_then(wheel).map_or(Evidence::Unavailable(reason), Evidence::Observed),
                    hook_wheel: hook.and_then(wheel).map_or(Evidence::Unavailable(reason), Evidence::Observed),
                    physical_button: physical.map_or(Evidence::Unavailable(reason), Evidence::Observed),
                    warp,
                }
            });
        Self { correlated, origin, key_name, pointer, cursor_motion }
    }
}

fn origin(
    correlated: &CorrelatedInput, raw: Option<&PendingInput>, hook: Option<&PendingInput>,
    pointer_hook_lost: bool, reason: UnavailableReason,
) -> OriginEvidence {
    let mut signature = None;
    let mut conflicting = false;
    for slot in std::iter::once(&correlated.first).chain(&correlated.partners) {
        let extra = match slot.packet.sample {
            InputSample::RawPointer(pointer) => u64::from(pointer.extra_information),
            InputSample::HookPointer(pointer) => pointer.extra_information,
            _ => continue,
        };
        if promoted(extra) {
            let current = extra as u8;
            conflicting |= signature.is_some_and(|old| old != current);
            signature = Some(current);
        }
    }
    let device = raw.and_then(|slot| match &slot.packet.sample {
        InputSample::RawKey(key) => key.device,
        InputSample::RawPointer(pointer) => pointer.device,
        InputSample::Hid(report) => Some(report.device),
        _ => None,
    });
    let device = match (device, correlated.correlation) {
        (Some(value), Correlation::MissingScan) => Evidence::Uncertain { value, reason },
        (Some(value), _) => Evidence::Observed(value),
        (None, _) if raw.is_some() => Evidence::Unavailable(UnavailableReason::NoDevice),
        (None, _) => Evidence::Unavailable(reason),
    };
    if conflicting {
        return OriginEvidence {
            origin: InputOrigin::Unknown, device,
            cursor_id: Evidence::Unavailable(UnavailableReason::Ambiguous), injector_hint: None,
        };
    }
    if let Some(signature) = signature {
        return OriginEvidence {
            origin: if signature & 0x80 == 0 { InputOrigin::PenPromoted } else { InputOrigin::TouchPromoted },
            device, cursor_id: Evidence::Observed(signature & 0x7f), injector_hint: None,
        };
    }
    // An ambiguous cluster row carries no hook of its own. Its component is
    // injected when every hook is flagged and every raw record has a NULL handle.
    if correlated.cluster.is_some_and(|cluster| cluster.injected) {
        return OriginEvidence {
            origin: InputOrigin::Injected, device, cursor_id: Evidence::Unavailable(UnavailableReason::NoDevice),
            injector_hint: None,
        };
    }
    let injected = hook.filter(|slot| injected_hook(&slot.packet.sample)).and_then(|slot| match slot.packet.sample {
        InputSample::HookKey(key) => Some(key.extra_information),
        InputSample::HookPointer(pointer) => Some(pointer.extra_information),
        _ => None,
    });
    if let Some(extra) = injected {
        return OriginEvidence {
            origin: InputOrigin::Injected, device, cursor_id: Evidence::Unavailable(UnavailableReason::NoDevice),
            injector_hint: (0xffc3_d3e9..=0xffc3_d450).contains(&extra).then_some(ControllerHint::AutoHotkey),
        };
    }
    if raw.is_some_and(|slot| matches!(&slot.packet.sample,
        InputSample::Hid(report) if (report.usage_page, report.usage) == (Some(0x0D), Some(5))))
        && matches!(device, Evidence::Observed(_))
    {
        return OriginEvidence {
            origin: InputOrigin::Touchpad, device,
            cursor_id: Evidence::Unavailable(UnavailableReason::NotObserved), injector_hint: None,
        };
    }
    let qualified = correlated.correlation == Correlation::Matched
        || pointer_hook_lost && raw.is_some_and(|slot| matches!(slot.packet.sample, InputSample::RawPointer(_)))
        || raw.is_some_and(|slot| matches!(slot.packet.sample, InputSample::Hid(_)));
    OriginEvidence {
        origin: if qualified && matches!(device, Evidence::Observed(_)) { InputOrigin::Hardware } else { InputOrigin::Unknown },
        device, cursor_id: Evidence::Unavailable(UnavailableReason::NotObserved), injector_hint: None,
    }
}
