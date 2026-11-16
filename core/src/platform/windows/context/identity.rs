use std::{
    collections::VecDeque, num::NonZeroU32, path::Path,
    sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{self, Receiver, SyncSender}},
    thread, time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, HWND},
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    },
    core::PWSTR,
};

use crate::{
    config::{ExecutableIdentity, ResolvedProfile},
    protocol::{QpcTick, WindowId, ipc::ProcessInstance},
};

const PROCESS_CACHE: usize = 64;
const RESOLUTION_WAIT: Duration = Duration::from_secs(2);
static RESOLVER_ACTIVE: AtomicBool = AtomicBool::new(false);

struct ResolutionLease;

impl Drop for ResolutionLease {
    fn drop(&mut self) { RESOLVER_ACTIVE.store(false, Ordering::Release); }
}

enum ApprovalMatch {
    Resolved(Option<usize>),
    Canonicalize(ExecutableIdentity),
}

#[derive(Clone, Copy)]
enum ResolutionError {
    ProcessIdentityUnavailable,
    ExecutablePathUnavailable,
}

struct CachedApproval {
    process: ProcessInstance,
    index: Option<usize>,
    not_before: Option<QpcTick>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::platform::windows) struct WindowProcess {
    pub window: WindowId,
    pub process: ProcessInstance,
    pub thread: NonZeroU32,
}

impl WindowProcess {
    pub fn read(window: HWND) -> Option<Self> {
        if window.0.is_null() {
            return None;
        }
        let mut pid = 0;
        let thread = NonZeroU32::new(unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) })?;
        let pid = NonZeroU32::new(pid)?;
        let handle = ProcessHandle::open(pid)?;
        let process = handle.instance(pid)?;
        let mut verified_pid = 0;
        let verified_thread = unsafe { GetWindowThreadProcessId(window, Some(&mut verified_pid)) };
        (verified_thread == thread.get() && verified_pid == pid.get()).then_some(Self {
            window: WindowId(window.0 as usize as u64), process, thread,
        })
    }

    pub fn foreground() -> Option<Self> {
        let window = unsafe { GetForegroundWindow() };
        let proof = Self::read(window)?;
        (unsafe { GetForegroundWindow() } == window).then_some(proof)
    }

    pub fn hwnd(self) -> HWND {
        HWND(self.window.0 as usize as *mut _)
    }
}

pub(in crate::platform::windows) struct ProcessHandle(HANDLE);

impl ProcessHandle {
    pub fn raw(&self) -> HANDLE { self.0 }

    pub fn open(pid: NonZeroU32) -> Option<Self> {
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid.get()) }.ok().map(Self)
    }

    pub fn instance(&self, pid: NonZeroU32) -> Option<ProcessInstance> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
        let creation_filetime = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        Some(ProcessInstance { pid, creation_filetime })
    }

    pub fn executable(&self) -> Option<ExecutableIdentity> {
        let mut units = [0_u16; 4096];
        let mut length = units.len() as u32;
        unsafe { QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, PWSTR(units.as_mut_ptr()), &mut length) }.ok()?;
        ExecutableIdentity::new(&String::from_utf16(units.get(..length as usize)?).ok()?).ok()
    }
}

