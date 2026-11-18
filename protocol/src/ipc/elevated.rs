//! Portable facts shared by the native observations and the elevated source.
//! Window IDs identify observed Windows objects. They do not authorize reads.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use crate::{
    AdmissionEpoch, BoundedList, BoundedText, DeviceId, FocusGeneration, GeometryId,
    MessageTickMs, PayloadLimit, PhysicalPoint, PhysicalRect, QpcTick, RequestId, WindowId,
};

use super::{
    CollectedInput, DeviceDescriptor, EnvironmentState, FocusAtReceipt, HelperToCore, OptionalCollection, ProcessInstance,
    Producer, ProducerGap, ProducerProgress, Receipt,
};

pub const MAX_IMAGE_PATH_BYTES: usize = 4096;
pub const MAX_METADATA_BYTES: usize = 4096;
pub const MAX_UIA_VALUE_UTF16: usize = 1024;
pub const MAX_UIA_ANCESTORS: usize = 8;
pub const MAX_COMMIT_UTF16: usize = 1024;
pub const MAX_COMMIT_PARTS: usize = 8;
const MAX_COMMIT_BYTES: usize = 3 * MAX_COMMIT_UTF16;

/// Exact Windows-reported spelling for comparison with a live process observation.
pub type ImagePath = BoundedText<MAX_IMAGE_PATH_BYTES>;
pub type Metadata = BoundedText<MAX_METADATA_BYTES>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowInstance {
    pub window: WindowId,
    pub process: ProcessInstance,
    pub thread: NonZeroU32,
}

