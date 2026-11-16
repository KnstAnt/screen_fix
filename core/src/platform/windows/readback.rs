use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::{self, Thread},
    time::Instant,
};

use windows::Win32::Graphics::{
    Direct3D11::{ID3D11Device, ID3D11Texture2D},
    Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
};

use crate::{
    capture::frame::{FrameExtent, FrameMetadata, MonitorGeometry},
    protocol::{AdmissionEpoch, CaptureEpoch, FrameId, GeometryId, RequestId},
};

use super::{ProbeError, native::{NativeFrame, NativeImages}, pixels};

const QUEUED_REQUESTS: usize = 8;
const MAX_JOBS: usize = 256;
pub(super) const MAX_BYTES: usize = 256_000_000;

#[derive(Debug, Clone, Copy)]
pub(super) struct Request {
    pub id: RequestId,
    pub frame: FrameId,
    pub geometry: GeometryId,
    pub admission: AdmissionEpoch,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rejected {
    Pressure,
    FrameLost,
    EpochMismatch,
    Expired,
    Closed,
    CaptureFailed,
}

pub(super) struct Pixels {
    pub metadata: FrameMetadata,
    pub geometry: MonitorGeometry,
    bytes: Vec<u8>,
    _job: Reservation,
    _cpu: Bytes,
}

impl Pixels {
    pub fn bgra(&self) -> &[u8] {
        &self.bytes
    }

    pub fn bgra_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

pub(super) struct Ticket {
    pub request: RequestId,
    result: Option<Receiver<Result<Pixels, Rejected>>>,
}

impl Ticket {
    pub fn try_result(&mut self) -> Option<Result<Pixels, Rejected>> {
        let result = match self.result.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(Rejected::Closed),
        };
        self.result = None;
        Some(result)
    }
}

struct Work {
    request: Request,
    result: SyncSender<Result<Pixels, Rejected>>,
    budget: Reservation,
}

#[derive(Default)]
pub(super) struct Budget {
    closed: AtomicBool,
    jobs: AtomicUsize,
    bytes: AtomicUsize,
}

pub(super) struct Bytes {
    budget: Arc<Budget>,
    length: usize,
}

impl Drop for Bytes {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.length, Ordering::AcqRel);
    }
}

impl Budget {
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Bytes, Rejected> {
        charge(&self.bytes, bytes, MAX_BYTES)?;
        Ok(Bytes { budget: self.clone(), length: bytes })
    }

    pub fn fits(&self, bytes: usize) -> bool {
        self.bytes.load(Ordering::Acquire).checked_add(bytes).is_some_and(|used| used <= MAX_BYTES)
    }
}

pub(super) fn image_bytes(extent: FrameExtent) -> Option<usize> {
    (extent.width.get() as usize).checked_mul(4)?.checked_mul(extent.height.get() as usize)
}

struct Reservation(Arc<Budget>);

impl Reservation {
    fn job(budget: &Arc<Budget>) -> Result<Self, Rejected> {
        charge(&budget.jobs, 1, MAX_JOBS)?;
        Ok(Self(Arc::clone(budget)))
    }
}

fn charge(counter: &AtomicUsize, amount: usize, limit: usize) -> Result<(), Rejected> {
    counter.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
        count.checked_add(amount).filter(|value| *value <= limit)
    }).map(|_| ()).map_err(|_| Rejected::Pressure)
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.jobs.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct Requests {
    send: SyncSender<Work>,
    owner: Thread,
    budget: Arc<Budget>,
}

impl Requests {
    pub fn try_request(&self, request: Request) -> Result<Ticket, Rejected> {
        if self.budget.closed.load(Ordering::Acquire) {
            return Err(Rejected::Closed);
        }
        let budget = Reservation::job(&self.budget)?;
        let (respond, result) = sync_channel(1);
        let work = Work { request, result: respond, budget };
        self.send.try_send(work).map_err(|error| match error {
            TrySendError::Full(_) => Rejected::Pressure,
            TrySendError::Disconnected(_) => Rejected::Closed,
        })?;
        self.owner.unpark();
        Ok(Ticket { request: request.id, result: Some(result) })
    }
}

