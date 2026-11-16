//! The attempt's sole native PNG encoder and image publisher.

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

use crate::{
    capture::frame::CaptureColor,
    package::{ArtifactKind, ArtifactRef, ImageDigest, journal::{Flush, JournalIngress}, metadata::{AttemptLayout, PngCounters}},
    protocol::MonitorId,
    session::windows::{PauseProducer, WorkerSession},
};

use super::{ProbeError, package_journal_error, readback::Pixels, still_rows::{Merge, Rows}};

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
    priority: Priority,
    exempt: bool,
    reply: SyncSender<Outcome>,
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
    pub fn request(&self, pixels: Pixels, priority: Priority, exempt: bool) -> Result<Ticket, Dropped> {
        let offered_at = Instant::now();
        self.0.offered.fetch_add(1, Ordering::Relaxed);
        let (reply, result) = sync_channel(1);
        let mut jobs = self.0.jobs.try_lock().map_err(|_| {
            self.0.rejected.fetch_add(1, Ordering::Relaxed);
            Dropped::Capacity
        })?;
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
        jobs.push_back(Job { offered_at, pixels, priority, exempt, reply });
        self.0.peak.fetch_max(jobs.len(), Ordering::Relaxed);
        drop(jobs);
        self.0.wake.notify_one();
        Ok(Ticket(result))
    }
}

pub(super) struct Ticket(Receiver<Outcome>);

impl Ticket {
    pub fn poll(&self) -> Option<Outcome> {
        match self.0.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Disconnected) => Some(Outcome::Dropped(Dropped::WorkerClosed)),
            Err(TryRecvError::Empty) => None,
        }
    }
}

pub(super) struct Worker {
    input: Arc<Queue>,
    rows: Merge,
    rate: HashMap<MonitorId, VecDeque<Instant>>,
}

impl Worker {
    pub fn new(lanes: usize) -> (Inbox, Vec<Rows>, Self) {
        let input = Arc::new(Queue::default());
        input.senders.store(1, Ordering::Relaxed);
        let (rows, merge) = Merge::new(lanes);
        (Inbox(Arc::clone(&input)), rows, Self { input, rows: merge, rate: HashMap::new() })
    }

    pub fn run(mut self, layout: &AttemptLayout, journal: JournalIngress, session: &WorkerSession)
        -> (PngCounters, Result<(), ProbeError>)
    {
        let mut counters = PngCounters::default();
        let result = self.drain(layout, journal, session, &mut counters);
        counters.jobs_offered = self.input.offered.load(Ordering::Relaxed);
        counters.jobs_rejected = self.input.rejected.load(Ordering::Relaxed);
        counters.jobs_evicted = self.input.evicted.load(Ordering::Relaxed);
        counters.queue_peak = self.input.peak.load(Ordering::Relaxed);
        (counters, result)
    }

    fn drain(&mut self, layout: &AttemptLayout, mut journal: JournalIngress, session: &WorkerSession,
        counters: &mut PngCounters,
    ) -> Result<(), ProbeError> {
        let mut flushed = None;
        let mut revision = session.journal_flush_revision();
        loop {
            self.rows.publish(&mut journal, Instant::now())?;
            if let Some(pause) = session.paused_cutoff().filter(|pause| flushed != Some(*pause))
                && self.rows.includes(pause.time)
            {
                journal.flush_wait(Flush::Os, pause.time).map_err(package_journal_error)?;
                flushed = Some(pause);
                session.finish_pause(pause, PauseProducer::Stills);
            }
            let latest = session.journal_flush_revision();
            if latest != revision {
                journal.flush_wait(Flush::Os, self.rows.latest_receipt()).map_err(package_journal_error)?;
                revision = latest;
            }
            if let Some(mut job) = self.take_job() {
                let started = Instant::now();
                let waited = u64::try_from(started.duration_since(job.offered_at).as_micros()).unwrap_or(u64::MAX);
                counters.queue_wait_max_us = counters.queue_wait_max_us.max(waited);
                let outcome = self.save(layout, &mut job, session);
                let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                counters.processing_max_us = counters.processing_max_us.max(elapsed);
                match &outcome {
                    Outcome::Saved { reused: false, .. } => counters.new_images += 1,
                    Outcome::Saved { reused: true, .. } => counters.deduplicated_jobs += 1,
                    Outcome::Dropped(_) => counters.dropped_at_processing += 1,
                    Outcome::Failed { .. } => counters.failed_jobs += 1,
                }
                let _ = job.reply.try_send(outcome);
            } else if self.input.senders.load(Ordering::Acquire) == 0 {
                break;
            }
        }
        self.rows.finish(&mut journal)
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

    fn save(&mut self, layout: &AttemptLayout, job: &mut Job, session: &WorkerSession) -> Outcome {
        let expired = || session.stop_deadline().is_some_and(|deadline| Instant::now() >= deadline);
        if expired() { return Outcome::Dropped(Dropped::Deadline); }
        let pixels = &mut job.pixels;
        let metadata = pixels.metadata;
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

        let kind = ArtifactKind::StillImage(ImageDigest(*hash.finalize().as_bytes()));
        let image = layout.path(kind);
        let reference = ArtifactRef { attempt: layout.id().clone(), kind };
        // The filesystem of this unique attempt is the successful-image index.
        // Temporary images have a disjoint name and cannot produce a hit.
        match image.metadata() {
            Ok(meta) if !meta.is_file() || meta.len() == 0 => {
                return failed("check_image_path", io::Error::from(io::ErrorKind::InvalidData));
            }
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
        let result = (|| -> Result<(), Outcome> {
            let mut encoder = png::Encoder::new(&mut file, metadata.extent.width.get(), metadata.extent.height.get());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header().map_err(|_| png_failure())?;
            writer.write_image_data(pixels.bgra()).map_err(|_| png_failure())?;
            writer.finish().map_err(|_| png_failure())?;
            file.sync_all().map_err(|error| failed("flush_image", error))
        })();
        drop(file);
        let result = result.and_then(|()| {
            if expired() { return Err(Outcome::Dropped(Dropped::Deadline)); }
            move_new(&temporary, &image).map_err(|error| failed("publish_image", error))
        });
        if let Err(outcome) = result {
            let _ = fs::remove_file(&temporary);
            return outcome;
        }
        if !job.exempt {
            rate.push_back(Instant::now());
        }
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
    let wide = |path: &Path| path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
    let source = wide(source);
    let destination = wide(destination);
    // Same-directory move without REPLACE_EXISTING preserves immutability.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
