use std::sync::{Arc, mpsc::{self, Receiver, TryRecvError}};

use eframe::egui;

use crate::{platform, session_gate::SessionGate};

pub(crate) struct ScreenFix {
    state: RecordingState,
    close_after_recording: bool,
    record_clipboard: bool,
    displays: Vec<platform::Display>,
    display_id: String,
    folder: String,
}

impl Default for ScreenFix {
    fn default() -> Self {
        let mut app = Self {
            state: RecordingState::Idle,
            close_after_recording: false,
            record_clipboard: false,
            displays: Vec::new(),
            display_id: String::new(),
            folder: String::new(),
        };
        app.refresh_displays();
        app
    }
}

#[derive(Default)]
enum RecordingState {
    #[default]
    Idle,
    Running { result: Receiver<Result<String, String>>, stop: Option<Arc<SessionGate>> },
    Complete(Result<String, String>),
}

impl eframe::App for ScreenFix {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let RecordingState::Running { result: receiver, .. } = &self.state {
            match receiver.try_recv() {
                Ok(result) => self.state = RecordingState::Complete(result),
                Err(TryRecvError::Disconnected) => {
                    self.state = RecordingState::Complete(Err(report_error(
                        "Поток записи завершился без результата.".into(),
                    )));
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
            }
        }

        if self.close_after_recording && matches!(self.state, RecordingState::Complete(Ok(_))) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.close_after_recording &= matches!(self.state, RecordingState::Running { .. });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("screen_fix");
            ui.label("Запись выбранного монитора, клавиатуры и мыши. Данные сохраняются локально.");
            let running = matches!(self.state, RecordingState::Running { .. });
            if platform::VIDEO_RECORDING_AVAILABLE {
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_label("Монитор")
                        .selected_text(self.displays.iter().find(|item| item.id == self.display_id)
                            .map_or("Нет доступных мониторов", |item| item.label.as_str()))
                        .show_ui(ui, |ui| {
                            for item in &self.displays {
                                ui.selectable_value(&mut self.display_id, item.id.clone(), &item.label);
                            }
                        });
                    if ui.button("Обновить список мониторов").clicked() {
                        self.refresh_displays();
                    }
                    ui.label("Каталог сохранения");
                    ui.text_edit_singleline(&mut self.folder);
                });
            }
            ui.add_enabled(
                platform::VIDEO_RECORDING_AVAILABLE && !running,
                egui::Checkbox::new(&mut self.record_clipboard, "Сохранять текст буфера обмена"),
            );
            if ui
                .add_enabled(
                    platform::VIDEO_RECORDING_AVAILABLE && !running
                        && !self.display_id.is_empty() && !self.folder.trim().is_empty(),
                    egui::Button::new("Начать запись"),
                )
                .clicked()
            {
                self.start_recording(ctx.clone());
            }

            if !platform::VIDEO_RECORDING_AVAILABLE {
                ui.label("Для этой ОС пока доступен только интерфейс.");
            }
            match &mut self.state {
                RecordingState::Idle => {}
                RecordingState::Running { stop, .. } => {
                    if stop.is_some() {
                        ui.label("Запуск или запись…");
                        if ui.button("Остановить запись").clicked() {
                            if let Some(stop) = stop.take() {
                                stop.stop();
                            }
                        }
                    } else {
                        ui.label("Завершение и проверка MP4…");
                    }
                    if ui.ctx().input(|input| input.viewport().close_requested()) {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                        self.close_after_recording = true;
                        if let Some(stop) = stop.take() {
                            stop.stop();
                        }
                    }
                }
                RecordingState::Complete(Ok(message)) => {
                    ui.label(message.as_str());
                }
                RecordingState::Complete(Err(message)) => {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(message.as_str());
                        if ui.button("Скопировать диагностику").clicked() {
                            ui.ctx().copy_text(message.clone());
                        }
                    });
                }
            }
        });
    }
}

impl ScreenFix {
    fn refresh_displays(&mut self) {
        match platform::displays() {
            Ok(displays) => {
                if !displays.iter().any(|display| display.id == self.display_id) {
                    self.display_id = displays.first().map(|display| display.id.clone()).unwrap_or_default();
                }
                self.displays = displays;
            }
            Err(error) => {
                self.displays.clear();
                self.display_id.clear();
                self.state = RecordingState::Complete(Err(report_error(error)));
            }
        }
    }

    fn start_recording(&mut self, ctx: egui::Context) {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(SessionGate::default());
        let worker_gate = Arc::clone(&stop);
        let options = platform::RecordingOptions {
            display_id: self.display_id.clone(),
            folder: self.folder.trim().into(),
            clipboard: self.record_clipboard,
        };
        self.record_clipboard = false;
        let started = std::thread::Builder::new()
            .name("video-recording".into())
            .spawn(move || {
                let result = platform::record_video(&worker_gate, &options).map_err(report_error);
                let _ = sender.send(result);
                ctx.request_repaint();
            });
        self.state = match started {
            Ok(_) => RecordingState::Running { result: receiver, stop: Some(stop) },
            Err(error) => RecordingState::Complete(Err(report_error(format!(
                "Не удалось запустить поток записи: {error}"
            )))),
        };
    }
}

pub(crate) fn report_error(error: String) -> String {
    let path = std::env::temp_dir().join(format!("screen-fix-probe-{}.txt", std::process::id()));
    let report = format!(
        "screen_fix {}\nПлатформа: {}\n{error}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
    );
    let location = match std::fs::write(&path, &report) {
        Ok(()) => format!("Отчёт для отправки: {}", path.display()),
        Err(save_error) => format!("Не удалось сохранить отчёт: {save_error}"),
    };
    format!("{report}\n{location}")
}
