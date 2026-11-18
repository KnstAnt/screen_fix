//! Input-helper side of attachment to the recorder's bounded IPC channel.
//!

#[cfg(not(target_os = "windows"))]
compile_error!("Screen Fix supports only Windows.");

mod windows;

pub fn run_child() -> std::process::ExitCode {
    windows::run_child()
}
