//! Startup and shutdown of a native owner thread.
//!
//! An owner thread reports once that it is ready, with its thread ID, or why
//! it could not start. The starter waits at most [`STARTUP_WAIT`] for that
//! report.
//!
//! Join rule: after a failed start or a failed stop, the starter joins the
//! thread only when the thread is scoped, that is, when it reads a capture
//! scope the caller must outlive. Such a thread must end before the caller
//! reports, even though the join can block. An unscoped thread is left to
//! end on its own, so a stuck thread cannot hang its starter.

use std::{
    io,
    sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel},
    thread::{self, JoinHandle},
    time::Duration,
};

use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT},
};

/// The longest wait for an owner thread's startup report.
pub const STARTUP_WAIT: Duration = Duration::from_secs(2);

/// Why an owner thread did not start.
#[derive(Debug)]
pub enum StartupError<E> {
    /// The thread could not be created.
    Spawn(io::Error),
    /// The thread reported this failure.
    Failed(E),
    /// No report arrived within [`STARTUP_WAIT`], or the thread ended without one.
    Timeout,
}

/// The owner thread's side of the startup report.
pub struct Startup<T, E>(SyncSender<Result<(u32, T), E>>);

impl<T, E> Startup<T, E> {
    /// Report that the thread is ready, with its thread ID and `value`. False
    /// when the starter no longer waits; the thread then ends without serving.
    pub fn ready(&self, value: T) -> bool {
        self.0.try_send(Ok((unsafe { GetCurrentThreadId() }, value))).is_ok()
    }

    /// Report why the thread could not start.
    pub fn fail(&self, error: E) {
        let _ = self.0.try_send(Err(error));
    }
}

/// A started owner thread.
pub struct OwnerThread<R> {
    pub thread_id: u32,
    pub handle: JoinHandle<R>,
    /// Whether a failed stop joins the thread (see the module's join rule).
    join_on_failure: bool,
}

/// Start the owner thread `name` running `body`, and wait for its startup
/// report. On failure, `on_failure` runs first; then the thread is joined if
/// `join_on_failure` (see the module's join rule).
pub fn spawn<T, R, E>(
    name: &str,
    join_on_failure: bool,
    on_failure: impl FnOnce(),
    body: impl FnOnce(Startup<T, E>) -> R + Send + 'static,
) -> Result<(OwnerThread<R>, T), StartupError<E>>
where
    T: Send + 'static,
    R: Send + 'static,
    E: Send + 'static,
{
    let (ready, initialized) = sync_channel(1);
    let handle = thread::Builder::new().name(name.into()).spawn(move || body(Startup(ready)))
        .map_err(StartupError::Spawn)?;
    let result = initialized.recv_timeout(STARTUP_WAIT);
    // A thread that reports after this point finds no receiver.
    drop(initialized);
    match result {
        Ok(Ok((thread_id, value))) => Ok((OwnerThread { thread_id, handle, join_on_failure }, value)),
        result => {
            on_failure();
            if join_on_failure { let _ = handle.join(); }
            Err(match result {
                Ok(Err(error)) => StartupError::Failed(error),
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => StartupError::Timeout,
            })
        }
    }
}

impl<R> OwnerThread<R> {
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Post `WM_QUIT` to the thread, unless it already ended, and join it.
    /// A failed post returns its error; the thread is then joined only if
    /// it is scoped (see the module's join rule).
    pub fn post_quit_and_join(self) -> io::Result<thread::Result<R>> {
        // The thread has a message queue once it reported ready.
        if !self.handle.is_finished() && unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } == 0 {
            let error = io::Error::last_os_error();
            if self.join_on_failure { let _ = self.handle.join(); }
            return Err(error);
        }
        Ok(self.handle.join())
    }
}
