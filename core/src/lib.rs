#[cfg(not(target_os = "windows"))]
compile_error!("Screen Fix supports only Windows.");

pub use screen_fix_protocol as protocol;

mod error;
pub use error::{AttemptStage, CaptureError, Remedy, RetainedRecording};

pub mod capture;
pub mod config;
pub(crate) mod devlog;
pub mod platform;
pub mod package;
pub mod session;
mod util;