pub(in crate::platform::windows) fn canonical_executable(path: &Path) -> Option<ExecutableIdentity> {
    let resolved = path.canonicalize().ok()?;
    if !resolved.is_file() {
        return None;
    }
    let text = resolved.to_str()?;
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return ExecutableIdentity::new(&format!(r"\\{unc}")).ok();
    }
    ExecutableIdentity::new(text.strip_prefix(r"\\?\").unwrap_or(text)).ok()
}

pub(in crate::platform::windows) fn image_name(path: &str) -> Option<String> {
    Path::new(path).file_name()?.to_str().map(str::to_lowercase)
}

impl ProcessHandle {
    fn approval_match(&self, approved: &[ExecutableIdentity]) -> Result<ApprovalMatch, ()> {
        let actual = self.executable().ok_or(())?;
        if let Some(index) = approved.iter().position(|entry| *entry == actual) {
            return Ok(ApprovalMatch::Resolved(Some(index)));
        }
        let name = image_name(actual.executable()).ok_or(())?;
        if !approved.iter().any(|entry| image_name(entry.executable()).as_deref() == Some(name.as_str())) {
            return Ok(ApprovalMatch::Resolved(None));
        }
        Ok(ApprovalMatch::Canonicalize(actual))
    }

    /// Only startup and the path-resolution owner may perform filesystem IO.
    pub fn approved_index(&self, approved: &[ExecutableIdentity]) -> Result<Option<usize>, ()> {
        let actual = match self.approval_match(approved)? {
            ApprovalMatch::Resolved(index) => return Ok(index),
            ApprovalMatch::Canonicalize(actual) => actual,
        };
        // Canonicalize potential alternate Win32 spellings. Compare canonical
        // paths exactly because a Windows directory can be case sensitive.
        let actual = canonical_executable(Path::new(actual.executable())).ok_or(())?;
        Ok(approved.iter().position(|entry| *entry == actual))
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Cache only executable authorization for a verified process object. A
/// window handle or the currently foreground app cannot grant authorization
/// for an earlier packet.
pub(in crate::platform::windows) struct ApprovedProcesses<'a> {
    profile: &'a ResolvedProfile,
    cache: VecDeque<CachedApproval>,
    pending: VecDeque<(ProcessInstance, Instant)>,
    requests: SyncSender<ProcessInstance>,
    results: Receiver<(ProcessInstance, Result<Option<usize>, ResolutionError>)>,
    stop: Arc<AtomicBool>,
}

impl<'a> ApprovedProcesses<'a> {
    pub fn new(profile: &'a ResolvedProfile) -> Result<Self, super::ProbeError> {
        RESOLVER_ACTIVE.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| super::unavailable("A previous executable path resolution owner is still active"))?;
        let lease = ResolutionLease;
        let (requests, incoming) = mpsc::sync_channel::<ProcessInstance>(PROCESS_CACHE);
        let (completed, results) = mpsc::sync_channel(PROCESS_CACHE);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let approved = profile.approved_identities().to_vec();
        thread::Builder::new().name("executable-path-resolution".into()).spawn(move || {
            let _lease = lease;
            while let Ok(proof) = incoming.recv() {
                if stopping.load(Ordering::Acquire) { break; }
                let index = ProcessHandle::open(proof.pid)
                    .filter(|process| process.instance(proof.pid) == Some(proof))
                    .ok_or(ResolutionError::ProcessIdentityUnavailable)
                    .and_then(|process| process.approved_index(&approved)
                        .map_err(|()| ResolutionError::ExecutablePathUnavailable));
                if stopping.load(Ordering::Acquire) || completed.try_send((proof, index)).is_err() { break; }
            }
        }).map_err(|error| super::super::mp4::io_error("Start executable path resolution", error))?;
        Ok(Self { profile, cache: VecDeque::with_capacity(PROCESS_CACHE),
            pending: VecDeque::with_capacity(PROCESS_CACHE), requests, results, stop })
    }

    pub fn poll(&mut self) -> Result<(), super::ProbeError> {
        loop {
            match self.results.try_recv() {
                Ok((proof, index)) => {
                    self.pending.retain(|(pending, _)| *pending != proof);
                    let index = match index {
                        Ok(index) => index,
                        // A queued candidate may exit or its PID may be reused.
                        // Unknown identity grants nothing and is not a mismatch.
                        Err(ResolutionError::ProcessIdentityUnavailable) => continue,
                        Err(ResolutionError::ExecutablePathUnavailable) => {
                            return Err(super::unavailable("The executable path could not be resolved"));
                        }
                    };
                    let not_before = index.map(|_| super::clock::qpc()).transpose()?;
                    self.remember(proof, index, not_before);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(super::unavailable("The executable path resolution owner exited unexpectedly"));
                }
            }
        }
        if self.pending.iter().any(|(_, submitted)| submitted.elapsed() >= RESOLUTION_WAIT) {
            return Err(super::unavailable("Executable path resolution exceeded its bounded wait"));
        }
        Ok(())
    }

    fn remember(&mut self, proof: ProcessInstance, index: Option<usize>, not_before: Option<QpcTick>) {
        if self.cache.len() == PROCESS_CACHE { self.cache.pop_front(); }
        self.cache.push_back(CachedApproval { process: proof, index, not_before });
    }

    pub fn get_at(&mut self, proof: ProcessInstance, receipt: QpcTick) -> Option<&'a ExecutableIdentity> {
        let identity = self.get(proof)?;
        let cached = self.cache.iter().find(|cached| cached.process == proof)?;
        cached.not_before.is_none_or(|boundary| receipt >= boundary).then_some(identity)
    }

    pub fn get(&mut self, proof: ProcessInstance) -> Option<&'a ExecutableIdentity> {
        if proof.pid.get() == std::process::id() || self.profile.approved_identities().is_empty() {
            return None;
        }
        if let Some(cached) = self.cache.iter().find(|cached| cached.process == proof) {
            return cached.index.map(|index| &self.profile.approved_identities()[index]);
        }
        let process = ProcessHandle::open(proof.pid)?;
        if process.instance(proof.pid)? != proof {
            return None;
        }
        let index = match process.approval_match(self.profile.approved_identities()).ok()? {
            ApprovalMatch::Resolved(index) => index,
            ApprovalMatch::Canonicalize(_) => {
                if self.pending.len() < PROCESS_CACHE
                    && !self.pending.iter().any(|(pending, _)| *pending == proof)
                    && self.requests.try_send(proof).is_ok()
                {
                    self.pending.push_back((proof, Instant::now()));
                }
                // Unknown approval remains private. A later result may admit
                // current evidence, never packets from before confirmation.
                return None;
            }
        };
        self.remember(proof, index, None);
        index.map(|index| &self.profile.approved_identities()[index])
    }
}

impl Drop for ApprovedProcesses<'_> {
    fn drop(&mut self) {
        // A blocked filesystem call retains only its own process handle and
        // immutable identities. Input and Stop never join that native call.
        self.stop.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(in crate::platform::windows) struct MetadataScrubber {
    substitutions: Vec<(String, &'static str)>,
}

impl MetadataScrubber {
    pub fn new() -> Self {
        let substitutions = [
            ("USERPROFILE", "<home>"),
            ("COMPUTERNAME", "<host>"),
            ("USERNAME", "<user>"),
        ].into_iter().filter_map(|(variable, token)| {
            std::env::var(variable).ok().filter(|value| !value.is_empty()).map(|value| (value, token))
        }).collect();
        Self { substitutions }
    }

    pub fn scrub(&self, source: &str, bound: usize) -> String {
        let mut result = String::with_capacity(source.len().min(bound));
        let mut offset = 0;
        while let Some(rest) = source.get(offset..).filter(|rest| !rest.is_empty()) {
            let matched = self.substitutions.iter().find(|(value, _)| {
                rest.get(..value.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(value))
            });
            let (part, consumed) = if let Some((value, token)) = matched {
                (*token, value.len())
            } else {
                let first = rest.chars().next().expect("nonempty metadata");
                (&rest[..first.len_utf8()], first.len_utf8())
            };
            if result.len().saturating_add(part.len()) > bound {
                break;
            }
            result.push_str(part);
            offset += consumed;
        }
        result
    }
}
