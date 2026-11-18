use std::{
    collections::VecDeque, path::Path,
    sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{self, Receiver, SyncSender}},
    thread, time::{Duration, Instant},
};

pub(in crate::platform::windows) use screen_fix_native_observation::identity::{
    MetadataScrubber, ProcessHandle, WindowProcess,
};
use screen_fix_native_observation::StaticLease;

use crate::{
    config::{ExecutableIdentity, ResolvedProfile},
    protocol::{QpcTick, ipc::ProcessInstance},
};

const PROCESS_CACHE: usize = 64;
const RESOLUTION_WAIT: Duration = Duration::from_secs(2);
static RESOLVER_ACTIVE: AtomicBool = AtomicBool::new(false);

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

/// The collector applies the same canonical rule to its own proof (E6).
pub(in crate::platform::windows) fn canonical_executable(path: &Path) -> Option<ExecutableIdentity> {
    ExecutableIdentity::new(&screen_fix_native_observation::identity::canonical_image(path.to_str()?)?).ok()
}

pub(in crate::platform::windows) fn image_name(path: &str) -> Option<String> {
    Path::new(path).file_name()?.to_str().map(str::to_lowercase)
}

fn approval_match(image: &str, approved: &[ExecutableIdentity]) -> Result<ApprovalMatch, ()> {
    let actual = ExecutableIdentity::new(image).map_err(|_| ())?;
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
pub(in crate::platform::windows) fn approved_index(process: &ProcessHandle, approved: &[ExecutableIdentity]) -> Result<Option<usize>, ()> {
    approved_image_index(&process.image_path().ok_or(())?, approved)
}

pub(in crate::platform::windows) fn approved_image_index(image: &str, approved: &[ExecutableIdentity]) -> Result<Option<usize>, ()> {
    let actual = match approval_match(image, approved)? {
        ApprovalMatch::Resolved(index) => return Ok(index),
        ApprovalMatch::Canonicalize(actual) => actual,
    };
    // Canonicalize potential alternate Win32 spellings. Compare canonical
    // paths exactly because a Windows directory can be case sensitive.
    let actual = canonical_executable(Path::new(actual.executable())).ok_or(())?;
    Ok(approved.iter().position(|entry| *entry == actual))
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
    /// The resolver is blocked or gone (§6.2 row 15). Alternative spellings
    /// stay approval-unknown; no replacement thread is started.
    lost: bool,
}

impl<'a> ApprovedProcesses<'a> {
    pub fn new(profile: &'a ResolvedProfile) -> Result<Self, super::ProbeError> {
        let lease = StaticLease::claim(&RESOLVER_ACTIVE)
            .ok_or_else(|| super::unavailable("A previous executable path resolution owner is still active"))?;
        let (requests, incoming) = mpsc::sync_channel::<ProcessInstance>(PROCESS_CACHE);
        let (completed, results) = mpsc::sync_channel(PROCESS_CACHE);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let approved = profile.approved_identities().to_vec();
        thread::Builder::new().name("executable-path-resolution".into()).spawn(move || {
            let _lease = lease;
            while let Ok(proof) = incoming.recv() {
                if stopping.load(Ordering::Acquire) { break; }
                let index = ProcessHandle::open_instance(proof)
                    .ok_or(ResolutionError::ProcessIdentityUnavailable)
                    .and_then(|process| approved_index(&process, &approved)
                        .map_err(|()| ResolutionError::ExecutablePathUnavailable));
                if stopping.load(Ordering::Acquire) || completed.try_send((proof, index)).is_err() { break; }
            }
        }).map_err(|error| super::super::io_error("Start executable path resolution", error))?;
        Ok(Self { profile, cache: VecDeque::with_capacity(PROCESS_CACHE),
            pending: VecDeque::with_capacity(PROCESS_CACHE), requests, results, stop, lost: false })
    }

    /// Apply resolver results. Returns the number of spellings that could not
    /// be canonicalized (each foreground stays approval-unknown, G) and
    /// whether the resolver was newly lost (L-auxiliary). Neither fails the
    /// context loop (C3).
    pub fn poll(&mut self) -> (u64, bool) {
        let mut unresolved = 0_u64;
        let mut lost = false;
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
                            unresolved = unresolved.saturating_add(1);
                            self.remember(proof, None, None);
                            continue;
                        }
                    };
                    // Without a receipt bound, the approval admits no earlier evidence.
                    let not_before = index.map(|_| super::clock::qpc().unwrap_or(QpcTick(i64::MAX)));
                    self.remember(proof, index, not_before);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    lost = true;
                    break;
                }
            }
        }
        // A resolution pending past the bound means the resolver is blocked in a native call.
        lost |= self.pending.iter().any(|(_, submitted)| submitted.elapsed() >= RESOLUTION_WAIT);
        if lost {
            self.pending.clear();
        }
        let newly = lost && !self.lost;
        self.lost |= lost;
        (unresolved, newly)
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
        let process = ProcessHandle::open_instance(proof)?;
        let index = match approval_match(&process.image_path()?, self.profile.approved_identities()).ok()? {
            ApprovalMatch::Resolved(index) => index,
            ApprovalMatch::Canonicalize(_) => {
                if !self.lost && self.pending.len() < PROCESS_CACHE
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
