use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    time::Instant,
};

use windows::Win32::Graphics::{
    Direct3D11::{ID3D11Device, ID3D11Texture2D},
    Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
};

use crate::{
    capture::frame::{FrameExtent, FrameMetadata},
    protocol::{AdmissionEpoch, FrameId, GeometryId},
};

use super::{ProbeError, native::{NativeFrame, NativeImages}, pixels};

const MAX_JOBS: usize = 256;
pub(super) const MAX_BYTES: usize = 256_000_000;

#[derive(Debug, Clone, Copy)]
pub(super) struct Request {
    pub frame: FrameId,
    pub geometry: GeometryId,
    pub admission: AdmissionEpoch,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rejected {
    Pressure,
    EpochMismatch,
    ApprovalSuppressed,
    Expired,
    Closed,
    CaptureFailed,
}

pub(super) struct Pixels {
    pub metadata: FrameMetadata,
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

struct Pending {
    work: Option<Work>,
    expired: bool,
    metadata: FrameMetadata,
    row_bytes: usize,
    byte_count: usize,
    cpu_bytes: Option<Bytes>,
}

pub(super) struct Readbacks {
    budget: Arc<Budget>,
    /// Reused for every readback of one extent (format is always BGRA8) and
    /// charged while it exists. Dropped on an extent change or cancel_epoch.
    staging: Option<(FrameExtent, ID3D11Texture2D, Bytes)>,
    pending: Option<Pending>,
}

impl Readbacks {
    pub fn new(budget: Arc<Budget>) -> Self {
        Self { budget, staging: None, pending: None }
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the budget takes a readback of the frame: its CPU image, and a
    /// staging texture unless one of the same extent is kept.
    pub fn fits(&self, frame: &NativeFrame) -> bool {
        let extent = frame.metadata.extent;
        let kept = self.staging.as_ref().is_some_and(|(size, ..)| *size == extent);
        image_bytes(extent).and_then(|bytes| bytes.checked_mul(if kept { 1 } else { 2 }))
            .is_some_and(|bytes| self.budget.fits(bytes))
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
        if !frame.metadata.covered() {
            return Ok(Some(Err(Rejected::ApprovalSuppressed)));
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
        Ok(Some(Ok(Ticket { result: Some(result) })))
    }

    fn submit_frame(
        &mut self, work: Work, device: &ID3D11Device, images: &NativeImages, frame: &NativeFrame,
    ) -> Result<(), ProbeError> {
        let extent = frame.metadata.extent;
        let Some(byte_count) = image_bytes(extent) else {
            let _ = work.result.try_send(Err(Rejected::Pressure));
            return Ok(());
        };
        let row_bytes = byte_count / extent.height.get() as usize;
        // Both representations count against the same per-monitor limit as
        // distinct pinned native frames.
        let cpu_bytes = match self.budget.reserve(byte_count) {
            Ok(bytes) => bytes,
            Err(reason) => {
                let _ = work.result.try_send(Err(reason));
                return Ok(());
            }
        };
        if self.staging.as_ref().is_none_or(|(size, ..)| *size != extent) {
            self.staging = None;
            let staging_bytes = match self.budget.reserve(byte_count) {
                Ok(bytes) => bytes,
                Err(reason) => {
                    let _ = work.result.try_send(Err(reason));
                    return Ok(());
                }
            };
            let staging = pixels::texture(device, extent.width.get(), extent.height.get(), DXGI_FORMAT_B8G8R8A8_UNORM, 0, true);
            let staging = match staging {
                Ok(staging) => staging,
                Err(error) => {
                    let _ = work.result.try_send(Err(Rejected::CaptureFailed));
                    return Err(error);
                }
            };
            self.staging = Some((extent, staging, staging_bytes));
        }
        let (_, staging, _) = self.staging.as_ref().ok_or_else(|| super::unavailable("The still staging texture is unavailable"))?;
        frame.copy_into(images.context(), staging);
        self.pending = Some(Pending { work: Some(work), expired: false, metadata: frame.metadata.clone(),
            row_bytes, byte_count, cpu_bytes: Some(cpu_bytes) });
        Ok(())
    }

    pub fn poll(&mut self, images: &NativeImages, now: Instant) -> Result<(), ProbeError> {
        let (Some(pending), Some((_, staging, _))) = (&mut self.pending, &self.staging) else { return Ok(()) };
        if !pending.expired && let Some(work) = &pending.work {
            let rejection = if !pending.metadata.covered() {
                Some(Rejected::ApprovalSuppressed)
            } else {
                (now >= work.request.deadline).then_some(Rejected::Expired)
            };
            if let Some(reason) = rejection {
                let _ = work.result.try_send(Err(reason));
                pending.expired = true;
            }
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
        if !pending.expired && let Some(work) = pending.work.take() {
            let cpu = pending.cpu_bytes.take().expect("a mapped image has a CPU budget");
            let result = if !pending.metadata.covered() {
                Err(Rejected::ApprovalSuppressed)
            } else if allocation_failed {
                Err(Rejected::Pressure)
            } else {
                Ok(Pixels {
                    metadata: pending.metadata.clone(),
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
    }
}

impl Drop for Readbacks {
    fn drop(&mut self) {
        self.close(Rejected::Closed);
    }
}
