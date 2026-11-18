//! Terminal-outcome vocabulary shared by every stream owner, the journal loss
//! ledger, the manifest health summary and the package reader.
//!
//! Every item an owner accepts ends as exactly one of: a persisted row, a
//! policy suppression (counted only), a ledger loss (a typed `gap` row, or a
//! count only when the loss is privacy sensitive) or an explicit unavailable row.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::protocol::{QpcTick, SessionTimeUs};

/// Closed vocabulary of reasons why items never reached a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossReason {
    /// The stream's bounded data queue or byte budget was full (REL-2).
    QueueOverflow,
    /// A confirmed text commit arrived behind the already published input prefix.
    LateCommit,
    /// A bounded staging or confirmation capacity could not hold the item.
    StagingPressure,
    /// The item's receipt precedes a prefix that was already published or certified.
    LateRow,
    /// The producer stopped advancing its progress within the bounded wait.
    ProducerStall,
    /// The retained provisional evidence for an approved action no longer existed.
    RetentionUnavailable,
    /// The input helper's bounded native queue or transport lost records.
    NativeQueueOverflow,
    /// The bounded input matcher could not hold the record.
    MatcherCapacity,
    /// The record arrived before the opening or post-gap held-state resync completed.
    ResyncPending,
    /// The receipt-time foreground was not accessible to the ordinary source.
    ForegroundInaccessible,
    /// The record named a device absent from the current device inventory.
    UnknownDevice,
    /// Required hook evidence was confirmed lost; records from the first missed
    /// receipt cannot extend the certified prefix.
    HookLost,
    /// A native packet or its encoded record was invalid and was skipped.
    MalformedPacket,
    /// A bounded commit queue (the TextEdit callback slots or a confirmation
    /// stage) could not hold a finalized text commit.
    CommitCapacity,
    /// A finalized text commit exceeded the bounded UTF-16 length.
    CommitTooLong,
    /// A finalized text commit had more parts than the bound, or an unreadable part array.
    CommitParts,
    /// A finalized text commit failed the sender, foreground or callback
    /// deadline check; it is counted without detail (E3).
    CommitRejected,
    /// A bounded clipboard notice, grant or result queue was full; no content is kept.
    ClipboardQueue,
    /// The bounded window notification ring or queue was full.
    NotificationOverflow,
    /// Elevated records past the certified prefix of an interrupted visit (E1).
    UncertifiedSource,
}

/// Typed identity of the producer whose items were lost, when it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossProducer {
    /// Confirmed TextEdit composition commits observed by the context owner.
    TextEdit,
    /// Raw Input keyboard, pointer and HID records.
    RawInput,
    /// Raw Input device arrival and removal notifications.
    DeviceNotifications,
    /// The low-level keyboard hook.
    KeyboardHook,
    /// The low-level pointer hook.
    PointerHook,
    /// HID reports of the optional touchpad and multi-axis collections.
    OptionalHid,
}

impl From<crate::protocol::ipc::Producer> for LossProducer {
    fn from(producer: crate::protocol::ipc::Producer) -> Self {
        use crate::protocol::ipc::Producer;
        match producer {
            Producer::RawInput => Self::RawInput,
            Producer::DeviceNotifications => Self::DeviceNotifications,
            Producer::KeyboardHook => Self::KeyboardHook,
            Producer::PointerHook => Self::PointerHook,
            Producer::OptionalHid => Self::OptionalHid,
        }
    }
}

/// Policy classes for items deliberately withheld from a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionClass {
    /// Observations made while the attempt was paused (PRIV-2).
    Pause,
    /// The receipt-time foreground was not approved for recording.
    NotApproved,
    /// The receipt-time approval could not be established (no raw foreground
    /// proof, disagreeing proofs, or hook evidence without a current visit).
    ApprovalUnknown,
    /// A confirmed recorder control chord transition.
    RecorderControl,
    /// A transition that may belong to a recorder control chord.
    ControlUncertain,
    /// Hook records of an ambiguous cluster beyond its raw record count.
    ClusterExcessHook,
    /// A hook-only keyboard record whose focus changed inside its receipt
    /// uncertainty interval and that no adjacent redacted visit could absorb.
    FocusUncertain,
    /// Synthetic held-state edges whose synchronization target was not
    /// confirmed as safe to publish.
    UnconfirmedSynchronization,
    /// A record with no publishable input form (an AltGr companion transition).
    NotPublishable,
}

