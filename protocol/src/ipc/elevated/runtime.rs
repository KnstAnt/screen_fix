//! Runtime records after all three attachment lanes have authenticated.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{AdmissionEpoch, AttemptId, BoundedText, GeometryId, QpcTick, RequestId, SessionId};
use crate::ipc::{
    FocusAtReceipt, HelperFailure, InputRegistration, PacketError, ProducerGap, validate_input,
};

use super::{
    ContextEvidence, GrantedInput, ImagePath, InputEvidence, Query, ReadBinding, ReadEvidence, ReadOutcome,
    ReadUnavailable, VisitBinding, WindowInstance,
    attachment::{ConnectionId, FrameHeader, HEADER_BYTES, MAX_FRAME_BYTES, Role, Scope},
};

pub const MAX_APPROVED_EXECUTABLES: usize = 64;
/// Byte bound of one approved executable identity (`ApprovalKey`).
pub const MAX_APPROVAL_KEY_BYTES: usize = 4096;
pub type ApprovalKey = BoundedText<MAX_APPROVAL_KEY_BYTES>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityClaim {
    pub target: WindowInstance,
    pub approval: ApprovalKey,
    pub image: ImagePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRequest {
    pub foreground: IdentityClaim,
    pub visit: VisitBinding,
    pub geometry: GeometryId,
    pub lease: NonZeroU64,
    pub expires_qpc: QpcTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub sequence: NonZeroU64,
    pub epoch: Option<AdmissionEpoch>,
    pub controller_qpc: QpcTick,
    pub pump_qpc: QpcTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub visit: VisitBinding,
    pub lease: NonZeroU64,
    pub opening_qpc: QpcTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRevision {
    pub sequence: NonZeroU64,
    pub changed_qpc: QpcTick,
    pub change: crate::ipc::EnvironmentChange,
    pub cleanup_deadline_qpc: Option<QpcTick>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Configure { approved_count: u8, pen_hid: bool },
    Approval(ApprovalKey),
    SealConfiguration,
    Renew(Lease),
    Prepare(GrantRequest),
    Activate(Activation),
    RefreshFocus { previous: VisitBinding, next: VisitBinding, lease: NonZeroU64 },
    AcceptResync { visit: VisitBinding, request: RequestId, completed_qpc: QpcTick },
    Revoke { visit: VisitBinding, before_qpc: QpcTick },
    Environment(EnvironmentRevision),
    Stop { before_qpc: QpcTick, cleanup_deadline_qpc: Option<QpcTick> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Revocation {
    Requested,
    Foreground,
    Focus,
    Environment,
    Expired,
    WitnessLost,
    LateNotice,
    RequiredAccess,
    NativeFailure,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldReadiness {
    Known,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageReason {
    Opening,
    HeldUnknown,
    NativeGap,
    UncertifiedSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageLoss {
    pub first_qpc: QpcTick,
    pub before_qpc: QpcTick,
    pub reason: CoverageReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Prepare,
    Activate,
    RefreshFocus,
    AcceptResync,
    Revoke,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum Status {
    Configured,
    Witness { revision: u64, focus_revision: u64, safe: bool, observed_qpc: QpcTick },
    Armed {
        visit: VisitBinding,
        arm_qpc: QpcTick,
        registration: InputRegistration,
        focus: FocusAtReceipt,
        held: HeldReadiness,
    },
    Activated { visit: VisitBinding, activation_qpc: QpcTick },
    FocusSuspended { visit: VisitBinding, before_qpc: QpcTick },
    ResyncAccepted {
        visit: VisitBinding, after_qpc: QpcTick, held: HeldReadiness, input_coverage: CoverageLoss,
    },
    ResyncRequired {
        previous: VisitBinding, visit: VisitBinding, first_uncovered: QpcTick,
        resync_after_qpc: QpcTick, cause: CoverageReason, native_gap: Option<ProducerGap>,
    },
    TransitionRejected { visit: VisitBinding, transition: Transition, reason: ReadUnavailable },
    ReadRejected { read: ReadBinding, reason: ReadUnavailable },
    FocusRefreshed { visit: VisitBinding, focus: FocusAtReceipt, resync_after_qpc: QpcTick },
    Invalidated { visit: VisitBinding, reason: Revocation, before_qpc: QpcTick },
    Failed(HelperFailure),
    Closed { visit: VisitBinding, before_qpc: QpcTick },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextCommand {
    Read { query: Query, secondary: Option<IdentityClaim> },
    Cancel { visit: VisitBinding, request: RequestId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextProducer {
    Notices,
    NativeRead,
    Automation,
    Ime,
    TextEdit,
}

impl ContextProducer {
    /// Every producer, ordered so that `ALL[p.index()] == p`.
    pub const ALL: [Self; 5] = [Self::Notices, Self::NativeRead, Self::Automation, Self::Ime, Self::TextEdit];

    /// Dense index for per-producer state sized by `ALL`.
    pub const fn index(self) -> usize {
        match self {
            Self::Notices => 0,
            Self::NativeRead => 1,
            Self::Automation => 2,
            Self::Ime => 3,
            Self::TextEdit => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextGap {
    pub visit: VisitBinding,
    pub producer: ContextProducer,
    pub cause: ContextGapCause,
    pub first_position: NonZeroU64,
    pub last_position: NonZeroU64,
    pub first_qpc: QpcTick,
    pub last_qpc: QpcTick,
}

impl ContextGap {
    /// The number of undelivered positions, at least one.
    pub fn count(&self) -> u64 {
        self.last_position.get().saturating_sub(self.first_position.get()).saturating_add(1)
    }
}

/// Why context positions were not delivered (E3). TextEdit commits are
/// counted per reason; a sender or foreground mismatch carries no detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextGapCause {
    /// A bounded queue (the outbound lane or the commit slots) had no room.
    Overflow,
    /// The item's visit, focus or edit revision was no longer current.
    Stale,
    /// A finalized commit exceeded the bounded UTF-16 length.
    CommitTooLong,
    /// A finalized commit had too many parts or an unreadable part array.
    CommitParts,
    /// The commit sender, foreground or callback deadline check failed.
    CommitRejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    Item { producer: ContextProducer, position: NonZeroU64, evidence: ContextEvidence },
    Gap(ContextGap),
    /// Exclusive receipt coverage associated with an independent local
    /// witness sample. A late source event can invalidate this prefix.
    Progress { visit: VisitBinding, before_qpc: QpcTick, witness_qpc: QpcTick },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scoped<T> {
    attempt: AttemptId,
    session: SessionId,
    record: T,
}

mod sealed {
    pub trait Sealed {}
}

/// Each type has one direction and lane. The binding sequence is followed
/// directly by the runtime sequence on both sides.
pub trait Record: sealed::Sealed + Serialize + DeserializeOwned {
    const ROLE: Role;
    const DIRECTION: u8;
    const FIRST_SEQUENCE: u64;
    fn kind(&self) -> u8;
    fn valid_kind(kind: u8) -> bool;
    fn valid(&self) -> bool;
}

macro_rules! records {
    ($type:ty, $role:ident, $direction:literal, $first:literal, $validator:path, { $($kind:literal => $pattern:pat),+ }) => {
        impl sealed::Sealed for $type {}
        impl Record for $type {
            const ROLE: Role = Role::$role;
            const DIRECTION: u8 = $direction;
            const FIRST_SEQUENCE: u64 = $first;

            fn kind(&self) -> u8 {
                match self { $($pattern => $kind),+ }
            }

            fn valid_kind(kind: u8) -> bool {
                matches!(kind, $($kind)|+)
            }

            fn valid(&self) -> bool {
                $validator(self)
            }
        }
    };
}

records!(Control, Control, 1, 2, control_valid, {
    16 => Control::Configure { .. }, 17 => Control::Approval(_), 18 => Control::SealConfiguration,
    19 => Control::Renew(_), 20 => Control::Prepare(_), 21 => Control::Activate(_),
    22 => Control::Revoke { .. }, 23 => Control::Environment(_), 24 => Control::Stop { .. }, 25 => Control::AcceptResync { .. }, 26 => Control::RefreshFocus { .. }
});
records!(Status, Control, 2, 2, status_valid, {
    16 => Status::Configured, 17 => Status::Armed { .. }, 18 => Status::Activated { .. },
    19 => Status::Invalidated { .. }, 20 => Status::Failed(_), 21 => Status::Closed { .. },
    22 => Status::Stopped, 23 => Status::Witness { .. }, 24 => Status::FocusSuspended { .. }, 25 => Status::FocusRefreshed { .. }, 26 => Status::ResyncAccepted { .. }, 27 => Status::TransitionRejected { .. }, 28 => Status::ReadRejected { .. }, 29 => Status::ResyncRequired { .. }
});
records!(ContextCommand, Context, 1, 1, query_valid, {
    16 => ContextCommand::Read { .. }, 17 => ContextCommand::Cancel { .. }
});
records!(Observation, Context, 2, 1, observation_valid, {
    16 => Observation::Item { .. }, 17 => Observation::Gap(_), 18 => Observation::Progress { .. }
});
records!(GrantedInput, Input, 2, 1, input_valid, { 16 => GrantedInput { .. } });

fn tick(tick: QpcTick) -> bool { tick.0 > 0 }

fn window_valid(window: WindowInstance) -> bool {
    window.window.0 != 0 && window.process.creation_filetime != 0
}

fn visit_valid(visit: VisitBinding) -> bool {
    visit.epoch.0 != 0 && window_valid(visit.foreground)
        && visit.witness_revision != 0 && visit.focus_revision != 0 && visit.focus_generation.0 != 0
}

fn binding_valid(read: ReadBinding) -> bool {
    visit_valid(read.visit) && window_valid(read.target)
        && read.request.0 != 0 && tick(read.requested_qpc) && read.deadline_qpc > read.requested_qpc
}

fn claim_valid(claim: &IdentityClaim) -> bool {
    window_valid(claim.target)
        && !claim.approval.as_str().trim().is_empty() && !claim.approval.as_str().contains('\0')
        && !claim.image.as_str().trim().is_empty() && !claim.image.as_str().contains('\0')
}

fn control_valid(command: &Control) -> bool {
    match command {
        Control::Configure { approved_count, .. } =>
            (1..=MAX_APPROVED_EXECUTABLES).contains(&usize::from(*approved_count)),
        Control::Approval(key) => !key.as_str().trim().is_empty() && !key.as_str().contains('\0'),
        Control::SealConfiguration => true,
        Control::Renew(lease) => tick(lease.controller_qpc) && tick(lease.pump_qpc)
            && lease.epoch.is_none_or(|epoch| epoch.0 != 0),
        Control::Prepare(request) => visit_valid(request.visit) && claim_valid(&request.foreground)
            && request.foreground.target == request.visit.foreground && tick(request.expires_qpc),
        Control::Activate(activation) => visit_valid(activation.visit) && tick(activation.opening_qpc),
        Control::RefreshFocus { previous, next, .. } => visit_valid(*previous) && visit_valid(*next),
        Control::AcceptResync { visit, request, completed_qpc } => visit_valid(*visit) && request.0 != 0 && tick(*completed_qpc),
        Control::Revoke { visit, before_qpc } => visit_valid(*visit) && tick(*before_qpc),
        Control::Environment(revision) => tick(revision.changed_qpc) && revision.cleanup_deadline_qpc.is_none_or(tick),
        Control::Stop { before_qpc, cleanup_deadline_qpc } => tick(*before_qpc) && cleanup_deadline_qpc.is_none_or(tick),
    }
}

fn status_valid(status: &Status) -> bool {
    match status {
        Status::Failed(failure) => failure.valid(),
        Status::Configured | Status::Stopped => true,
        Status::Witness { revision, focus_revision, observed_qpc, .. } =>
            *revision != 0 && *focus_revision != 0 && tick(*observed_qpc),
        Status::Armed { visit, arm_qpc, registration, focus, .. } =>
            visit_valid(*visit) && tick(*arm_qpc) && focus.window.0 != 0 && registration.required_ready(),
        Status::Activated { visit, activation_qpc } => visit_valid(*visit) && tick(*activation_qpc),
        Status::FocusSuspended { visit, before_qpc } | Status::Invalidated { visit, before_qpc, .. }
        | Status::Closed { visit, before_qpc } => visit_valid(*visit) && tick(*before_qpc),
        Status::ResyncAccepted { visit, after_qpc, input_coverage, .. } =>
            visit_valid(*visit) && tick(input_coverage.first_qpc) && input_coverage.first_qpc <= *after_qpc
                && *after_qpc <= input_coverage.before_qpc,
        Status::ResyncRequired { previous, visit, first_uncovered, resync_after_qpc, native_gap, .. } =>
            visit_valid(*previous) && visit_valid(*visit) && tick(*first_uncovered)
                && *first_uncovered <= *resync_after_qpc && native_gap.is_none_or(gap_valid),
        Status::TransitionRejected { visit, .. } => visit_valid(*visit),
        Status::ReadRejected { read, .. } => binding_valid(*read),
        Status::FocusRefreshed { visit, focus, resync_after_qpc } =>
            visit_valid(*visit) && focus.window.0 != 0 && tick(*resync_after_qpc),
    }
}

fn query_valid(command: &ContextCommand) -> bool {
    match command {
        ContextCommand::Read { query, secondary } => {
            let read = query.binding();
            if !binding_valid(read) { return false; }
            match query {
                Query::Pointer { point, .. } => window_valid(point.root) && point.action_qpc.is_none_or(|at| tick(at) && at <= read.requested_qpc)
                    && secondary.as_ref().is_none_or(|claim| claim_valid(claim) && claim.target == read.target),
                Query::Window(_) | Query::Hung(_) => secondary.as_ref().is_none_or(|claim| claim_valid(claim) && claim.target == read.target),
                Query::ImeKey { key, expected, key_qpc, .. } => secondary.is_none() && key.0 != 0
                    && expected.window.0 != 0 && tick(*key_qpc) && *key_qpc <= read.requested_qpc,
                Query::Focus { expected, .. } | Query::ImeFocus { expected, .. } =>
                    secondary.is_none() && expected.window.0 != 0,
                _ => secondary.is_none(),
            }
        }
        ContextCommand::Cancel { visit, request } => visit_valid(*visit) && request.0 != 0,
    }
}

fn gap_valid(gap: ProducerGap) -> bool {
    gap.first_sequence.0 != 0 && gap.first_sequence <= gap.last_sequence
        && tick(gap.first_qpc) && gap.first_qpc <= gap.last_qpc
}

fn input_valid(input: &GrantedInput) -> bool {
    visit_valid(input.visit) && match &input.evidence {
        InputEvidence::Input(sample) => tick(sample.receipt.qpc) && sample.receipt.sequence.0 != 0
            && validate_input(sample).is_ok(),
        InputEvidence::DeviceArrived { receipt, descriptor } =>
            tick(receipt.qpc) && receipt.sequence.0 != 0 && descriptor.id.0 != 0,
        InputEvidence::DeviceRemoved { receipt, id } =>
            tick(receipt.qpc) && receipt.sequence.0 != 0 && id.0 != 0,
        InputEvidence::Environment(state) => tick(state.observed_qpc) && state.valid(),
        InputEvidence::Progress(progress) => tick(progress.before_qpc),
        InputEvidence::Gap(gap) => gap_valid(*gap),
        InputEvidence::OptionalUnavailable { observed_qpc, .. } => tick(*observed_qpc),
    }
}

fn read_valid<T>(read: &ReadEvidence<T>) -> bool {
    binding_valid(read.binding)
        && read.started_qpc.is_none_or(|start| start >= read.binding.requested_qpc)
        && read.completed_qpc.is_none_or(|end| end >= read.started_qpc.unwrap_or(read.binding.requested_qpc))
        && (!matches!(&read.outcome, ReadOutcome::Observed(_)) || matches!(
            (read.started_qpc, read.completed_qpc),
            (Some(_), Some(end)) if end < read.binding.deadline_qpc
        ))
}

fn observation_valid(observation: &Observation) -> bool {
    match observation {
        Observation::Item { producer, evidence, .. } => match evidence {
            ContextEvidence::Notice(notice) => *producer == ContextProducer::Notices && visit_valid(notice.visit)
                && tick(notice.first_qpc) && notice.first_qpc <= notice.observed_qpc
                && notice.observed_qpc == notice.value.receipt && notice.value.window.0 != 0,
            ContextEvidence::Window(read) => *producer == ContextProducer::NativeRead && read_valid(read),
            ContextEvidence::NativeFocus(read) => *producer == ContextProducer::NativeRead && read_valid(read),
            ContextEvidence::HungRead(read) => *producer == ContextProducer::NativeRead && read_valid(read),
            ContextEvidence::FocusUia(read) => *producer == ContextProducer::Automation && read_valid(read),
            ContextEvidence::PointerUia { point, read } => *producer == ContextProducer::Automation
                && read_valid(read) && window_valid(point.root)
                && point.action_qpc.is_none_or(|at| tick(at) && at <= read.binding.requested_qpc),
            ContextEvidence::ImeFocus(read) => *producer == ContextProducer::Ime && read_valid(read),
            ContextEvidence::ImeKey { key, key_qpc, read } => *producer == ContextProducer::Ime && key.0 != 0
                && tick(*key_qpc) && *key_qpc <= read.binding.requested_qpc && read_valid(read),
            ContextEvidence::Commit(commit) => *producer == ContextProducer::TextEdit
                && binding_valid(commit.binding) && commit.receipt_qpc >= commit.binding.requested_qpc
                && !commit.text.as_str().is_empty(),
            ContextEvidence::Held(read) => *producer == ContextProducer::NativeRead && read_valid(read),
            ContextEvidence::ClipboardFence(read) => *producer == ContextProducer::NativeRead && read_valid(read),
            ContextEvidence::Rejected(read) => matches!(producer, ContextProducer::NativeRead | ContextProducer::Automation | ContextProducer::Ime)
                && matches!(&read.outcome, ReadOutcome::Unavailable(_)) && read_valid(read),
        },
        Observation::Gap(gap) => visit_valid(gap.visit) && gap.first_position <= gap.last_position
            && tick(gap.first_qpc) && gap.first_qpc <= gap.last_qpc,
        Observation::Progress { visit, before_qpc, witness_qpc } =>
            visit_valid(*visit) && tick(*before_qpc) && before_qpc <= witness_qpc,
    }
}

const MAGIC: [u8; 4] = *b"SFRT";
const VERSION: u16 = 3;

pub const fn capacity<T: Record>() -> usize {
    match T::ROLE {
        Role::Input => HEADER_BYTES + crate::ipc::MAX_PACKET_BYTES,
        Role::Control | Role::Context => MAX_FRAME_BYTES,
    }
}

/// Validate the header before any sized byte-mode payload read.
pub fn payload_len<T: Record>(
    header: &[u8], connection: ConnectionId, sequence: u64,
) -> Result<usize, PacketError> {
    let bytes: &[u8; HEADER_BYTES] = header.try_into().map_err(|_| PacketError::HeaderTooShort)?;
    let header = FrameHeader::parse(bytes);
    if header.magic != MAGIC {
        return Err(PacketError::InvalidMagic);
    }
    if header.version != VERSION {
        return Err(PacketError::UnsupportedVersion(header.version));
    }
    if header.role != T::ROLE as u8 || header.direction != T::DIRECTION {
        return Err(PacketError::WrongDirection);
    }
    if !FrameHeader::reserved_clear(bytes) {
        return Err(PacketError::ReservedHeaderByte);
    }
    if !T::valid_kind(header.kind) || header.sequence != sequence
        || sequence < T::FIRST_SEQUENCE || sequence == u64::MAX
        || header.connection != connection.as_bytes()
    {
        return Err(PacketError::InvalidPayload);
    }
    let length = header.length as usize;
    if !(1..=capacity::<T>() - HEADER_BYTES).contains(&length) {
        return Err(PacketError::InvalidLength);
    }
    Ok(length)
}

pub fn encode<T: Record>(
    record: &T, scope: &Scope, connection: ConnectionId, sequence: u64, output: &mut [u8],
) -> Result<usize, PacketError> {
    if !record.valid() { return Err(PacketError::InvalidPayload); }
    let end = output.len().min(capacity::<T>());
    let mut payload = output.get_mut(HEADER_BYTES..end).ok_or(PacketError::OutputTooSmall)?;
    let scoped = Scoped { attempt: scope.attempt.clone(), session: scope.session.clone(), record };
    // These are bounded IPC schemas, separate from the Recorder's journal format.
    serde_json::to_writer(&mut payload, &scoped).map_err(|error| {
        if error.io_error_kind() == Some(std::io::ErrorKind::WriteZero) {
            PacketError::OutputTooSmall
        } else {
            PacketError::InvalidPayload
        }
    })?;
    let length = end - HEADER_BYTES - payload.len();
    let header = FrameHeader {
        magic: MAGIC, version: VERSION, role: T::ROLE as u8, kind: record.kind(), direction: T::DIRECTION,
        length: length as u32, sequence, connection: connection.as_bytes(),
    }.write();
    output[..HEADER_BYTES].copy_from_slice(&header);
    payload_len::<T>(&header, connection, sequence)?;
    Ok(HEADER_BYTES + length)
}

pub fn decode<T: Record>(
    frame: &[u8], scope: &Scope, connection: ConnectionId, sequence: u64,
) -> Result<T, PacketError> {
    let header: &[u8; HEADER_BYTES] = frame.get(..HEADER_BYTES).and_then(|header| header.try_into().ok())
        .ok_or(PacketError::HeaderTooShort)?;
    let length = payload_len::<T>(header, connection, sequence)?;
    let kind = FrameHeader::parse(header).kind;
    if frame.len() != HEADER_BYTES + length {
        return Err(PacketError::InvalidLength);
    }
    let payload = &frame[HEADER_BYTES..];
    let mut records = serde_json::Deserializer::from_slice(payload).into_iter::<Scoped<T>>();
    let scoped = records.next().ok_or(PacketError::InvalidPayload)?.map_err(|_| PacketError::InvalidPayload)?;
    // A foreign scope or a kind that contradicts the header is a protocol
    // violation, never a skippable record.
    if scoped.attempt != scope.attempt || scoped.session != scope.session || scoped.record.kind() != kind {
        return Err(PacketError::InvalidRecord("The record scope or kind contradicts its frame"));
    }
    if records.byte_offset() != payload.len() || !scoped.record.valid() {
        return Err(PacketError::InvalidPayload);
    }
    Ok(scoped.record)
}
