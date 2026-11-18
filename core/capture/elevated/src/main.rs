#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    match screen_fix_elevated::run_collector() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}
