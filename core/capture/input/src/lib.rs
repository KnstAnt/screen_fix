//! Input-helper side of attachment to the recorder's bounded IPC channel.
//!

use screen_fix_protocol::ipc::{ConfigureInput, CoreToHelper, EnvironmentChange, PacketError};

pub struct InputAttachment {
    config: ConfigureInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputControl {
    EnvironmentChanged(EnvironmentChange),
    Stop,
}

impl InputAttachment {
    pub fn configure(first_packet: &[u8]) -> Result<Self, PacketError> {
        match CoreToHelper::decode(first_packet)? {
            CoreToHelper::Configure(config) => Ok(Self { config }),
            _ => Err(PacketError::InvalidRecord("Expected input configuration as the first packet.")),
        }
    }

    pub fn configuration(&self) -> &ConfigureInput {
        &self.config
    }

    pub fn receive_control(&self, packet: &[u8]) -> Result<InputControl, PacketError> {
        match CoreToHelper::decode(packet)? {
            CoreToHelper::EnvironmentChanged(change) => Ok(InputControl::EnvironmentChanged(change)),
            CoreToHelper::Stop => Ok(InputControl::Stop),
            CoreToHelper::Configure(_) => {
                Err(PacketError::InvalidRecord("Input configuration is already set."))
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub fn run_child() -> std::process::ExitCode {
    windows::run_child()
}
