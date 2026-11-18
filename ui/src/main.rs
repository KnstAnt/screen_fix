mod app;

use screen_fix_core::package::diagnostics::FailureComponent;

fn main() -> std::process::ExitCode {
    #[cfg(all(target_os = "windows", feature = "elevated-release"))]
    if let Err(error) = screen_fix_core::platform::restrict_release_dll_search() {
        eprintln!("{error}");
        return std::process::ExitCode::FAILURE;
    }

    if std::env::args_os().nth(1).is_some() {
        eprintln!("Usage: screen-fix");
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
