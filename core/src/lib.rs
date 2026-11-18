pub use screen_fix_protocol as protocol;

mod error;
pub use error::{CaptureError, CaptureErrorKind, Remedy, RetainedRecording};

pub mod capture;
pub mod config;
pub(crate) mod devlog;
pub mod platform;
pub mod package;
pub mod session;
