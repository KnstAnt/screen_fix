//! Publication policy of the input gate that needs no Windows state: pending
//! terminal outcomes, modifier knowledge and the per-second health counts.

use std::collections::BTreeMap;

use crate::{
    package::{journal::HealthRecord, outcome::{LossEvidence, LossScope, SuppressionClass}},
    protocol::{DeviceId, Evidence, SessionTimeUs, UnavailableReason, ipc::InputSample},
};

use super::{Action, HeldInput, Key, Modifier, Motion, PendingInput, modifier_bit};

/// Input terminal outcomes that are not rows: losses and policy suppressions.
/// The publication owner admits them to the input journal in receipt order.
#[derive(Default)]
pub(crate) struct Outcomes {
    /// Ordinary losses, at most one per (reason, producer). Each is admitted
    /// only once every record at or before its last receipt has been
    /// published, so its range never exceeds its admission receipt.
    pub(crate) ordinary: Vec<LossEvidence>,
    /// Count-only losses (§6.1 rule 5); they carry no time.
    pub(crate) sensitive: Vec<LossEvidence>,
    pub(crate) suppressed: BTreeMap<SuppressionClass, u64>,
    pub(crate) health: Vec<HealthRecord>,
}

impl Outcomes {
    pub(crate) fn loss(&mut self, evidence: LossEvidence) {
        let pending = match evidence.scope {
            LossScope::Ordinary(_) => &mut self.ordinary,
            LossScope::Sensitive => &mut self.sensitive,
        };
        let mut evidence = evidence;
        for entry in pending.iter_mut() {
            match entry.merge(evidence) {
                Ok(merged) => {
                    *entry = merged;
                    return;
                }
                Err(unmerged) => evidence = unmerged,
            }
        }
        pending.push(evidence);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ModifierKnowledge {
    pub(crate) known: u8,
    // A second device may still hold a side after an approved device releases it.
    pub(crate) possibly_hidden: u8,
    pub(crate) awaiting_confirmation: u8,
}

impl ModifierKnowledge {
    pub(crate) fn from_released(released: u8) -> Self {
        Self { known: released, possibly_hidden: !released, awaiting_confirmation: 0 }
    }

    pub(crate) fn conceal(&mut self, bits: u8) {
        self.known &= !bits;
        self.possibly_hidden |= bits;
        self.awaiting_confirmation &= !bits;
    }

    pub(crate) fn after(mut self, input: &HeldInput, key: Key) -> Self {
        self.known &= input.held.modifiers_known;
        self.awaiting_confirmation &= input.held.modifiers;
        if let Some(modifier) = key.modifier {
            let bit = modifier_bit(modifier);
            self.awaiting_confirmation &= !bit;
            let held = input.held.modifiers & bit != 0;
            if input.held.modifiers_known & bit == 0
                || matches!(input.repeat, Evidence::Unavailable(UnavailableReason::Ambiguous | UnavailableReason::Blind))
            {
                self.conceal(bit);
            } else if key.down && held {
                self.known |= bit;
            } else if !key.down && !held {
                self.known |= bit;
                self.possibly_hidden &= !bit;
            } else if !key.down && self.possibly_hidden & bit != 0 {
                self.known &= !bit;
            }
        } else {
            self.conceal(modifier_bits(key));
        }
        self
    }
}

pub(crate) fn modifier_bits(key: Key) -> u8 {
    match (key.modifier, key.virtual_key) {
        (Some(modifier), _) => modifier_bit(modifier),
        (None, 0x10) => modifier_bit(Modifier::LeftShift) | modifier_bit(Modifier::RightShift),
        (None, 0x11) => modifier_bit(Modifier::LeftCtrl) | modifier_bit(Modifier::RightCtrl),
        (None, 0x12) => modifier_bit(Modifier::LeftAlt) | modifier_bit(Modifier::RightAlt),
        _ => 0,
    }
}

/// One session second of `input_health` counts (INP-6): coalesced raw
/// packets, thinned motion rows, policy suppressions that carry no
/// privacy-sensitive timing, and raw-motion evidence that no cursor sample
/// carries: per device, the packet count and summed relative delta (stage I2).
#[derive(Default)]
pub(crate) struct MotionCounts {
    pub(crate) second: i64,
    pub(crate) coalesced: u64,
    pub(crate) thinned: u64,
    pub(crate) suppressed: BTreeMap<SuppressionClass, u64>,
    pub(crate) raw_motion: BTreeMap<Option<DeviceId>, (u64, i64, i64)>,
}

impl MotionCounts {
    /// The bucket of the second of `at`, opened if none is pending.
    pub(crate) fn at(motion: &mut Option<Self>, at: SessionTimeUs) -> &mut Self {
        motion.get_or_insert(Self { second: at.0.div_euclid(1_000_000), ..Self::default() })
    }

    pub(crate) fn raw_motion(&mut self, record: &PendingInput) {
        let InputSample::RawPointer(pointer) = record.packet.sample else { return };
        let Action::Motion(motion) = record.action else { return };
        let (packets, x, y) = self.raw_motion.entry(pointer.device).or_default();
        *packets = packets.saturating_add(1);
        if let Motion::Relative { x: dx, y: dy } = motion {
            *x = x.saturating_add(i64::from(dx));
            *y = y.saturating_add(i64::from(dy));
        }
    }
}

/// Suppression classes whose per-second counts can be written. Approval,
/// focus and cluster classes can belong to an unapproved or sensitive visit
/// and are counted only in the stream totals (§6.1).
pub(crate) fn per_second(class: SuppressionClass) -> bool {
    matches!(class, SuppressionClass::RecorderControl | SuppressionClass::UnconfirmedSynchronization
        | SuppressionClass::NotPublishable)
}