/// Kinds of runtime and final health rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthKind {
    /// One monitor's video can no longer be produced.
    VideoFailure,
    /// An HDR monitor without an SDR white level is captured through the
    /// system BGRA8 conversion (VID-8).
    HdrWhiteLevelUnavailable,
    /// A hardware encoder candidate was rejected for a new video format (VID-6).
    EncoderCandidateRejected,
    /// A monitor's video had not closed its open segment when the system
    /// slept; finalization continued after wake (D-5 A, G).
    SuspendFinalizeLate,
    /// The standard input source can no longer be produced.
    InputFailure,
    /// The attempt failed; written with the final rows.
    AttemptFailure,
    /// A still producer stopped advancing its receipt progress.
    StillProgressStall,
    /// Counts of health conditions observed while paused and not yet reported
    /// in a resume row; written only with the final rows of a stream closed while paused.
    PausedSummary,
    /// An input producer stopped advancing its progress (G from 250 ms, L from 5 s).
    InputProducerStall,
    /// A required Raw Input registration was lost.
    InputRegistrationLost,
    /// A required low-level hook was suspected lost.
    InputHookLost,
    /// An optional Raw Input collection is no longer registered.
    OptionalCollectionUnavailable,
    /// An approved executable spelling could not be canonicalized, so its
    /// foreground stays approval-unknown (§6.2 row 18, G).
    IdentityUnresolved,
    /// An auxiliary activity stopped for the rest of the attempt: UI
    /// Automation, identity resolution, the PNG encoder or the cursor asset
    /// owner (§6.2 rows 14–16, L-auxiliary).
    AuxiliaryActivityLoss,
    /// A still image could not be written or published; one row per episode
    /// of the same failing operation (G).
    StillWriteFailure,
    /// The elevated collector did not survive a system sleep; a later
    /// elevated visit launches a new one (E4, G).
    ElevatedSourceSleep,
}

/// Why a UI Automation read ended without an element: the `uia` value of a
/// focus or element row in place of its element (CTX-9 `"timeout"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiaOutcome {
    Timeout,
    Hung,
    Unavailable,
    Stale,
    OutsideApproval,
}

impl UiaOutcome {
    /// The recorded outcome of a read unavailable for this reason.
    pub(crate) fn of(reason: crate::protocol::ipc::elevated::ReadUnavailable) -> Self {
        use crate::protocol::ipc::elevated::ReadUnavailable;
        match reason {
            ReadUnavailable::TimedOut => Self::Timeout,
            ReadUnavailable::Hung => Self::Hung,
            ReadUnavailable::Unavailable => Self::Unavailable,
            ReadUnavailable::Stale => Self::Stale,
            ReadUnavailable::OutsideApproval => Self::OutsideApproval,
        }
    }
}

/// The context read of a row whose `uia` value is an outcome (manifest `uia_unavailable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiaRead {
    Focus,
    Element,
}

/// The number of lost items. An unknown count stays unknown when merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossCount {
    Known(NonZeroU64),
    Unknown,
}

impl LossCount {
    pub(crate) fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Known(left), Self::Known(right)) => Self::Known(left.saturating_add(right.get())),
            _ => Self::Unknown,
        }
    }

    pub(crate) fn known(self) -> Option<u64> {
        match self {
            Self::Known(count) => Some(count.get()),
            Self::Unknown => None,
        }
    }
}

/// Original receipt bounds of ordinary lost items, with optional native ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LossRange {
    pub first_t_us: SessionTimeUs,
    pub last_t_us: SessionTimeUs,
    pub source_seq: Option<[u64; 2]>,
    pub qpc: Option<[QpcTick; 2]>,
}

