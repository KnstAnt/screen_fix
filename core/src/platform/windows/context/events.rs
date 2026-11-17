use screen_fix_native_observation::events::{self, StartError};

pub(super) use events::{DISPLAY_CHANGE, Notice, SETTINGS_CHANGE};

use super::super::{ProbeError, mp4::io_error, unavailable};

pub(super) struct Notifications(events::Notifications);

impl Notifications {
    pub fn start() -> Result<Self, ProbeError> {
        events::Notifications::start().map(Self).map_err(|error| match error {
            StartError::Spawn(error) => io_error("Start context notification loop", error),
            StartError::StartupTimeout => unavailable("A context notification loop did not start"),
            StartError::HookRegistration => unavailable("Could not subscribe to Windows context notifications"),
            StartError::HiddenWindowModule => unavailable("Could not get the window context module"),
            StartError::HiddenWindowClass => unavailable("Could not register the window context class"),
            StartError::HiddenWindowCreation => unavailable("Could not create the hidden top-level context window"),
        })
    }

    pub fn drain(&self) -> impl Iterator<Item = Notice> + '_ { self.0.drain() }

    /// Lost notices since the last drain: count and first and last receipt.
    pub fn take_loss(&self) -> Option<(u64, crate::protocol::QpcTick, crate::protocol::QpcTick)> { self.0.take_loss() }

    pub fn running(&self) -> bool { self.0.running() }
}