/// The original source and visit remain attached to evidence queued before a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisitBinding {
    pub epoch: AdmissionEpoch,
    pub source_generation: NonZeroU64,
    pub grant: NonZeroU64,
    pub witness_revision: u64,
    pub foreground: WindowInstance,
    pub focus_generation: FocusGeneration,
    pub focus_revision: u64,
    pub resync_revision: NonZeroU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadBinding {
    pub visit: VisitBinding,
    pub request: RequestId,
    pub target: WindowInstance,
    pub requested_qpc: QpcTick,
    pub deadline_qpc: QpcTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointBinding {
    pub point: PhysicalPoint,
    pub root: WindowInstance,
    pub geometry: GeometryId,
    pub action_qpc: Option<QpcTick>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusEvidence {
    pub focus: FocusAtReceipt,
    pub caret: Option<PhysicalRect>,
    pub caret_window: Option<WindowId>,
    pub gui_flags: u32,
    pub hung: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowEvidence {
    pub bounds: Option<PhysicalRect>,
    pub title: Option<Metadata>,
    pub class: Option<Metadata>,
    pub dpi: Option<NonZeroU32>,
    pub dpi_awareness: Option<i32>,
    pub enabled: bool,
    pub owned_visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowNotice {
    pub event: u32,
    pub window: WindowId,
    pub object: i32,
    pub thread: u32,
    pub source_tick: Option<MessageTickMs>,
    pub receipt: QpcTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HungEvidence {
    pub window: WindowInstance,
    pub direct: bool,
    pub ghost: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldSnapshot {
    /// Left/right Shift, Control, Alt and Windows keys, in that order.
    pub modifiers: [bool; 8],
    /// Left, right, middle, X1 and X2 physical buttons, in that order.
    pub physical_buttons: [bool; 5],
    pub swapped: bool,
    pub caps_lock: bool,
    /// Every virtual key whose physical high bit was set, as a 256-bit set
    /// (bit `vk % 64` of word `vk / 64`).
    pub keys: [u64; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImeState {
    NotIme,
    Closed,
    PassThrough,
    Composing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextClassification {
    Password,
    Editable,
    NonText,
    PossiblySensitive,
    /// A non-password field whose Text range reports a mixed read-only
    /// attribute: keys are published with unknown typing (D-4c).
    TypingUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiaElement {
    pub name: Option<Metadata>,
    pub automation_id: Option<Metadata>,
    pub class: Option<Metadata>,
    pub framework: Option<Metadata>,
    pub control_type: Option<i32>,
    pub bounds: Option<PhysicalRect>,
    pub ancestor_names: BoundedList<Metadata, MAX_UIA_ANCESTORS>,
    pub password: Option<bool>,
    pub editable: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiaEvidence {
    pub target: TextClassification,
    pub element: UiaElement,
    pub dpi: Option<NonZeroU32>,
    pub dpi_awareness: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadUnavailable {
    TimedOut,
    Hung,
    Unavailable,
    Stale,
    OutsideApproval,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadOutcome<T> {
    Observed(T),
    Unavailable(ReadUnavailable),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadEvidence<T> {
    pub binding: ReadBinding,
    pub started_qpc: Option<QpcTick>,
    pub completed_qpc: Option<QpcTick>,
    pub outcome: ReadOutcome<T>,
}

impl<T> ReadEvidence<T> {
    fn source(&self) -> (VisitBinding, QpcTick) {
        (self.binding.visit, self.completed_qpc.unwrap_or(self.binding.requested_qpc))
    }
}

/// COM callback parts are combined natively. Keep the original UTF-16 limit
/// after conversion and independently bound the allocation during decoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CommitText(BoundedText<MAX_COMMIT_BYTES>);

impl CommitText {
    pub fn new(text: &str) -> Result<Self, PayloadLimit> {
        if text.encode_utf16().take(MAX_COMMIT_UTF16 + 1).count() > MAX_COMMIT_UTF16 {
            return Err(PayloadLimit(MAX_COMMIT_UTF16));
        }
        BoundedText::new(text).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> Deserialize<'de> for CommitText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = BoundedText::<MAX_COMMIT_BYTES>::deserialize(deserializer)?;
        if text.as_str().encode_utf16().count() > MAX_COMMIT_UTF16 {
            return Err(serde::de::Error::custom(PayloadLimit(MAX_COMMIT_UTF16)));
        }
        Ok(Self(text))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitEvidence {
    pub binding: ReadBinding,
    pub receipt_qpc: QpcTick,
    pub text: CommitText,
    /// False for invalid UTF-16; `text` then carries U+FFFD replacements.
    pub valid: bool,
}

/// Input payloads reuse the ordinary evidence and exclusive producer progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputEvidence {
    Input(CollectedInput),
    DeviceArrived { receipt: Receipt, descriptor: DeviceDescriptor },
    DeviceRemoved { receipt: Receipt, id: DeviceId },
    Environment(EnvironmentState),
    Progress(ProducerProgress),
    Gap(ProducerGap),
    /// The registration check no longer finds an optional collection (E3).
    OptionalUnavailable { collection: OptionalCollection, observed_qpc: QpcTick },
}

impl InputEvidence {
    /// The producer whose sequence the record belongs to. Environment states
    /// and optional-collection notices belong to no producer.
    pub fn producer(&self) -> Option<Producer> {
        match self {
            Self::Input(input) => Some(input.sample.producer()),
            Self::DeviceArrived { .. } | Self::DeviceRemoved { .. } => Some(Producer::DeviceNotifications),
            Self::Progress(progress) => Some(progress.producer),
            Self::Gap(gap) => Some(gap.producer),
            Self::Environment(_) | Self::OptionalUnavailable { .. } => None,
        }
    }

    /// The earliest receipt the record covers: a gap's first one, otherwise
    /// its only one.
    pub fn first_qpc(&self) -> QpcTick {
        match self {
            Self::Gap(gap) => gap.first_qpc,
            _ => self.last_qpc(),
        }
    }

    /// The latest receipt the record covers.
    pub fn last_qpc(&self) -> QpcTick {
        match self {
            Self::Input(input) => input.receipt.qpc,
            Self::DeviceArrived { receipt, .. } | Self::DeviceRemoved { receipt, .. } => receipt.qpc,
            Self::Environment(state) => state.observed_qpc,
            Self::OptionalUnavailable { observed_qpc, .. } => *observed_qpc,
            Self::Progress(progress) => progress.before_qpc,
            Self::Gap(gap) => gap.last_qpc,
        }
    }
}

impl From<InputEvidence> for HelperToCore {
    fn from(evidence: InputEvidence) -> Self {
        match evidence {
            InputEvidence::Input(input) => Self::Input(input),
            InputEvidence::DeviceArrived { receipt, descriptor } => Self::DeviceArrived { receipt, descriptor },
            InputEvidence::DeviceRemoved { receipt, id } => Self::DeviceRemoved { receipt, id },
            InputEvidence::Environment(state) => Self::Environment(state),
            InputEvidence::Progress(progress) => Self::Progress(progress),
            InputEvidence::Gap(gap) => Self::Gap(gap),
            InputEvidence::OptionalUnavailable { collection, observed_qpc } => Self::OptionalUnavailable { collection, observed_qpc },
        }
    }
}

/// The helper's own lifecycle records are not evidence and are returned.
impl TryFrom<HelperToCore> for InputEvidence {
    type Error = HelperToCore;

    fn try_from(record: HelperToCore) -> Result<Self, HelperToCore> {
        Ok(match record {
            HelperToCore::Input(input) => Self::Input(input),
            HelperToCore::DeviceArrived { receipt, descriptor } => Self::DeviceArrived { receipt, descriptor },
            HelperToCore::DeviceRemoved { receipt, id } => Self::DeviceRemoved { receipt, id },
            HelperToCore::Environment(state) => Self::Environment(state),
            HelperToCore::Progress(progress) => Self::Progress(progress),
            HelperToCore::Gap(gap) => Self::Gap(gap),
            HelperToCore::OptionalUnavailable { collection, observed_qpc } => Self::OptionalUnavailable { collection, observed_qpc },
            HelperToCore::Ready(_) | HelperToCore::Failed(_) => return Err(record),
        })
    }
}

/// A fixed observation kind selects the local native algorithm. The source
/// independently resolves and verifies the target carried by the binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "binding", rename_all = "snake_case", deny_unknown_fields)]
pub enum Query {
    Focus { read: ReadBinding, expected: FocusAtReceipt },
    Pointer { read: ReadBinding, point: PointBinding },
    Window(ReadBinding),
    NativeFocus(ReadBinding),
    Hung(ReadBinding),
    ImeFocus { read: ReadBinding, expected: FocusAtReceipt },
    ImeKey { read: ReadBinding, expected: FocusAtReceipt, key: RequestId, key_qpc: QpcTick },
    Held(ReadBinding),
    ClipboardFence(ReadBinding),
}

impl Query {
    pub fn binding(self) -> ReadBinding {
        match self {
            Self::Focus { read, .. } | Self::Pointer { read, .. }
            | Self::ImeFocus { read, .. } | Self::ImeKey { read, .. }
            | Self::Window(read) | Self::NativeFocus(read) | Self::Hung(read)
            | Self::Held(read) | Self::ClipboardFence(read) => read,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeObservation<T> {
    pub visit: VisitBinding,
    pub first_qpc: QpcTick,
    pub observed_qpc: QpcTick,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextEvidence {
    Notice(NativeObservation<WindowNotice>),
    Window(ReadEvidence<WindowEvidence>),
    NativeFocus(ReadEvidence<FocusEvidence>),
    HungRead(ReadEvidence<HungEvidence>),
    FocusUia(ReadEvidence<UiaEvidence>),
    PointerUia { point: PointBinding, read: ReadEvidence<UiaEvidence> },
    Commit(CommitEvidence),
    ImeFocus(ReadEvidence<ImeState>),
    ImeKey { key: RequestId, key_qpc: QpcTick, read: ReadEvidence<ImeState> },
    Held(ReadEvidence<HeldSnapshot>),
    Rejected(ReadEvidence<()>),
    ClipboardFence(ReadEvidence<()>),
}

impl ContextEvidence {
    pub fn source(&self) -> (VisitBinding, QpcTick) {
        match self {
            Self::Notice(notice) => (notice.visit, notice.observed_qpc),
            Self::Commit(commit) => (commit.binding.visit, commit.receipt_qpc),
            Self::Window(read) => read.source(),
            Self::NativeFocus(read) => read.source(),
            Self::HungRead(read) => read.source(),
            Self::FocusUia(read) | Self::PointerUia { read, .. } => read.source(),
            Self::ImeFocus(read) | Self::ImeKey { read, .. } => read.source(),
            Self::Held(read) => read.source(),
            Self::Rejected(read) | Self::ClipboardFence(read) => read.source(),
        }
    }

    pub fn read_binding(&self) -> Option<ReadBinding> {
        Some(match self {
            Self::Window(read) => read.binding,
            Self::NativeFocus(read) => read.binding,
            Self::HungRead(read) => read.binding,
            Self::FocusUia(read) | Self::PointerUia { read, .. } => read.binding,
            Self::ImeFocus(read) | Self::ImeKey { read, .. } => read.binding,
            Self::Held(read) => read.binding,
            Self::Rejected(read) | Self::ClipboardFence(read) => read.binding,
            Self::Notice(_) | Self::Commit(_) => return None,
        })
    }
}

/// Keeps native receipt and progress timing inside the originating grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedInput {
    pub visit: VisitBinding,
    pub evidence: InputEvidence,
}

pub mod attachment;
pub mod runtime;
