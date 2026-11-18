//! Development log of one attempt (`dev.log` in the attempt directory).
//!
//! Enabled by the `dev-log` feature, which is for development diagnosis only
//! and is to be removed or disabled for production. Without the feature every
//! call compiles to nothing. Writes never fail the recording: errors are
//! ignored. The file is not part of the attempt inventory and is not listed in
//! the manifest.
//!
//! Privacy: only timing, counters, ids, enum reasons, error stages and sizes
//! are logged; never key codes, text, window titles, paths or user names.

use std::{fmt::Arguments, path::Path};

/// Whether the development log is compiled in.
pub(crate) const ENABLED: bool = cfg!(feature = "dev-log");

/// The file name inside the attempt directory.
#[cfg_attr(not(feature = "dev-log"), allow(dead_code))]
const FILE_NAME: &str = "dev.log";

/// Write one line: `devlog!("tag", "format", args...)`.
macro_rules! devlog {
    ($tag:expr, $($arg:tt)+) => {
        if $crate::devlog::ENABLED {
            $crate::devlog::write($tag, format_args!($($arg)+));
        }
    };
}
pub(crate) use devlog;

#[cfg(feature = "dev-log")]
mod sink {
    use std::{
        fs::File,
        io::{LineWriter, Write},
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    pub(super) static SINK: Mutex<Option<LineWriter<File>>> = Mutex::new(None);

    pub(super) fn line(tag: &str, message: std::fmt::Arguments<'_>) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let thread = std::thread::current();
        let mut sink = SINK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(file) = sink.as_mut() {
            let _ = match thread.name() {
                Some(name) => writeln!(file, "{}.{:06} [{name}] {tag}: {message}", now.as_secs(), now.subsec_micros()),
                None => writeln!(file, "{}.{:06} [{:?}] {tag}: {message}", now.as_secs(), now.subsec_micros(), thread.id()),
            };
        }
    }
}

/// Start `dev.log` in a newly created attempt directory, replacing any previous sink.
pub(crate) fn open(_attempt: &Path) {
    #[cfg(feature = "dev-log")]
    {
        let file = std::fs::File::create(_attempt.join(FILE_NAME)).ok().map(std::io::LineWriter::new);
        *sink::SINK.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = file;
        write("devlog", format_args!("opened"));
    }
}

/// Flush and close the current attempt's `dev.log`.
pub(crate) fn close() {
    #[cfg(feature = "dev-log")]
    {
        write("devlog", format_args!("closed"));
        let file = sink::SINK.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(mut file) = file {
            let _ = std::io::Write::flush(&mut file);
        }
    }
}

#[doc(hidden)]
pub(crate) fn write(_tag: &str, _message: Arguments<'_>) {
    #[cfg(feature = "dev-log")]
    sink::line(_tag, _message);
}
