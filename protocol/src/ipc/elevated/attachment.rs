//! The one-shot attachment state machine. Capture records have no transition here.

use std::num::NonZeroU64;

use crate::{AttemptId, QpcFrequency, SessionId};

/// The elevated wire version, checked at attachment so an incompatible
/// collector is rejected before admission. Version 2: five input producers
/// with `OptionalHid` (stage I5), `HeldSnapshot.keys` (stage I4),
/// `CommitEvidence.valid` (stage I7), `InputEvidence::OptionalUnavailable`
/// and `ContextGap.cause` (stage E3).
pub const VERSION: u16 = 2;
pub const HEADER_BYTES: usize = 40;
pub const MAX_FRAME_BYTES: usize = 65_536;
const MAGIC: [u8; 4] = *b"SFEL";
const ID_BYTES: usize = 65;
pub const SCOPE_BYTES: usize = 2 * ID_BYTES + 16 + 8;
pub const CHALLENGE_BYTES: usize = 32;
pub const PROOF_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAttachment;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Control = 1,
    Input = 2,
    Context = 3,
}

impl Role {
    pub const ORDER: [Self; 3] = [Self::Control, Self::Input, Self::Context];

    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Control => ".control",
            Self::Input => ".input",
            Self::Context => ".context",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Challenge = 1,
    Proof = 2,
    Seal = 3,
    Authenticated = 4,
}

impl Transition {
    pub const fn payload_len(self) -> usize {
        match self {
            Self::Challenge => SCOPE_BYTES + CHALLENGE_BYTES,
            Self::Proof => PROOF_BYTES,
            Self::Seal | Self::Authenticated => SCOPE_BYTES,
        }
    }

    const fn direction(self) -> u8 {
        match self {
            Self::Challenge | Self::Seal => 1,
            Self::Proof | Self::Authenticated => 2,
        }
    }

    const fn sequence(self) -> u64 {
        match self {
            Self::Challenge | Self::Proof => 0,
            Self::Seal | Self::Authenticated => 1,
        }
    }

    const fn valid_for(self, role: Role) -> bool {
        matches!(self, Self::Challenge | Self::Proof) || matches!(role, Role::Control)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionId([u8; 16]);

impl ConnectionId {
    pub fn new(bytes: [u8; 16]) -> Result<Self, InvalidAttachment> {
        if bytes == [0; 16] { return Err(InvalidAttachment) }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(self) -> [u8; 16] { self.0 }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub attempt: AttemptId,
    pub session: SessionId,
    pub release: [u8; 16],
    pub qpc_frequency: QpcFrequency,
}

impl Scope {
    pub fn encode(&self) -> [u8; SCOPE_BYTES] {
        let mut bytes = [0u8; SCOPE_BYTES];
        encode_id(&mut bytes[..ID_BYTES], self.attempt.as_str());
        encode_id(&mut bytes[ID_BYTES..2 * ID_BYTES], self.session.as_str());
        bytes[2 * ID_BYTES..2 * ID_BYTES + 16].copy_from_slice(&self.release);
        bytes[2 * ID_BYTES + 16..].copy_from_slice(&self.qpc_frequency.0.get().to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, InvalidAttachment> {
        let bytes: &[u8; SCOPE_BYTES] = bytes.try_into().map_err(|_| InvalidAttachment)?;
        let attempt = AttemptId::new(decode_id(&bytes[..ID_BYTES])?).map_err(|_| InvalidAttachment)?;
        let session = SessionId::new(decode_id(&bytes[ID_BYTES..2 * ID_BYTES])?).map_err(|_| InvalidAttachment)?;
        let release = bytes[2 * ID_BYTES..2 * ID_BYTES + 16].try_into().map_err(|_| InvalidAttachment)?;
        if release == [0; 16] { return Err(InvalidAttachment) }
        let frequency = u64::from_le_bytes(bytes[2 * ID_BYTES + 16..].try_into().map_err(|_| InvalidAttachment)?);
        let qpc_frequency = QpcFrequency(NonZeroU64::new(frequency).ok_or(InvalidAttachment)?);
        Ok(Self { attempt, session, release, qpc_frequency })
    }
}

fn encode_id(field: &mut [u8], id: &str) {
    field[0] = id.len() as u8;
    field[1..=id.len()].copy_from_slice(id.as_bytes());
}

fn decode_id(field: &[u8]) -> Result<&str, InvalidAttachment> {
    let length = usize::from(field[0]);
    if length == 0 || length >= field.len() || field[1 + length..].iter().any(|&byte| byte != 0) {
        return Err(InvalidAttachment);
    }
    std::str::from_utf8(&field[1..1 + length]).map_err(|_| InvalidAttachment)
}

pub fn header(
    role: Role, transition: Transition, connection: ConnectionId,
) -> Result<[u8; HEADER_BYTES], InvalidAttachment> {
    let payload = transition.payload_len();
    let cap = match role {
        Role::Input => crate::ipc::MAX_PACKET_BYTES,
        Role::Control | Role::Context => MAX_FRAME_BYTES - HEADER_BYTES,
    };
    if payload > cap || !transition.valid_for(role) {
        return Err(InvalidAttachment);
    }
    let mut bytes = [0; HEADER_BYTES];
    bytes[..4].copy_from_slice(&MAGIC);
    bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
    bytes[6] = role as u8;
    bytes[7] = transition as u8;
    bytes[8] = transition.direction();
    bytes[12..16].copy_from_slice(&(payload as u32).to_le_bytes());
    bytes[16..24].copy_from_slice(&transition.sequence().to_le_bytes());
    bytes[24..].copy_from_slice(&connection.as_bytes());
    Ok(bytes)
}

pub fn check_header(
    bytes: &[u8], role: Role, transition: Transition, connection: ConnectionId,
) -> Result<(), InvalidAttachment> {
    if bytes != header(role, transition, connection)?.as_slice() {
        return Err(InvalidAttachment);
    }
    Ok(())
}
