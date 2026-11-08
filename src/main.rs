mod app;
mod platform;
mod session_gate;

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
                            eprintln!("{}", app::report_error(error));
                            std::process::ExitCode::FAILURE
                        }
                    };
                }
            }
        }
        eprintln!("Использование: screen-fix --export <каталог завершённой сессии>");
        return std::process::ExitCode::FAILURE;
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([440.0, 240.0]),
        ..Default::default()
    };
    match eframe::run_native(
        "screen_fix",
        options,
        Box::new(|_| Ok(Box::new(app::ScreenFix::default()))),
    ) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", app::report_error(format!("Не удалось открыть интерфейс: {error}")));
            std::process::ExitCode::FAILURE
        }
    }
}