impl LossRange {
    /// The smallest range covering both ranges (§6.1 merge rule).
    pub(crate) fn merge(self, other: Self) -> Self {
        Self {
            first_t_us: self.first_t_us.min(other.first_t_us),
            last_t_us: self.last_t_us.max(other.last_t_us),
            source_seq: merge_bounds(self.source_seq, other.source_seq),
            qpc: merge_bounds(self.qpc, other.qpc),
        }
    }
}

/// A sensitive loss belongs to a password, possibly sensitive or
/// approval-unknown context. It is counted without any time or sequence range
/// and never becomes a gap row (ALG §7.2, PRIV-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossScope {
    Ordinary(LossRange),
    Sensitive,
}

/// Typed loss evidence reported by an owner to its stream ingress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LossEvidence {
    pub reason: LossReason,
    pub producer: Option<LossProducer>,
    pub count: LossCount,
    pub scope: LossScope,
}

impl LossEvidence {
    pub(crate) fn ordinary(
        reason: LossReason, producer: Option<LossProducer>, count: NonZeroU64,
        first_t_us: SessionTimeUs, last_t_us: SessionTimeUs,
    ) -> Self {
        Self {
            reason, producer, count: LossCount::Known(count),
            scope: LossScope::Ordinary(LossRange { first_t_us, last_t_us, source_seq: None, qpc: None }),
        }
    }

    pub(crate) fn sensitive(reason: LossReason, producer: Option<LossProducer>, count: NonZeroU64) -> Self {
        Self { reason, producer, count: LossCount::Known(count), scope: LossScope::Sensitive }
    }

    /// Merge evidence with the same reason, producer and privacy class (§6.1
    /// merge rule). Evidence with a different key is returned unchanged.
    pub(crate) fn merge(self, other: Self) -> Result<Self, Self> {
        if (self.reason, self.producer) != (other.reason, other.producer) {
            return Err(other);
        }
        let scope = match (self.scope, other.scope) {
            (LossScope::Sensitive, LossScope::Sensitive) => LossScope::Sensitive,
            (LossScope::Ordinary(left), LossScope::Ordinary(right)) => LossScope::Ordinary(left.merge(right)),
            _ => return Err(other),
        };
        Ok(Self { count: self.count.merge(other.count), scope, ..self })
    }
}

fn merge_bounds<T: Ord + Copy>(left: Option<[T; 2]>, right: Option<[T; 2]>) -> Option<[T; 2]> {
    match (left, right) {
        (Some(left), Some(right)) => Some([left[0].min(right[0]), left[1].max(right[1])]),
        (range, None) | (None, range) => range,
    }
}

/// The payload of one `gap` row. The row's own `recv_t_us` is its admission
/// receipt; the range fields keep the original loss times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapRecord {
    pub reason: LossReason,
    pub producer: Option<LossProducer>,
    pub count: Option<u64>,
    pub count_known: bool,
    pub first_recv_t_us: SessionTimeUs,
    pub last_recv_t_us: SessionTimeUs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_source_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_source_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_qpc: Option<QpcTick>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_qpc: Option<QpcTick>,
}

impl GapRecord {
    /// Structural validity of a deserialized row against its admission receipt.
    pub(crate) fn is_valid(&self, admitted: SessionTimeUs) -> bool {
        self.count_known == self.count.is_some() && self.count != Some(0)
            && self.first_recv_t_us <= self.last_recv_t_us && self.last_recv_t_us <= admitted
            && ordered_pair(self.first_source_seq, self.last_source_seq)
            && ordered_pair(self.first_qpc, self.last_qpc)
    }
}

/// Optional range bounds are present together and ordered.
fn ordered_pair<T: PartialOrd>(first: Option<T>, last: Option<T>) -> bool {
    match (first, last) {
        (Some(first), Some(last)) => first <= last,
        (None, None) => true,
        _ => false,
    }
}

/// Per-reason loss totals of one stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedTotal {
    /// Sum of the known counts written as gap rows.
    pub gap_items: u64,
    /// Gap rows whose item count is unknown; they are excluded from `gap_items`.
    pub unknown_gaps: u64,
    /// Known items of privacy-sensitive losses, which never become gap rows.
    pub sensitive_items: u64,
    /// Privacy-sensitive losses with an unknown count.
    pub sensitive_unknown: u64,
}
