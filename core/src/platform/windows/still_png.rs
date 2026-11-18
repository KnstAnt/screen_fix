//! The attempt's sole native PNG encoder and image publisher.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::{
    capture::frame::CaptureColor,
    package::{
        ArtifactKind, ArtifactRef, ImageDigest, raw_relative,
        diagnostics::{Failure, FailureComponent},
        journal::{Flush, JournalIngress},
        metadata::{AttemptLayout, ImageFile, LostActivity, PngCounters},
        outcome::HealthKind,
    },
    protocol::MonitorId,
    platform::windows::controller::{PauseProducer, WorkerSession},
    session::fault::FaultReport,
};

use super::{ProbeError, clock, readback::Pixels, still_rows::{Awake, Merge, Rows, StillCoverage}};

const JOBS: usize = 12;
const MAX_NEW_IMAGES: usize = 6;
const WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Priority {
    Idle,
    Final,
    Start,
    Checkpoint,
    Settled,
    Next,
    BeforeSeen,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Dropped {
    Capacity,
    LowerPriority,
    RateLimit,
    WorkerClosed,
    Deadline,
    ApprovalSuppressed,
    /// The PNG encoder stopped for the rest of the attempt (§6.2 row 16).
    PngWorkerUnavailable,
}

#[derive(Debug)]
pub(super) enum Outcome {
    Saved { reference: ArtifactRef, reused: bool },
    Dropped(Dropped),
    Failed { operation: &'static str, native_code: Option<i32> },
}

struct Job {
    offered_at: Instant,
    pixels: Pixels,
    coverage: Option<StillCoverage>,
    priority: Priority,
    exempt: bool,
    reply: SyncSender<Outcome>,
    /// Set when the requesting slot gave up the job; checked before the image
    /// is written and before it is published.
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    senders: AtomicUsize,
    offered: AtomicU64,
    rejected: AtomicU64,
    evicted: AtomicU64,
    peak: AtomicUsize,
}

pub(super) struct Inbox(Arc<Queue>);

impl Clone for Inbox {
    fn clone(&self) -> Self {
        self.0.senders.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(&self.0))
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.0.senders.fetch_sub(1, Ordering::Release);
        self.0.wake.notify_one();
    }
}

impl Inbox {
    pub fn request(&self, pixels: Pixels, priority: Priority, exempt: bool, coverage: Option<StillCoverage>) -> Result<Ticket, Dropped> {
        let offered_at = Instant::now();
        self.0.offered.fetch_add(1, Ordering::Relaxed);
        let (reply, result) = sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        // The worker holds the lock only to pop a job, so this wait is short.
        let mut jobs = self.0.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if jobs.len() >= JOBS {
            let lowest = jobs.iter().map(|job| job.priority).min().expect("the PNG queue is full");
            if lowest >= priority {
                self.0.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(Dropped::Capacity);
            }
            let index = jobs.iter().rposition(|job| job.priority == lowest).expect("lowest priority exists");
            if let Some(evicted) = jobs.remove(index) {
                self.0.evicted.fetch_add(1, Ordering::Relaxed);
                let _ = evicted.reply.try_send(Outcome::Dropped(Dropped::LowerPriority));
            }
        }
        jobs.push_back(Job { offered_at, pixels, coverage, priority, exempt, reply, cancelled: Arc::clone(&cancelled) });
        self.0.peak.fetch_max(jobs.len(), Ordering::Relaxed);
        drop(jobs);
        self.0.wake.notify_one();
        Ok(Ticket { result, cancelled })
    }
}

/// Dropping an unfinished ticket cancels its job.
pub(super) struct Ticket {
    result: Receiver<Outcome>,
    cancelled: Arc<AtomicBool>,
}

impl Ticket {
    pub fn poll(&self) -> Option<Outcome> {
        match self.result.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Disconnected) => Some(Outcome::Dropped(Dropped::WorkerClosed)),
            Err(TryRecvError::Empty) => None,
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub(super) struct Worker {
    input: Arc<Queue>,
    rows: Merge,
    rate: HashMap<MonitorId, VecDeque<Instant>>,
    /// The operation of the current PNG write failure episode, reported once.
    failing: Option<&'static str>,
    /// The encoder stopped after an unexpected exit; jobs are dropped (§6.2 row 16).
    encoder_lost: bool,
    /// Every image this worker published (M1-09 output inventory).
    images: Vec<ImageFile>,
    /// Images whose authorization was revoked after their rename and whose
    /// removal failed. Such a file is never reused as a deduplicated image.
    revoked: HashSet<ImageDigest>,
}

impl Worker {
    pub fn new(lanes: usize) -> (Inbox, Vec<Rows>, Self) {
        let input = Arc::new(Queue::default());
        input.senders.store(1, Ordering::Relaxed);
        let (rows, merge) = Merge::new(lanes);
        (Inbox(Arc::clone(&input)), rows, Self {
            input, rows: merge, rate: HashMap::new(), failing: None, encoder_lost: false, images: Vec::new(),
            revoked: HashSet::new(),
        })
    }

    pub fn run(mut self, layout: &AttemptLayout, journal: JournalIngress, session: &WorkerSession)
        -> (PngCounters, Vec<ImageFile>, Result<(), ProbeError>)
    {
        let mut counters = PngCounters::default();
        let result = self.drain(layout, journal, session, &mut counters);
        counters.unreferenced_images = self.unreferenced_images(layout);
        counters.jobs_offered = self.input.offered.load(Ordering::Relaxed);
        counters.jobs_rejected = self.input.rejected.load(Ordering::Relaxed);
        counters.jobs_evicted = self.input.evicted.load(Ordering::Relaxed);
        counters.queue_peak = self.input.peak.load(Ordering::Relaxed);
        (counters, self.images, result)
    }

    fn drain(&mut self, layout: &AttemptLayout, mut journal: JournalIngress, session: &WorkerSession,
        counters: &mut PngCounters,
    ) -> Result<(), ProbeError> {
        let mut flushed = None;
        let mut revision = session.journal_flush_revision();
        let frequency = clock::frequency()?;
        loop {
            let awake = Awake { now: clock::qpc()?, marks: session.suspend_marks(), frequency };
            self.rows.publish(&mut journal, Instant::now(), &awake)?;
            if let Some(pause) = session.paused_cutoff().filter(|pause| flushed != Some(*pause))
                && self.rows.includes(pause.time)
            {
                journal.flush_wait(Flush::Os, pause.time)?;
                flushed = Some(pause);
                session.finish_pause(pause, PauseProducer::Stills);
            }
            let latest = session.journal_flush_revision();
            if latest != revision {
                journal.flush_wait(Flush::Os, self.rows.latest_receipt())?;
                revision = latest;
            }
            if let Some(mut job) = self.take_job() {
                let started = Instant::now();
                let waited = u64::try_from(started.duration_since(job.offered_at).as_micros()).unwrap_or(u64::MAX);
                counters.queue_wait_max_us = counters.queue_wait_max_us.max(waited);
                let outcome = self.encode(layout, &mut job, session);
                let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                counters.processing_max_us = counters.processing_max_us.max(elapsed);
                match &outcome {
                    Outcome::Saved { reused: false, .. } => counters.new_images += 1,
                    Outcome::Saved { reused: true, .. } => counters.deduplicated_jobs += 1,
                    Outcome::Dropped(_) => counters.dropped_at_processing += 1,
                    Outcome::Failed { .. } => counters.failed_jobs += 1,
                }
                match &outcome {
                    Outcome::Saved { .. } => self.failing = None,
                    Outcome::Failed { operation, native_code } if self.failing != Some(*operation) => {
                        // One context health row per failure kind episode; each
                        // still row keeps its own operation and native code.
                        self.failing = Some(*operation);
                        session.report_health(HealthKind::StillWriteFailure,
                            Failure::new(FailureComponent::Stills, *operation, *native_code), None, false);
                    }
                    _ => {}
                }
                // A slot that gave up the job after its image was published
                // leaves the file without a row; it is kept and counted at the end.
                let _ = job.reply.try_send(outcome);
            } else if self.input.senders.load(Ordering::Acquire) == 0 {
                break;
            }
        }
        self.rows.finish(&mut journal)
    }

    /// Image files that no journaled row names: published images whose slot
    /// gave up the job, fell back, was withdrawn or lost its row, and revoked
    /// images whose removal still fails. The files are kept.
    fn unreferenced_images(&self, layout: &AttemptLayout) -> u64 {
        let published = self.images.iter()
            .filter(|image| raw_relative(&image.file).is_none_or(|file| !self.rows.references(file)))
            .count();
        let revoked = self.revoked.iter()
            .filter(|digest| fs::remove_file(layout.path(ArtifactKind::StillImage(**digest)))
                .is_err_and(|error| error.kind() != io::ErrorKind::NotFound))
            .count();
        u64::try_from(published + revoked).unwrap_or(u64::MAX)
    }

    fn take_job(&self) -> Option<Job> {
        let queue = &self.input;
        let mut jobs = queue.jobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if jobs.is_empty() && queue.senders.load(Ordering::Acquire) != 0 {
            jobs = queue.wake.wait_timeout(jobs, Duration::from_millis(5))
                .unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        let highest = jobs.iter().map(|job| job.priority).max()?;
        let index = jobs.iter().position(|job| job.priority == highest)?;
        jobs.remove(index)
    }

    /// Encode one job. An unexpected exit of the encoder is an auxiliary
    /// activity loss (§6.2 row 16): it is reported once, this and every later
    /// job resolves as `png_worker_unavailable`, and the still journal and the
    /// recording continue. Journal failures stay attempt failures (row 1).
    fn encode(&mut self, layout: &AttemptLayout, job: &mut Job, session: &WorkerSession) -> Outcome {
        if !self.encoder_lost {
            match catch_unwind(AssertUnwindSafe(|| self.save(layout, job, session))) {
                Ok(outcome) => return outcome,
                Err(_) => {
                    self.encoder_lost = true;
                    let failure = Failure::new(FailureComponent::Stills, "Encode and publish a still image", None);
                    session.report_health(HealthKind::AuxiliaryActivityLoss, failure, None, true);
                    session.report_fault(FaultReport::activity(LostActivity::Auxiliary, failure, None, session.session_time()));
                }
            }
        }
        Outcome::Dropped(Dropped::PngWorkerUnavailable)
    }

    fn save(&mut self, layout: &AttemptLayout, job: &mut Job, session: &WorkerSession) -> Outcome {
        let cancelled = Arc::clone(&job.cancelled);
        let expired = || cancelled.load(Ordering::Acquire)
            || session.stop_deadline().is_some_and(|deadline| Instant::now() >= deadline);
        if expired() { return Outcome::Dropped(Dropped::Deadline); }
        let pixels = &mut job.pixels;
        let metadata = pixels.metadata.clone();
        let coverage = &job.coverage;
        let authorized = || metadata.covered() && coverage.as_ref().is_none_or(StillCoverage::allows);
        if !authorized() { return Outcome::Dropped(Dropped::ApprovalSuppressed); }
        let mut hash = blake3::Hasher::new();
        hash.update(b"screen-fix-still\0v1\0packed-bgra8\0");
        hash.update(&metadata.extent.width.get().to_le_bytes());
        hash.update(&metadata.extent.height.get().to_le_bytes());
        hash.update(&[match metadata.color {
            CaptureColor::Sdr => 1,
            CaptureColor::HdrToneMappedToSdr => 2,
            CaptureColor::HdrSystemBgra8 => 3,
        }]);
        hash.update(pixels.bgra());

        let digest = ImageDigest(*hash.finalize().as_bytes());
        let kind = ArtifactKind::StillImage(digest);
        let image = layout.path(kind);
        let reference = ArtifactRef { attempt: layout.id().clone(), kind };
        // A revoked image left at its digest path is not an inventoried image
        // (§8): it is removed before this job may publish the same digest.
        if self.revoked.contains(&digest) {
            match fs::remove_file(&image) {
                Ok(()) => { self.revoked.remove(&digest); }
                Err(error) if error.kind() == io::ErrorKind::NotFound => { self.revoked.remove(&digest); }
                Err(error) => return failed("remove_revoked_image", error),
            }
        }
        // The filesystem of this unique attempt is the successful-image index.
        // Temporary images have a disjoint name and cannot produce a hit.
        match image.metadata() {
            Ok(meta) if !meta.is_file() || meta.len() == 0 => {
                return failed("check_image_path", io::Error::from(io::ErrorKind::InvalidData));
            }
            Ok(_) if !authorized() => return Outcome::Dropped(Dropped::ApprovalSuppressed),
            Ok(_) if expired() => return Outcome::Dropped(Dropped::Deadline),
            Ok(_) => return Outcome::Saved { reference, reused: true },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return failed("check_image_path", error),
        }
        let rate = self.rate.entry(metadata.id.monitor).or_default();
        let now = Instant::now();
        while rate.front().is_some_and(|old| now.duration_since(*old) >= WINDOW) {
            rate.pop_front();
        }
        if !job.exempt && rate.len() >= MAX_NEW_IMAGES {
            return Outcome::Dropped(Dropped::RateLimit);
        }

        let temporary = image.with_extension("png.pending");
        let mut file = match OpenOptions::new().create_new(true).write(true).open(&temporary) {
            Ok(file) => file,
            Err(error) => return failed("create_temporary_image", error),
        };
        for pixel in pixels.bgra_mut().chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let result = (|| -> Result<u64, Outcome> {
            let mut encoder = png::Encoder::new(&mut file, metadata.extent.width.get(), metadata.extent.height.get());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header().map_err(|_| png_failure())?;
            writer.write_image_data(pixels.bgra()).map_err(|_| png_failure())?;
            writer.finish().map_err(|_| png_failure())?;
            file.sync_all().map_err(|error| failed("flush_image", error))?;
            file.metadata().map(|meta| meta.len()).map_err(|error| failed("inspect_image", error))
        })();
        drop(file);
        let result = result.and_then(|bytes| {
            if expired() { return Err(Outcome::Dropped(Dropped::Deadline)); }
            if !authorized() { return Err(Outcome::Dropped(Dropped::ApprovalSuppressed)); }
            move_new(&temporary, &image).map_err(|error| failed("publish_image", error))?;
            if !authorized() {
                fs::remove_file(&image).map_err(|error| {
                    self.revoked.insert(digest);
                    failed("remove_revoked_image", error)
                })?;
                return Err(Outcome::Dropped(Dropped::ApprovalSuppressed));
            }
            Ok(bytes)
        });
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(outcome) => {
                let _ = fs::remove_file(&temporary);
                return outcome;
            }
        };
        if !job.exempt {
            rate.push_back(Instant::now());
        }
        self.images.push(ImageFile { file: reference.relative_uri(), bytes,
            width: metadata.extent.width.get(), height: metadata.extent.height.get() });
        Outcome::Saved { reference, reused: false }
    }
}

fn png_failure() -> Outcome {
    Outcome::Failed { operation: "encode_png", native_code: None }
}

fn failed(operation: &'static str, error: io::Error) -> Outcome {
    Outcome::Failed { operation, native_code: error.raw_os_error() }
}

fn move_new(source: &Path, destination: &Path) -> io::Result<()> {
    // Same-directory move without REPLACE_EXISTING preserves immutability.
    super::move_file(source, destination, false)
}
