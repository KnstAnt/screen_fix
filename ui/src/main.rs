mod app;

use screen_fix_core::{package::diagnostics::FailureComponent, platform};

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    if let Some(command) = args.next() {
        if command == "--export" {
            if let Some(folder) = args.next() {
                if args.next().is_none() {
                    return match platform::export_session(std::path::Path::new(&folder)) {
                        Ok(output) => {
                            println!("{}", output.display());
                            std::process::ExitCode::SUCCESS
                        }
                        Err(error) => {
                            let message = error.to_string();
                            eprintln!("{}", app::with_report(&message, error.into_diagnostic().with_output_root(std::path::PathBuf::from(&folder)).write()));
                            std::process::ExitCode::FAILURE
                        }
                    };
                }
            }
        }
        eprintln!("Usage: screen-fix --export <completed-session-directory>");
        return std::process::ExitCode::FAILURE;
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([500.0, 570.0])
            .with_min_inner_size([360.0, 250.0]),
        ..Default::default()
    };
    match eframe::run_native(
        "screen_fix",
        options,
        Box::new(|creation| Ok(Box::new(app::ScreenFix::new(creation)))),
    ) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let message = format!("Could not open the interface: {error}");
            let report = app::diagnostic(FailureComponent::Interface, "Open the interface").write();
            eprintln!("{}", app::with_report(&message, report));
            std::process::ExitCode::FAILURE
        }
    }
}
