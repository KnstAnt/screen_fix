//! Input-helper side of attachment to the recorder's bounded IPC channel.
//!

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub fn run_child() -> std::process::ExitCode {
    windows::run_child()
}
