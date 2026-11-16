use serde::Serialize;

use crate::{capture::save::CheckpointRequest, protocol::{MessageTickMs, QpcTick, RequestId}};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderAction {
    Start,
    TogglePause,
    Checkpoint,
    Stop,
}

impl RecorderAction {
    pub const ALL: [Self; 4] = [Self::Start, Self::TogglePause, Self::Checkpoint, Self::Stop];

    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::TogglePause => "Pause or resume",
            Self::Checkpoint => "Checkpoint request",
            Self::Stop => "Stop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyBindings {
    functions: [u8; 4],
}

impl Default for HotkeyBindings {
    fn default() -> Self {
        Self { functions: [9, 10, 11, 12] }
    }
}

impl HotkeyBindings {
    pub fn function(self, action: RecorderAction) -> u8 { self.functions[action as usize] }

    pub(crate) fn virtual_keys(self) -> [u16; 4] { self.functions.map(|function| 0x6f + u16::from(function)) }

    pub fn set_function(&mut self, action: RecorderAction, function: u8) -> bool {
        if !(1..=24).contains(&function) {
            return false;
        }
        self.functions[action as usize] = function;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyActivation {
    pub(crate) action: RecorderAction,
    pub(crate) sequence: u64,
    pub(crate) at: QpcTick,
    pub(crate) message_tick: MessageTickMs,
    pub(crate) virtual_key: u16,
}

impl HotkeyActivation {
    pub(crate) fn checkpoint(self) -> CheckpointRequest {
        // Keep recorder IDs distinct from the input owner's private slot IDs.
        CheckpointRequest { id: RequestId((1 << 63) | self.sequence) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOutcome {
    StartRequested,
    AlreadyStartingOrRecording,
    PauseRequested,
    ResumeRequested,
    PauseUnavailable,
    CheckpointRequested,
    CheckpointUnavailable,
    StopRequested,
    AlreadyStopping,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlNotice {
    pub activation: HotkeyActivation,
    pub outcome: ControlOutcome,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlLoss {
    pub first: QpcTick,
    pub last: QpcTick,
    pub count: Option<u64>,
}

impl ControlLoss {
    pub fn record(previous: &mut Option<Self>, at: QpcTick) {
        Self::merge(previous, Self { first: at, last: at, count: Some(1) });
    }

    pub fn merge(previous: &mut Option<Self>, next: Self) {
        *previous = Some(previous.map_or(next, |old| Self {
            first: old.first.min(next.first),
            last: old.last.max(next.last),
            count: old.count.zip(next.count).map(|(old, next)| old.saturating_add(next)),
        }));
    }
}