struct Pending {
    work: Option<Work>,
    expired: bool,
    metadata: FrameMetadata,
    geometry: MonitorGeometry,
    row_bytes: usize,
    byte_count: usize,
    staging_bytes: Option<Bytes>,
    cpu_bytes: Option<Bytes>,
}

pub(super) struct Readbacks {
    jobs: Receiver<Work>,
    budget: Arc<Budget>,
    staging: Option<(FrameExtent, ID3D11Texture2D)>,
    pending: Option<Pending>,
}

impl Readbacks {
    pub fn new(budget: Arc<Budget>) -> (Requests, Self) {
        let (send, jobs) = sync_channel(QUEUED_REQUESTS);
        let requests = Requests { send, owner: thread::current(), budget: Arc::clone(&budget) };
        (requests, Self { jobs, budget, staging: None, pending: None })
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn step(
        &mut self, device: &ID3D11Device, images: &NativeImages, geometry: GeometryId,
        capture_epoch: CaptureEpoch, admission: AdmissionEpoch, now: Instant,
    ) -> Result<(), ProbeError> {
        self.poll(images, now)?;
        if self.pending.is_some() {
            return Ok(());
        }
        // Rejected jobs are bounded work as well.
        for _ in 0..2 {
            let Ok(work) = self.jobs.try_recv() else { break };
            if self.submit(work, device, images, geometry, capture_epoch, admission, now)? {
                break;
            }
        }
        Ok(())
    }

    fn submit(
        &mut self, work: Work, device: &ID3D11Device, images: &NativeImages,
        geometry: GeometryId, capture_epoch: CaptureEpoch, admission: AdmissionEpoch, now: Instant,
    ) -> Result<bool, ProbeError> {
        let request = work.request;
        if now >= request.deadline {
            let _ = work.result.try_send(Err(Rejected::Expired));
            return Ok(false);
        }
        if request.frame.monitor != geometry.monitor || request.frame.epoch != capture_epoch
            || request.geometry.monitor != geometry.monitor || request.admission != admission
        {
            let _ = work.result.try_send(Err(Rejected::EpochMismatch));
            return Ok(false);
        }
        let Some(frame) = images.lease(request.frame) else {
            let reason = if request.geometry == geometry { Rejected::FrameLost } else { Rejected::EpochMismatch };
            let _ = work.result.try_send(Err(reason));
            return Ok(false);
        };
        if frame.metadata.geometry.id != request.geometry || frame.metadata.admission != request.admission {
            let _ = work.result.try_send(Err(Rejected::EpochMismatch));
            return Ok(false);
        }
        self.submit_frame(work, device, images, &frame)
    }

    /// Submit an exact owner-local lease, even after history eviction.
    /// A busy staging slot leaves the lease with the monitor scheduler.
    pub fn try_retained(
        &mut self, request: Request, frame: &NativeFrame, device: &ID3D11Device, images: &NativeImages,
    ) -> Result<Option<Result<Ticket, Rejected>>, ProbeError> {
        if self.pending.is_some() {
            return Ok(None);
        }
        if self.budget.closed.load(Ordering::Acquire) {
            return Ok(Some(Err(Rejected::Closed)));
        }
        if request.frame != frame.metadata.id || request.geometry != frame.metadata.geometry.id
            || request.admission != frame.metadata.admission
        {
            return Ok(Some(Err(Rejected::EpochMismatch)));
        }
        let budget = match Reservation::job(&self.budget) {
            Ok(budget) => budget,
            Err(reason) => return Ok(Some(Err(reason))),
        };
        let (reply, result) = sync_channel(1);
        let work = Work { request, result: reply, budget };
        self.submit_frame(work, device, images, frame)?;
        Ok(Some(Ok(Ticket { request: request.id, result: Some(result) })))
    }

    fn submit_frame(
        &mut self, work: Work, device: &ID3D11Device, images: &NativeImages, frame: &NativeFrame,
    ) -> Result<bool, ProbeError> {
        let extent = frame.metadata.extent;
        let Some(byte_count) = image_bytes(extent) else {
            let _ = work.result.try_send(Err(Rejected::Pressure));
            return Ok(false);
        };
        let row_bytes = byte_count / extent.height.get() as usize;
        // Both representations count against the same per-monitor limit as
        // distinct pinned native frames. Staging drops as soon as Unmap returns.
        let charges = self.budget.reserve(byte_count).and_then(|staging| {
            self.budget.reserve(byte_count).map(|cpu| (staging, cpu))
        });
        let (staging_bytes, cpu_bytes) = match charges {
            Ok(charges) => charges,
            Err(reason) => {
                let _ = work.result.try_send(Err(reason));
                return Ok(false);
            }
        };
        if self.staging.as_ref().is_none_or(|(size, _)| *size != extent) {
            let staging = pixels::texture(device, extent.width.get(), extent.height.get(), DXGI_FORMAT_B8G8R8A8_UNORM, 0, true);
            let staging = match staging {
                Ok(staging) => staging,
                Err(error) => {
                    let _ = work.result.try_send(Err(Rejected::CaptureFailed));
                    return Err(error);
                }
            };
            self.staging = Some((extent, staging));
        }
        let (_, staging) = self.staging.as_ref().ok_or_else(|| super::unavailable("The still staging texture is unavailable"))?;
        frame.copy_into(images.context(), staging);
        self.pending = Some(Pending { work: Some(work), expired: false, metadata: frame.metadata,
            geometry: frame.metadata.geometry, row_bytes, byte_count, staging_bytes: Some(staging_bytes),
            cpu_bytes: Some(cpu_bytes) });
        Ok(true)
    }

    fn poll(&mut self, images: &NativeImages, now: Instant) -> Result<(), ProbeError> {
        let (Some(pending), Some((_, staging))) = (&mut self.pending, &self.staging) else { return Ok(()) };
        if !pending.expired && let Some(work) = &pending.work && now >= work.request.deadline {
            let _ = work.result.try_send(Err(Rejected::Expired));
            // Keep the budget until the GPU stops using staging. Never reuse it early.
            pending.expired = true;
        }
        let mut bytes = Vec::new();
        let mut allocation_failed = false;
        let maps = pixels::poll_rows(images.context(), staging, pending.row_bytes,
            pending.metadata.extent.height.get() as usize, |row| {
                if pending.expired || allocation_failed {
                    return;
                }
                if bytes.capacity() == 0 && bytes.try_reserve_exact(pending.byte_count).is_err() {
                    allocation_failed = true;
                    return;
                }
                bytes.extend_from_slice(row);
            });
        match maps {
            Ok(false) => return Ok(()),
            Err(error) => {
                if !pending.expired && let Some(work) = &pending.work {
                    let _ = work.result.try_send(Err(Rejected::CaptureFailed));
                    pending.expired = true;
                }
                return Err(error);
            }
            Ok(true) => {}
        }
        self.staging = None;
        pending.staging_bytes.take();
        if !pending.expired && let Some(work) = pending.work.take() {
            let cpu = pending.cpu_bytes.take().expect("a mapped image has a CPU budget");
            let result = if allocation_failed {
                Err(Rejected::Pressure)
            } else {
                Ok(Pixels {
                    metadata: pending.metadata, geometry: pending.geometry,
                    bytes, _job: work.budget, _cpu: cpu,
                })
            };
            let _ = work.result.try_send(result);
        }
        self.pending = None;
        Ok(())
    }

    pub fn close(&mut self, reason: Rejected) {
        self.budget.closed.store(true, Ordering::Release);
        self.cancel_epoch(reason);
    }

    pub fn cancel_epoch(&mut self, reason: Rejected) {
        self.staging = None;
        if let Some(work) = self.pending.take().filter(|pending| !pending.expired).and_then(|pending| pending.work) {
            let _ = work.result.try_send(Err(reason));
        }
        while let Ok(work) = self.jobs.try_recv() {
            let _ = work.result.try_send(Err(reason));
        }
    }
}

impl Drop for Readbacks {
    fn drop(&mut self) {
        self.close(Rejected::Closed);
    }
}
