#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    screen_fix_input_helper::run_child()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
