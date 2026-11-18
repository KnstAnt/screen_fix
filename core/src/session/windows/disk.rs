use std::{
    fs, io,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::null_mut,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

use crate::{CaptureError, session::DiskPreflight};

use super::WorkerSession;

const GB: u64 = 1_000_000_000;
/// The free space that starting a recording requires, in GB.
pub(super) const START_MINIMUM_GB: u64 = 20;
pub(super) const START_MINIMUM: u64 = START_MINIMUM_GB * GB;
pub(super) const PAUSE_BELOW: u64 = 5 * GB;
pub(super) const RESUME_ABOVE: u64 = 6 * GB;
const INTERVAL: Duration = Duration::from_secs(1);
pub(super) const MAX_SAMPLE_AGE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
pub(super) struct Sample {
    pub started: Instant,
    pub available_bytes: Option<u64>,
}

pub(crate) struct Monitor {
    stop: mpsc::SyncSender<()>,
    owner: Option<thread::JoinHandle<()>>,
}

impl Monitor {
    pub(crate) fn start(session: &WorkerSession) -> Result<(Self, DiskPreflight), CaptureError> {
        // Check the parent of the attempt directories, including any volume
        // mount or junction on the attempts directory itself.
        let directory = session.config().output_root().join("attempts");
        fs::create_dir_all(&directory).map_err(preflight_error)?;
        let started = Instant::now();
        let available_bytes = available(&directory).map_err(preflight_error)?;
        if available_bytes < START_MINIMUM {
            return Err(CaptureError::owned(crate::package::diagnostics::FailureComponent::Controller, "Check recording disk space", None,
                format!("Recording needs at least {START_MINIMUM_GB} GB free. Available: {available_bytes} bytes.")));
        }
        let preflight = DiskPreflight {
            available_bytes, minimum_bytes: START_MINIMUM, duration_reserve_bytes: None,
        };
        session.disk_sample(Sample { started, available_bytes: Some(available_bytes) });
        let (stop, stopping) = mpsc::sync_channel(1);
        let session = session.clone();
        let owner = thread::Builder::new().name("recording-disk-space".into()).spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopping.recv_timeout(INTERVAL) {
                let started = Instant::now();
                session.disk_sample(Sample { started, available_bytes: available(&directory).ok() });
            }
        }).map_err(preflight_error)?;
        Ok((Self { stop, owner: Some(owner) }, preflight))
    }

}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

fn preflight_error(error: io::Error) -> CaptureError {
    CaptureError::owned(crate::package::diagnostics::FailureComponent::Controller, "Check recording disk space", error.raw_os_error(), error.to_string())
}

fn available(directory: &Path) -> io::Result<u64> {
    let mut wide = std::path::absolute(directory)?.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid recording directory"));
    }
    // A trailing separator is required for UNC shares.
    if wide.last() != Some(&u16::from(b'\\')) && wide.last() != Some(&u16::from(b'/')) {
        wide.push(u16::from(b'\\'));
    }
    wide.push(0);
    let mut caller_available = 0;
    // The UTF-16 directory and output buffer live through the call. The other
    // output pointers are optional. Caller-available space accounts for quotas.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut caller_available, null_mut(), null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(caller_available)
}
