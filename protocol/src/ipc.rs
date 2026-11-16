//! Bounded record format for the trusted, launched-child input channel.
//!
//! Every packet starts with a four-byte magic, a little-endian u16 version,
//! one direction byte, a zero reserved byte, and a little-endian u32 length.
//! The remainder is one typed payload. Ring ownership and signaling belong to P06.

use std::{fmt, num::NonZeroU32};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    AttemptId, BoundedList, BoundedText, QpcFrequency, DeviceId, MessageTickMs, PhysicalPoint,
    PhysicalRect, QpcTick, SessionId, SourceSequence, WindowId,
};

pub const SCHEMA_VERSION: u16 = 6;
pub const MAX_PACKET_BYTES: usize = 8192;
pub const MAX_HID_REPORT_BYTES: usize = 1024;
pub const MAX_HID_FIELDS: usize = 32;
pub const DEVICE_PATH_HASH_BYTES: usize = 64;

const MAGIC: [u8; 4] = *b"SFIN";
const HEADER_BYTES: usize = 12;
const TO_CORE: u8 = 1;
const TO_HELPER: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureInput {
    pub session: SessionId,
    pub attempt: AttemptId,
    pub qpc_frequency: QpcFrequency,
    pub pen_hid: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentChange {
    Display,
    Session,
    Settings,
    SessionEnding,
}

impl EnvironmentChange {
    pub const fn bit(self) -> u32 {
        match self {
            Self::Display => 1,
            Self::Session => 2,
            Self::Settings => 4,
            Self::SessionEnding => 8,
        }
    }

    pub const ALL_BITS: u32 = Self::Display.bit() | Self::Session.bit()
        | Self::Settings.bit() | Self::SessionEnding.bit();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentState {
    pub observed_qpc: QpcTick,
    pub changes: u32,
    pub swapped_buttons: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreToHelper {
    Configure(ConfigureInput),
    EnvironmentChanged(EnvironmentChange),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Producer {
    RawInput,
    KeyboardHook,
    PointerHook,
    DeviceNotifications,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub sequence: SourceSequence,
    pub qpc: QpcTick,
    pub message_tick_ms: Option<MessageTickMs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRegistration {
    pub raw_keyboard: bool,
    pub raw_pointer: bool,
    pub keyboard_hook: bool,
    pub pointer_hook: bool,
    pub pen_hid: bool,
    pub hook_timeout: HookTimeoutEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTimeoutEvidence {
    Absent,
    Unavailable,
    ConfiguredMs(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperFailureStage {
    Startup,
    PowerThrottling,
    ClockMismatch,
    RawInputRegistration,
    DeviceDiscovery,
    KeyboardHook,
    PointerHook,
    MessageLoop,
    Transport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperFailure {
    pub stage: HelperFailureStage,
    pub native_code: Option<i32>,
}

/// Inclusive bounds for the producer's missing sequence and receipt-time interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerGap {
    pub producer: Producer,
    pub first_sequence: SourceSequence,
    pub last_sequence: SourceSequence,
    pub first_qpc: QpcTick,
    pub last_qpc: QpcTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerProgress {
    // All receipts strictly below this QPC have been queued or covered by a gap.
    pub producer: Producer,
    pub before_qpc: QpcTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawKey {
    pub device: Option<DeviceId>,
    pub make_code: u16,
    pub virtual_key: u16,
    pub flags: u16,
    pub message: u32,
    pub extra_information: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPointer {
    pub device: Option<DeviceId>,
    pub move_flags: u16,
    pub button_flags: u16,
    pub button_data: u16,
    pub raw_buttons: u32,
    pub x: i32,
    pub y: i32,
    pub extra_information: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookKey {
    pub virtual_key: u32,
    pub scan_code: u32,
    pub flags: u32,
    pub message: u32,
    pub extra_information: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookPointer {
    pub point: PhysicalPoint,
    pub mouse_data: u32,
    pub flags: u32,
    pub message: u32,
    pub extra_information: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HidReport {
    pub device: DeviceId,
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
    pub sub_index: u32,
    pub report_id: Option<u8>,
    pub result: HidReportResult,
}

impl HidReport {
    pub fn is_pen(&self) -> bool {
        matches!((self.usage_page, self.usage), (Some(0x0D), Some(1 | 2)))
    }

    pub fn pen_continuity_uncertain(&self) -> bool {
        if self.usage_page.is_some() && !self.is_pen() {
            return false;
        }
        match &self.result {
            HidReportResult::InvalidExtent | HidReportResult::PreparsedUnavailable
            | HidReportResult::UnsupportedReport => true,
            HidReportResult::Report(report) => report.fields.as_slice().iter().any(|field| {
                field.usage == HidUsage::Tip && !matches!(field.value, HidValue::Switch(_))
            }),
            _ => false,
        }
    }

    /// Distinguish a single attested tip switch from missing or
    /// multi-transducer evidence without guessing which transducer moved.
    pub fn pen_tip(&self) -> Option<(u16, bool)> {
        if !self.is_pen() {
            return None;
        }
        let HidReportResult::Report(report) = &self.result else { return None };
        let mut tips = report.fields.as_slice().iter().filter(|field| field.usage == HidUsage::Tip);
        match (tips.next(), tips.next()) {
            (Some(HidField { link, value: HidValue::Switch(down), .. }), None) => Some((*link, *down)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case", deny_unknown_fields)]
pub enum HidReportResult {
    Report(DecodedHidReport),
    Ndof(NdofReport),
    InvalidExtent,
    PreparsedUnavailable,
    UnsupportedCollection,
    UnsupportedReport,
    NoRelevantFields,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NdofReport {
    pub id: u8,
    pub data: NdofData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum NdofData {
    Motion { translation: Option<[i16; 3]>, rotation: Option<[i16; 3]> },
    ButtonMask(u64),
    ShortPress(BoundedList<u16, 32>),
    LongPress(BoundedList<u16, 32>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedHidReport {
    pub id: u8,
    pub fields: BoundedList<HidField, MAX_HID_FIELDS>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidUsage {
    X, Y, Tip, InRange, Barrel, SecondaryBarrel, Invert, Eraser,
    Pressure, TiltX, TiltY, Twist, ScanTime, ContactId, ContactCount,
    Confidence, Button1, Button2, Button3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HidField {
    pub link: u16,
    pub usage: HidUsage,
    pub value: HidValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidValue {
    Switch(bool),
    Number { raw: Option<i64>, spec: HidValueSpec },
    Angle { raw: Option<i64>, reading: Option<HidAngleReading>, spec: HidValueSpec },
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidAngleReading {
    ScaledPhysical(i32),
    SignedLogicalFallback(i64),
}

/// Logical/physical limits, HID unit and the signed four-bit unit exponent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidValueSpec {
    pub logical: [i64; 2],
    pub physical: [i64; 2],
    pub bits: u8,
    pub unit: u32,
    pub exponent: i8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", content = "sample", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputSample {
    RawKey(RawKey),
    RawPointer(RawPointer),
    HookKey(HookKey),
    HookPointer(HookPointer),
    Hid(HidReport),
}

impl InputSample {
    pub fn producer(&self) -> Producer {
        match self {
            Self::RawKey(_) | Self::RawPointer(_) | Self::Hid(_) => Producer::RawInput,
            Self::HookKey(_) => Producer::KeyboardHook,
            Self::HookPointer(_) => Producer::PointerHook,
        }
    }
}

/// Process birth time distinguishes reused process IDs without sending paths
/// across the native collection channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessInstance {
    pub pid: NonZeroU32,
    pub creation_filetime: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusAtReceipt {
    pub window: WindowId,
    pub thread: NonZeroU32,
    pub keyboard_layout: u64,
    /// Some(true) is affirmative native password evidence. An unavailable
    /// style read never makes a target safe.
    pub password_style: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForegroundAtReceipt {
    pub window: WindowId,
    pub process: ProcessInstance,
    pub focus: Option<FocusAtReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectedInput {
    pub receipt: Receipt,
    pub foreground_at_receipt: Option<ForegroundAtReceipt>,
    pub position_at_receipt: Option<PhysicalPoint>,
    pub swapped_buttons: Option<bool>,
    pub absolute_desktop: Option<PhysicalRect>,
    pub sample: InputSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeviceKind {
    Unknown,
    Keyboard,
    Pointer,
    Hid { usage_page: u16, usage: u16 },
}

pub const MAX_INPUT_DEVICES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceDescriptor {
    pub id: DeviceId,
    pub kind: DeviceKind,
    pub path_hash: Option<BoundedText<DEVICE_PATH_HASH_BYTES>>,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub version: Option<u16>,
    pub preparsed_data_available: bool,
    #[serde(default)]
    pub driver_version: Option<BoundedText<64>>,
    #[serde(default)]
    pub hid: Option<HidCollectionEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidCollectionEvidence {
    pub profile: HidProfile,
    pub fields: BoundedList<HidFieldDeclaration, MAX_HID_FIELDS>,
    pub fields_truncated: bool,
    pub missing_fields: BoundedList<HidUsage, MAX_HID_FIELDS>,
    pub touchpad_capabilities: Option<TouchpadCapabilities>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidProfile {
    ExternalPen,
    IntegratedPen,
    PrecisionTouchpad,
    SpaceMouse,
    UnsupportedMultiAxisController,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidFieldDeclaration {
    pub report_id: u8,
    pub link_collection: u16,
    pub usage: HidUsage,
    pub numerical_range: Option<HidValueSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchpadCapabilities {
    UnavailableUnboundedDriverIo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperToCore {
    Ready(InputRegistration),
    Failed(HelperFailure),
    Input(CollectedInput),
    DeviceArrived { receipt: Receipt, descriptor: DeviceDescriptor },
    DeviceRemoved { receipt: Receipt, id: DeviceId },
    Environment(EnvironmentState),
    Progress(ProducerProgress),
    Gap(ProducerGap),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    HeaderTooShort,
    PacketTooLarge,
    InvalidMagic,
    UnsupportedVersion(u16),
    WrongDirection,
    ReservedHeaderByte,
    InvalidLength,
    OutputTooSmall,
    InvalidRecord(&'static str),
    InvalidPayload,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooShort => f.write_str("Input IPC header is incomplete."),
            Self::PacketTooLarge => f.write_str("Input IPC packet exceeds the size limit."),
            Self::InvalidMagic => f.write_str("Input IPC header magic is invalid."),
            Self::UnsupportedVersion(version) => write!(f, "Unsupported input IPC version {version}."),
            Self::WrongDirection => f.write_str("Input IPC packet has the wrong direction."),
            Self::ReservedHeaderByte => f.write_str("Input IPC header has a nonzero reserved byte."),
            Self::InvalidLength => f.write_str("Input IPC payload length does not match the packet."),
            Self::OutputTooSmall => f.write_str("Input IPC output buffer is too small."),
            Self::InvalidRecord(reason) => f.write_str(reason),
            // Parsing errors may contain captured data. Keep public diagnostics structural.
            Self::InvalidPayload => f.write_str("Input IPC payload does not match the record schema."),
        }
    }
}

impl std::error::Error for PacketError {}

impl CoreToHelper {
    pub fn encode(&self, output: &mut [u8]) -> Result<usize, PacketError> {
        encode(self, TO_HELPER, output)
    }

    pub fn decode(packet: &[u8]) -> Result<Self, PacketError> {
        decode(packet, TO_HELPER)
    }
}

impl HelperToCore {
    pub fn encode(&self, output: &mut [u8]) -> Result<usize, PacketError> {
        self.validate()?;
        encode(self, TO_CORE, output)
    }

    pub fn decode(packet: &[u8]) -> Result<Self, PacketError> {
        let record: Self = decode(packet, TO_CORE)?;
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), PacketError> {
        match self {
            Self::Environment(state) if state.changes == 0 || state.changes & !EnvironmentChange::ALL_BITS != 0 =>
            {
                Err(PacketError::InvalidRecord("Input IPC environment flags are invalid."))
            }
            Self::Ready(registration)
                if !(registration.raw_keyboard && registration.raw_pointer
                    && registration.keyboard_hook && registration.pointer_hook) =>
            {
                Err(PacketError::InvalidRecord("Input helper reported readiness without required registrations."))
            }
            Self::Input(CollectedInput { receipt, foreground_at_receipt, sample: InputSample::HookKey(_) | InputSample::HookPointer(_), .. })
                if receipt.message_tick_ms.is_none() || foreground_at_receipt.is_some() =>
            {
                Err(PacketError::InvalidRecord("An input IPC hook sample requires its message tick and cannot attest foreground."))
            }
            Self::Progress(progress) if progress.before_qpc.0 < 0 => {
                Err(PacketError::InvalidRecord("An input producer progress tick is negative."))
            }
            Self::Gap(gap) if gap.first_sequence > gap.last_sequence || gap.first_qpc > gap.last_qpc => {
                Err(PacketError::InvalidRecord("Input IPC gap bounds are reversed."))
            }
            Self::Input(CollectedInput { sample: InputSample::Hid(HidReport {
                result: HidReportResult::Report(report), ..
            }), .. }) if report.fields.as_slice().is_empty() =>
            {
                Err(PacketError::InvalidRecord("A decoded input IPC HID report requires declared fields."))
            }
            _ => Ok(()),
        }
    }
}

fn encode<T: Serialize>(
    record: &T,
    direction: u8,
    output: &mut [u8],
) -> Result<usize, PacketError> {
    // The caller publishes only the returned prefix on success.
    let capacity = output.len().min(MAX_PACKET_BYTES);
    if capacity < HEADER_BYTES {
        return Err(PacketError::OutputTooSmall);
    }
    let mut payload = &mut output[HEADER_BYTES..capacity];
    if let Err(error) = serde_json::to_writer(&mut payload, record) {
        return Err(if error.io_error_kind() == Some(std::io::ErrorKind::WriteZero) {
            if capacity == MAX_PACKET_BYTES {
                PacketError::PacketTooLarge
            } else {
                PacketError::OutputTooSmall
            }
        } else {
            PacketError::InvalidPayload
        });
    }
    let length = capacity - HEADER_BYTES - payload.len();
    output[..4].copy_from_slice(&MAGIC);
    output[4..6].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
    output[6] = direction;
    output[7] = 0;
    output[8..HEADER_BYTES].copy_from_slice(&(length as u32).to_le_bytes());
    Ok(HEADER_BYTES + length)
}

fn decode<T: DeserializeOwned>(packet: &[u8], direction: u8) -> Result<T, PacketError> {
    if packet.len() < HEADER_BYTES {
        return Err(PacketError::HeaderTooShort);
    }
    if packet.len() > MAX_PACKET_BYTES {
        return Err(PacketError::PacketTooLarge);
    }
    if packet[..4] != MAGIC {
        return Err(PacketError::InvalidMagic);
    }
    let version = u16::from_le_bytes([packet[4], packet[5]]);
    if version != SCHEMA_VERSION {
        return Err(PacketError::UnsupportedVersion(version));
    }
    if packet[6] != direction {
        return Err(PacketError::WrongDirection);
    }
    if packet[7] != 0 {
        return Err(PacketError::ReservedHeaderByte);
    }
    let length = u32::from_le_bytes([packet[8], packet[9], packet[10], packet[11]]);
    if length == 0 || length as usize != packet.len() - HEADER_BYTES {
        return Err(PacketError::InvalidLength);
    }
    serde_json::from_slice(&packet[HEADER_BYTES..]).map_err(|_| PacketError::InvalidPayload)
}

#[cfg(target_os = "windows")]
pub mod shared;
