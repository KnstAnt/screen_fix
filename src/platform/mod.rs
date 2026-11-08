#[cfg(target_os = "windows")]
mod windows;

pub(crate) const VIDEO_RECORDING_AVAILABLE: bool = cfg!(target_os = "windows");

#[derive(Clone)]
pub(crate) struct Display {
    pub(crate) id: String,
    pub(crate) label: String,
}

pub(crate) struct RecordingOptions {
    pub(crate) display_id: String,
    pub(crate) folder: std::path::PathBuf,
    pub(crate) clipboard: bool,
}

pub(crate) fn displays() -> Result<Vec<Display>, String> {
    #[cfg(target_os = "windows")]
    {
        windows::displays().map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(Vec::new())
    }
}

pub(crate) fn record_video(
    _stop: &crate::session_gate::SessionGate, _options: &RecordingOptions,
) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        windows::record(_stop, _options).map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("Запись экрана для этой ОС пока недоступна.".into())
    }
}

pub(crate) fn export_session(_folder: &std::path::Path) -> Result<std::path::PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        windows::export_session(_folder).map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("Экспорт сессии для этой ОС пока недоступен.".into())
    }
}
