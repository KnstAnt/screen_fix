use std::{
    sync::{Arc, RwLock, atomic::{AtomicBool, Ordering}, mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel}},
    thread,
    time::{Duration, Instant},
};

use windows::{
    Graphics::{Capture::{GraphicsCaptureItem, GraphicsCaptureSession}, SizeInt32},
    Win32::{
        Graphics::{
            Direct3D11::{ID3D11Device, ID3D11Texture2D},
            Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT},
        },
        System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop,
    },
    core,
};

use crate::{
    devlog::devlog,
    capture::frame::{CaptureColor, FrameExtent, FrameMetadata, MonitorGeometry},
    package::{diagnostics::{Failure, FailureComponent}, metadata::{CaptureCounters, MonitorCapture}, outcome::HealthKind},
    protocol::{AdmissionEpoch, CaptureEpoch, FrameId, FrameSequence, GeometryEpoch, GeometryId, MonitorId, QpcTick, SessionTimeUs},
    config::SelectedMonitor,
    platform::windows::controller::{Admission, PauseProducer, Receipt, SuspendWatch, WorkerSession},
    session::{AdmissionBoundary, fault::FaultReport},
    util::widen,
};

use super::{
    context::{ELEVATED_FRAME_WAIT_US, FrameApproval, FramePolicy}, ProbeError, capture_device, clock, com_apartment,
    display::{self, topology::{Output, Snapshot}}, encoder, gpu, hdr, native, pixels, readback, slots, stage, still_png,
    still_rows, stills, unavailable, video, wgc,
};

/// The bound on one GPU video conversion.
pub(super) const CONVERSION_DEADLINE: Duration = Duration::from_secs(3);
const CONVERSION_TIMED_OUT: &str = "GPU video conversion did not complete within 3 seconds";
/// The wait of an elevated frame's finished conversion for its approval.
const APPROVAL_POLL: Duration = Duration::from_millis(1);
/// The capture loop's wait while GPU work or readbacks are pending.
const BUSY_POLL: Duration = Duration::from_millis(1);
/// The capture loop's longest wait for a frame while idle.
const IDLE_POLL: Duration = Duration::from_millis(10);
/// The capture loop's wait while the monitor is missing or reconfiguring.
const RECONFIGURE_POLL: Duration = Duration::from_millis(5);
/// The bound on draining the stills of a pause or Stop boundary.
const STILL_DRAIN_WAIT: Duration = Duration::from_secs(3);
/// The interval between device-removal checks of the capture GPU.
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// The largest lead of a WGC present timestamp over its dequeue that is
/// accepted as a present time: two refresh intervals at 48 Hz.
const MAX_PRESENT_LEAD_US: i64 = 42_000;
/// How long after a frame's content time an ordinary approval may still be
/// observed through it: several context polls (2-5 ms each).
const MAX_APPROVAL_CATCH_UP_US: i64 = 20_000;

/// The capture pacing interval: one frame period at the configured pace.
pub(super) fn capture_interval(worker: &WorkerSession) -> Duration {
    Duration::from_nanos(NANOS_PER_SECOND / u64::from(worker.config().profile().capture_pace().frames_per_second()))
}

/// A frame in video conversion, with its conversion deadline.
struct PendingConversion {
    sample: gpu::PendingSample,
    metadata: FrameMetadata,
    deadline: Instant,
}

type Texture = Arc<ID3D11Texture2D>;

pub(super) struct Ready {
    pub capture: MonitorCapture,
    pub stills: stills::Requests,
}

pub(super) struct FormatRequest {
    pub at: SessionTimeUs,
    pub output: Option<Output>,
    /// The capture device for the new output, created by this capture owner;
    /// none when the monitor is missing.
    pub device: Option<ID3D11Device>,
    pub reply: SyncSender<FormatReply>,
}

pub(super) enum FormatReply {
    Missing,
    Ready { device: ID3D11Device, encoding: Option<gpu::Encoding>, output: Output },
    ChangedAgain,
}

pub(super) struct Completion {
    /// The admission boundary capture closed at: none when the recording
    /// never started or closed without one.
    pub outcome: Result<Option<AdmissionBoundary>, ProbeError>,
    pub counters: CaptureCounters,
    pub epochs: Vec<MonitorCapture>,
}

pub(super) struct VideoTarget {
    pub send: SyncSender<video::Observation>,
    pub device: ID3D11Device,
    pub adapter: display::AdapterId,
    pub source_bounds: crate::protocol::PhysicalRect,
    pub encoding: gpu::Encoding,
    pub unavailable: Arc<AtomicBool>,
    pub evidence: Arc<slots::Evidence>,
    pub reconfigure: SyncSender<FormatRequest>,
    /// Encoder self-check pictures requested while a format request is pending.
    pub probes: Receiver<encoder::Probe>,
    pub pacing: Arc<RwLock<Option<(AdmissionEpoch, slots::Timeline)>>>,
}

pub(super) fn run(
    selected: &SelectedMonitor, worker: &WorkerSession,
    frame_policy: &FramePolicy, topology: &display::topology::Topology, borderless: bool, ready: SyncSender<Ready>, video: VideoTarget,
    png: still_png::Inbox, rows: still_rows::Rows,
) -> Completion {
    let mut stop = StopOnError(Some(worker));
    let mut counters = CaptureCounters::default();
    let mut epochs = Vec::new();
    let result = capture(selected, worker, frame_policy, topology, borderless, ready, video, png, rows, &mut counters, &mut epochs)
        .map_err(|error| error.owned_by(FailureComponent::Capture));
    // A capture-thread failure fails the attempt (§6.2 row 4).
    if let Err(error) = &result {
        worker.report_fault(FaultReport::attempt(&error.to_error(FailureComponent::Capture)));
    }
    if result.is_ok() {
        stop.0 = None;
    }
    Completion { outcome: result, counters, epochs }
}

/// The admission check that rejected a dequeued frame (`dev-log` only).
#[derive(Clone, Copy, Debug)]
enum Reject {
    AdmissionClosed,
    BeforeBoundary,
    NegativeSource,
    BeforeGeometry,
    NonMonotonic,
    SourceAfterReceipt,
}

/// Development log of frame admission: the first rejections in full, then
/// one line of per-condition counts per second. Inert without `dev-log`.
#[derive(Default)]
struct AdmissionLog {
    detailed: u32,
    counts: [u64; 6],
    window_start: Option<Instant>,
    accepted: bool,
}

impl AdmissionLog {
    const DETAILED: u32 = 20;
    const WINDOW: Duration = Duration::from_secs(1);

    fn reject(&mut self, reason: Reject, detail: impl FnOnce() -> String) {
        if !crate::devlog::ENABLED {
            return;
        }
        self.counts[reason as usize] += 1;
        if self.detailed < Self::DETAILED {
            self.detailed += 1;
            devlog!("monitor", "frame rejected #{} {reason:?}: {}", self.detailed, detail());
        }
        let now = Instant::now();
        let start = *self.window_start.get_or_insert(now);
        if now.duration_since(start) >= Self::WINDOW {
            self.summarize();
            self.window_start = Some(now);
        }
    }

    fn accept(&mut self, detail: impl FnOnce() -> String) {
        if crate::devlog::ENABLED && !std::mem::replace(&mut self.accepted, true) {
            devlog!("monitor", "first frame accepted by time admission: {}", detail());
        }
    }

    fn summarize(&mut self) {
        if self.counts.iter().any(|&count| count != 0) {
            let [closed, boundary, negative, geometry, monotonic, after] = std::mem::take(&mut self.counts);
            devlog!("monitor", "frames rejected since last summary: AdmissionClosed={closed} BeforeBoundary={boundary} \
                NegativeSource={negative} BeforeGeometry={geometry} NonMonotonic={monotonic} SourceAfterReceipt={after}");
        }
    }
}

impl Drop for AdmissionLog {
    fn drop(&mut self) {
        if crate::devlog::ENABLED {
            self.summarize();
        }
    }
}

struct StopOnError<'a>(Option<&'a WorkerSession>);

impl Drop for StopOnError<'_> {
    fn drop(&mut self) {
        if let Some(worker) = self.0 {
            worker.request_stop();
        }
    }
}

pub(super) fn report_video_ready(worker: &WorkerSession, monitor: MonitorId, encoding: gpu::Encoding) {
    let format = encoding.format;
    worker.video_ready(crate::session::ReadyVideo {
        monitor, physical_size: format.source, encoded_size: format.encoded, fps: format.fps, software: encoding.software,
    });
}

fn capture(
    selected: &SelectedMonitor, worker: &WorkerSession,
    frame_policy: &FramePolicy, topology: &display::topology::Topology, borderless: bool, ready: SyncSender<Ready>, video: VideoTarget,
    png: still_png::Inbox, rows: still_rows::Rows, counters: &mut CaptureCounters, epochs: &mut Vec<MonitorCapture>,
) -> Result<Option<AdmissionBoundary>, ProbeError> {
    let _apartment = com_apartment()?;
    if !stage(GraphicsCaptureSession::IsSupported(), "Check screen capture support")? {
        return Err(unavailable("Windows screen capture is unavailable"));
    }
    let display = display::resolve(&selected.key)?;
    let interop = stage(core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>(),
        "Get the display capture API")?;
    let item: GraphicsCaptureItem = stage(unsafe { interop.CreateForMonitor(display.handle) }, "Select the display to capture")?;
    let size = stage(item.Size(), "Read the display capture size")?;
    if display.size()? != size {
        return Err(unavailable("The monitor physical geometry does not match its capture size"));
    }
    let adapter = stage(unsafe { display.adapter.GetDesc1() }, "Read the capture adapter")?.AdapterLuid;
    if display::AdapterId::from(adapter) != video.adapter || display.bounds != video.source_bounds {
        return Err(unavailable("The primary display geometry or adapter changed during startup"));
    }
    let device = video.device.clone();
    let (capture, color, fallback) = wgc::start_monitor(item, &device, size, display.hdr, display.sdr_white.is_some(), borderless)?;
    report_conversion(worker, selected.id, color, display.sdr_white);
    let painter = pixels::Painter::new(&device)?;
    let images = Resources::new(&device, &painter, size, color, display.sdr_white)?;
    counters.native_texture_bytes_peak = images.native.allocation_bytes();
    let mode = metadata(&display, selected, GeometryEpoch(1), CaptureEpoch(1), &capture, color, fallback, true);
    let output = Output::from_native(&display)?;
    epochs.push(mode.clone());
    topology.capture(mode.clone());
    let converter = gpu::Converter::new(&device, video.encoding)?;
    let still_budget = Arc::new(readback::Budget::default());
    let readbacks = readback::Readbacks::new(Arc::clone(&still_budget));
    let (stills, observations) = stills::Stills::new(rows, still_budget);
    report_video_ready(worker, selected.id, video.encoding);
    ready.try_send(Ready { capture: mode.clone(), stills })
        .map_err(|_| unavailable("The capture coordinator is unavailable"))?;
    let mut owner = CaptureOwner {
        format: FormatState::default(),
        native_capture: Some(capture),
        observations,
        readbacks,
        video: VideoFeed { target: &video, converter: Some(converter), pending: None, waiting: TimelineWait::default() },
        output, mode, images, painter, device, interop,
        selected, worker, frame_policy, topology, borderless, png: &png, counters, epochs,
        reopen_after: None, reconfiguring: false, frozen_final: None,
    };
    let result = owner.run();
    owner.finish(result)
}

/// The capture owner of one monitor: its WGC capture, its images and stills,
/// and its side of the video pipeline.
///
/// Fields drop in declaration order, the order in which these resources were
/// released when they were locals of `capture`: the format request first, the
/// device and the capture interop last.
struct CaptureOwner<'a> {
    format: FormatState,
    native_capture: Option<wgc::Capture>,
    observations: stills::Stills,
    readbacks: readback::Readbacks,
    video: VideoFeed<'a>,
    /// The topology output the capture follows.
    output: Output,
    mode: MonitorCapture,
    images: Resources,
    painter: pixels::Painter,
    device: ID3D11Device,
    interop: IGraphicsCaptureItemInterop,
    selected: &'a SelectedMonitor,
    worker: &'a WorkerSession,
    frame_policy: &'a FramePolicy,
    topology: &'a display::topology::Topology,
    borderless: bool,
    png: &'a still_png::Inbox,
    counters: &'a mut CaptureCounters,
    epochs: &'a mut Vec<MonitorCapture>,
    /// The QPC at which the capture item closed, until capture resumes.
    reopen_after: Option<QpcTick>,
    /// A format request for a changed output is in progress.
    reconfiguring: bool,
    /// At a pause: its boundary, and whether capture was available then.
    frozen_final: Option<(AdmissionBoundary, bool)>,
}

/// The capture owner's format request to the video owner (VID-9).
#[derive(Default)]
struct FormatState {
    pending: Option<PendingFormat>,
    /// The last reply found the monitor missing.
    requested_missing: bool,
    /// The topology snapshot a reply rejected: it is not requested again.
    rejected_snapshot: Option<QpcTick>,
}

/// A request awaiting the video owner's reply.
struct PendingFormat {
    reply: Receiver<FormatReply>,
    /// The device its self-check pictures use; none for a missing monitor.
    probe_device: Option<ID3D11Device>,
}

/// The capture loop's pacing and the time admission state of its frames.
/// It lives as long as the loop.
struct Intake {
    interval: Duration,
    /// The earliest next dequeue.
    scheduled: Instant,
    sequence: u64,
    /// The suspends seen by the conversion deadline.
    suspends: SuspendWatch,
    /// The WGC time of the last frame taken.
    last_source: Option<i64>,
    /// The QPC of the latest geometry change: earlier content is rejected.
    geometry_after: Option<QpcTick>,
    next_device_check: Instant,
    log: AdmissionLog,
}

impl Intake {
    fn new(worker: &WorkerSession) -> Self {
        Self {
            interval: capture_interval(worker), scheduled: Instant::now(), sequence: 0,
            suspends: SuspendWatch::new(worker.suspend_marks()), last_source: None, geometry_after: None,
            next_device_check: Instant::now(), log: AdmissionLog::default(),
        }
    }
}

/// A dequeued frame that passed time admission.
struct Admitted {
    /// The admission at the frame's receipt.
    receipt: Receipt,
    wgc_time_100ns: i64,
    source_time: SessionTimeUs,
    dequeued_time: SessionTimeUs,
    /// The frame's receipt: max(dequeue, source).
    recv_time: SessionTimeUs,
}

impl CaptureOwner<'_> {
    fn run(&mut self) -> Result<Option<AdmissionBoundary>, ProbeError> {
        let Some(start) = self.worker.wait_for_start()? else { return Ok(None) };
        self.observations.arm_start(start.boundary, self.mode.geometry.id);
        let mut intake = Intake::new(self.worker);
        loop {
            let open = match self.worker.admit()? {
                Admission::Closed(Some(cutoff)) if self.worker.pause_pending(cutoff) => {
                    if !self.on_pause(cutoff)? {
                        return Ok(Some(cutoff));
                    }
                    intake.scheduled = Instant::now();
                    continue;
                }
                Admission::Closed(cutoff) => return Ok(cutoff),
                Admission::Open(receipt) => receipt,
                Admission::Starting => return Err(unavailable("Capture admission reopened without a boundary")),
            };
            let now = Instant::now();
            self.check_device(&mut intake, now)?;
            let now_session = open.clock.time(open.qpc)?;
            let snapshot = self.topology.snapshot();
            let latest = snapshot.as_ref().and_then(|snapshot| snapshot.output(&self.selected.key).map(|output| output.cloned()));
            let item_closed = self.native_capture.as_ref().is_some_and(wgc::Capture::item_closed);
            if item_closed || matches!(latest, Some(None)) && self.native_capture.is_some() {
                self.on_capture_lost(item_closed)?;
            }
            let active_geometry = self.on_format_reply(snapshot.as_deref(), latest.as_ref(), &mut intake)?;
            let latest = if self.format.requested_missing && self.reopen_after.zip(snapshot.as_ref())
                .is_some_and(|(closed, snapshot)| snapshot.received <= closed)
            {
                Some(None)
            } else { latest };
            let present = latest.as_ref().and_then(Option::as_ref);
            let format_changed = present.is_some_and(|target| {
                self.native_capture.is_none() || self.reconfiguring || !same_format(&self.output, target)
            });
            let missing = self.native_capture.is_none();
            self.request_format(snapshot.as_deref(), present, format_changed, now_session)?;
            if missing || format_changed || self.format.pending.is_some() {
                self.serve_reconfiguring(now, now_session)?;
                thread::park_timeout(RECONFIGURE_POLL);
                continue;
            }
            let Some(size) = self.native_capture.as_ref().map(|capture| capture.size) else { continue };
            if active_geometry.is_none() && let Some(current) = present && self.output != *current {
                self.on_geometry_change(current, &mut intake)?;
            }
            let epoch = open.boundary.epoch;
            let timeline = self.video.timeline(epoch);
            if let Some(timeline) = timeline {
                self.video.release_waiting(timeline, epoch, size, now, self.counters)?;
            }
            let elevated = self.worker.elevated_epoch(epoch);
            if !elevated {
                self.step_stills(epoch, now_session);
            }
            self.readbacks.poll(&self.images.native, now)?;
            self.observations.work(&self.device, &self.images.native, &mut self.readbacks, self.png, now_session, now)?;
            self.poll_thumbnails(timeline, epoch)?;
            if self.video.target.unavailable.load(Ordering::Acquire) {
                self.video.discard(self.counters);
                self.video.converter = None;
            }
            // Time asleep does not count against the conversion bound (D-5 A).
            if intake.suspends.slept(self.worker.suspend_marks()) {
                self.video.restart_deadline();
            }
            if self.video.deliver(&open, self.frame_policy, &self.images.native, timeline, now, self.counters)? {
                thread::park_timeout(APPROVAL_POLL);
                continue;
            }
            if let Ok(approval) = self.frame_policy.try_current(epoch, now_session) {
                self.images.release_staged(approval.as_ref(), now_session, &self.painter, &mut self.video, timeline,
                    size, now, self.counters)?;
            }
            if elevated {
                self.step_stills(epoch, now_session);
            }
            let sleep = if self.video.pending.is_some() || self.images.thumbnails.has_pending() || self.readbacks.has_pending() {
                BUSY_POLL
            } else {
                IDLE_POLL
            };
            if now < intake.scheduled {
                thread::park_timeout(intake.scheduled.duration_since(now).min(sleep));
                continue;
            }
            let Some(capture) = &self.native_capture else { continue };
            let Some(borrowed) = capture.newest()? else {
                thread::park_timeout(sleep);
                continue;
            };
            // Receipt is sampled by WGC at dequeue, before this admission check.
            self.counters.frames_dequeued = self.counters.frames_dequeued.saturating_add(1);
            let Ok(frame) = self.admit_frame(&borrowed, &open, &mut intake)? else {
                self.counters.rejected_time_or_epoch = self.counters.rejected_time_or_epoch.saturating_add(1);
                borrowed.close()?;
                continue;
            };
            let Some(approval) = self.await_approval(&frame)? else {
                self.counters.rejected_approval = self.counters.rejected_approval.saturating_add(1);
                borrowed.close()?;
                intake.scheduled = now + intake.interval;
                continue;
            };
            self.take_frame(borrowed, frame, approval, timeline, size, now, &mut intake)?;
        }
    }

    /// Close capture after the loop: Stop drains its boundary's stills, then
    /// the readbacks and the WGC capture close.
    fn finish(mut self, result: Result<Option<AdmissionBoundary>, ProbeError>) -> Result<Option<AdmissionBoundary>, ProbeError> {
        self.video.end_loop();
        let result = result.and_then(|exit| {
            if let Some(cutoff) = exit {
                let capture_available = self.frozen_final.filter(|(boundary, _)| *boundary == cutoff).map_or_else(
                    || self.native_capture.as_ref().is_some_and(|capture| !capture.item_closed()) && !self.reconfiguring,
                    |(_, available)| available,
                );
                finish_stills(self.worker, cutoff, self.worker.stop_boundary().unwrap_or(cutoff), true, capture_available,
                    &self.mode, &self.device, &self.images.native, &mut self.observations, &mut self.readbacks, self.png)?;
            }
            Ok(exit)
        });
        self.readbacks.close(if result.is_ok() { readback::Rejected::Closed } else { readback::Rejected::CaptureFailed });
        let close = self.native_capture.take().map_or(Ok(()), wgc::Capture::close);
        result.and_then(|result| close.map(|()| result))
    }

    /// A pause boundary: drop the video work, drain the boundary's stills and
    /// wait for the reopening. Returns false when the recording stopped instead.
    fn on_pause(&mut self, cutoff: AdmissionBoundary) -> Result<bool, ProbeError> {
        self.frozen_final = Some((cutoff, self.native_capture.as_ref().is_some_and(|capture| !capture.item_closed())
            && !self.reconfiguring));
        self.video.discard(self.counters);
        self.images.native.clear_staged();
        finish_stills(self.worker, cutoff, cutoff, false, false, &self.mode, &self.device, &self.images.native,
            &mut self.observations, &mut self.readbacks, self.png)?;
        self.worker.finish_pause(cutoff, PauseProducer::Capture(self.selected.id));
        let Some(opened) = self.worker.wait_for_reopen(cutoff)? else {
            return Ok(false);
        };
        self.frozen_final = None;
        self.images.native.clear_history();
        self.images.thumbnails = pixels::Thumbnails::new(&self.device, &self.painter)?;
        self.observations.arm_start(opened.boundary, self.mode.geometry.id);
        Ok(true)
    }

    fn check_device(&mut self, intake: &mut Intake, now: Instant) -> Result<(), ProbeError> {
        if self.native_capture.is_some() && (now >= intake.next_device_check
            || self.native_capture.as_ref().is_some_and(wgc::Capture::item_closed))
        {
            stage(unsafe { self.device.GetDeviceRemovedReason() }, "Check the monitor GPU device")?;
            intake.next_device_check = now + DEVICE_CHECK_INTERVAL;
        }
        Ok(())
    }

    /// The capture item closed or the monitor left the topology: capture
    /// stops under a new geometry epoch until the monitor is requested again.
    fn on_capture_lost(&mut self, item_closed: bool) -> Result<(), ProbeError> {
        if item_closed { self.reopen_after = Some(clock::qpc()?); }
        self.mode.geometry.id.epoch = next_geometry(self.mode.geometry.id.epoch)?;
        self.topology.missing(self.mode.geometry.id);
        if let Some(capture) = self.native_capture.take() { capture.close()?; }
        self.observations.geometry_changed(self.mode.geometry.id);
        self.video.discard(self.counters);
        self.images.native.clear_staged();
        self.video.converter = None;
        self.reconfiguring = false;
        Ok(())
    }

    /// Take the video owner's reply to a pending format request. Returns the
    /// QPC of the geometry change once the new output is captured.
    fn on_format_reply(
        &mut self, snapshot: Option<&Snapshot>, latest: Option<&Option<Output>>, intake: &mut Intake,
    ) -> Result<Option<QpcTick>, ProbeError> {
        let Some(pending) = &self.format.pending else { return Ok(None) };
        let reply = match pending.reply.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return Ok(None),
            Err(TryRecvError::Disconnected) => {
                if !self.video.target.unavailable.load(Ordering::Acquire) {
                    return Err(unavailable("The monitor's video format owner is unavailable"));
                }
                self.format.pending = None;
                return Ok(None);
            }
        };
        self.format.pending = None;
        let received = snapshot.map(|snapshot| snapshot.received);
        match reply {
            FormatReply::Ready { device, encoding, output } => {
                self.format.requested_missing = false;
                let active = if latest.is_some_and(|current| current.as_ref() == Some(&output))
                    && !self.native_capture.as_ref().is_some_and(wgc::Capture::item_closed)
                {
                    self.activate(device, encoding, output, intake)?
                } else {
                    None
                };
                if active.is_none() {
                    self.format.rejected_snapshot = received;
                }
                Ok(active)
            }
            FormatReply::Missing => {
                self.format.requested_missing = true;
                Ok(None)
            }
            FormatReply::ChangedAgain => {
                self.format.rejected_snapshot = received;
                Ok(None)
            }
        }
    }

    /// Capture the output a ready reply configured, on the reply's device
    /// unless the Windows monitor item is unchanged. A failed read proves
    /// nothing about the monitor: the request is retried on the next topology
    /// tick (VID-9).
    fn activate(
        &mut self, configured: ID3D11Device, encoding: Option<gpu::Encoding>, target: Output, intake: &mut Intake,
    ) -> Result<Option<QpcTick>, ProbeError> {
        let Some(actual) = display::resolve_matching(&self.selected.key, &target) else { return Ok(None) };
        let next = next_geometry(self.mode.geometry.id.epoch)?;
        let epoch = next_capture(self.mode.capture_epoch)?;
        let size = actual.size()?;
        let same_item = self.native_capture.is_some() && self.output.same_item(&target);
        if !same_item {
            if let Some(old) = self.native_capture.take() { old.close()?; }
            self.observations.expire_native_epoch();
            self.readbacks.cancel_epoch(readback::Rejected::EpochMismatch);
            self.device = configured;
            self.painter = pixels::Painter::new(&self.device)?;
        }
        let (capture, color, fallback) = if same_item {
            let mut capture = self.native_capture.take()
                .ok_or_else(|| unavailable("The existing monitor capture item is unavailable"))?;
            let (color, fallback) = capture.reconfigure(size, actual.hdr, actual.sdr_white.is_some())?;
            (capture, color, fallback)
        } else {
            let item: GraphicsCaptureItem = stage(unsafe { self.interop.CreateForMonitor(actual.handle) },
                "Create the returned monitor capture item")?;
            wgc::start_monitor(item, &self.device, size, actual.hdr, actual.sdr_white.is_some(), self.borderless)?
        };
        self.mode = metadata(&actual, self.selected, next, epoch, &capture, color, fallback, encoding.is_some());
        report_conversion(self.worker, self.selected.id, color, actual.sdr_white);
        if same_item {
            self.observations.expire_native_epoch();
            self.readbacks.cancel_epoch(readback::Rejected::EpochMismatch);
        }
        self.images = Resources::new(&self.device, &self.painter, size, color, actual.sdr_white)?;
        self.video.converter = encoding.map(|encoding| gpu::Converter::new(&self.device, encoding)).transpose()?;
        self.native_capture = Some(capture);
        self.output = target;
        self.video.discard(self.counters);
        intake.last_source = None;
        let changed = clock::qpc()?;
        intake.geometry_after = Some(changed);
        self.observations.geometry_changed(self.mode.geometry.id);
        self.epochs.push(self.mode.clone());
        self.topology.capture(self.mode.clone());
        self.counters.native_texture_bytes_peak = self.counters.native_texture_bytes_peak.max(self.images.native.allocation_bytes());
        self.reconfiguring = false;
        self.reopen_after = None;
        Ok(Some(changed))
    }

    /// Ask the video owner for the missing monitor or a changed output,
    /// unless a request is pending or a reply rejected this snapshot.
    fn request_format(
        &mut self, snapshot: Option<&Snapshot>, present: Option<&Output>, format_changed: bool, now_session: SessionTimeUs,
    ) -> Result<(), ProbeError> {
        let needs_missing = self.native_capture.is_none() && !self.format.requested_missing;
        let received = snapshot.map(|snapshot| snapshot.received);
        if self.format.pending.is_some() || !(needs_missing || format_changed)
            || received.is_some_and(|received| self.format.rejected_snapshot == Some(received))
        {
            return Ok(());
        }
        let target = if needs_missing { None } else { present.cloned() };
        // A new adapter's device is created here, by the owner of its
        // immediate context. A failed read is retried on the next topology snapshot.
        let request_device = match &target {
            None => Some(None),
            Some(target) if self.native_capture.is_some() && self.output.same_item(target) => Some(Some(self.device.clone())),
            Some(target) => display::resolve(&target.key).ok()
                .and_then(|actual| capture_device(&actual.adapter).ok()).map(Some),
        };
        let (reply, result) = sync_channel(1);
        let sent = request_device.clone()
            .map(|device| self.video.target.reconfigure.try_send(FormatRequest { at: now_session, output: target, device, reply }));
        match sent {
            None => self.format.rejected_snapshot = received,
            Some(Ok(())) => {
                self.format.pending = Some(PendingFormat { reply: result, probe_device: request_device.flatten() });
                if !needs_missing && !self.reconfiguring {
                    self.topology.reconfiguring(self.mode.geometry.id);
                    self.reconfiguring = true;
                }
                self.video.discard(self.counters);
                self.images.native.clear_staged();
            }
            Some(Err(TrySendError::Full(_))) => {}
            Some(Err(TrySendError::Disconnected(_))) => return Err(unavailable("The video format owner closed unexpectedly")),
        }
        Ok(())
    }

    /// While the monitor is missing or reconfiguring, the stills keep working
    /// and the video owner's self-check pictures are converted here (VID-12).
    fn serve_reconfiguring(&mut self, now: Instant, now_session: SessionTimeUs) -> Result<(), ProbeError> {
        if let Some(PendingFormat { probe_device: Some(probe_device), .. }) = &self.format.pending {
            // The video owner qualifies the new format's encoder with
            // pictures converted on this thread (VID-12).
            while let Ok(probe) = self.video.target.probes.try_recv() {
                let _ = probe.reply.try_send(encoder::probe_input(probe_device, probe.encoding));
            }
        }
        self.readbacks.poll(&self.images.native, now)?;
        self.observations.work(&self.device, &self.images.native, &mut self.readbacks, self.png, now_session, now)?;
        self.observations.advance_idle(now_session);
        Ok(())
    }

    /// A new geometry epoch for a changed position, DPI or refresh: the pixel
    /// size and format are unchanged.
    fn on_geometry_change(&mut self, current: &Output, intake: &mut Intake) -> Result<(), ProbeError> {
        let changed = clock::qpc()?;
        self.mode.geometry = MonitorGeometry {
            id: GeometryId { monitor: self.selected.id, epoch: next_geometry(self.mode.geometry.id.epoch)? },
            bounds: current.bounds, dpi_x: current.dpi_x, dpi_y: current.dpi_y,
        };
        self.mode.refresh_numerator = current.refresh.map(|refresh| refresh.0);
        self.mode.refresh_denominator = current.refresh.map(|refresh| refresh.1);
        self.mode.primary = current.primary;
        self.mode.unavailable = current.unavailable.clone();
        self.epochs.push(self.mode.clone());
        self.topology.capture(self.mode.clone());
        self.output = current.clone();
        intake.geometry_after = Some(changed);
        // A conversion of the old geometry is dropped and counted; a frame
        // held for the timeline stays and is judged when the timeline arrives.
        self.video.discard_conversion(self.counters);
        self.images.native.clear_staged();
        self.observations.geometry_changed(self.mode.geometry.id);
        Ok(())
    }

    fn step_stills(&mut self, epoch: AdmissionEpoch, now_session: SessionTimeUs) {
        self.observations.step(&self.images.native, &self.mode, self.frame_policy, self.worker.config(), epoch, now_session);
    }

    /// Record finished thumbnails. A changed frame of this geometry and epoch
    /// is a still candidate, and a clear change marks its slot as changed late.
    fn poll_thumbnails(&mut self, timeline: Option<slots::Timeline>, epoch: AdmissionEpoch) -> Result<(), ProbeError> {
        while let Some((frame, change)) = self.images.thumbnails.poll(self.images.native.context())? {
            self.images.native.record_change(frame, change);
            if change.is_some_and(|score| score > 0.0) && let Some(lease) = self.images.native.lease(frame)
                && lease.metadata.covered() && lease.metadata.geometry.id == self.mode.geometry.id && lease.metadata.admission == epoch
            {
                self.observations.changed(lease.metadata);
            }
            if let (Some(timeline), Some(frame)) = (timeline, self.images.native.lease(frame))
                && change.is_some_and(|change| change > 0.002) && frame.metadata.geometry.id == self.mode.geometry.id
                && frame.metadata.covered() && frame.metadata.admission == epoch
            {
                self.video.target.evidence.changed_late(timeline.containing(frame.metadata.source_t_us)?);
            }
        }
        Ok(())
    }

    /// Time admission of a dequeued frame. A rejection is logged here; the
    /// caller counts it and returns the frame to WGC.
    fn admit_frame(
        &self, borrowed: &wgc::BorrowedFrame, open: &Receipt, intake: &mut Intake,
    ) -> Result<Result<Admitted, Reject>, ProbeError> {
        let received = borrowed.received;
        let receipt = match self.worker.admit()? {
            Admission::Open(receipt) if receipt.boundary == open.boundary => receipt,
            Admission::Closed(_) => {
                intake.log.reject(Reject::AdmissionClosed, || format!("received_qpc={}", received.0));
                return Ok(Err(Reject::AdmissionClosed));
            }
            _ => return Err(unavailable("Monitor capture changed admission during dequeue")),
        };
        let wgc_time_100ns = borrowed.source_100ns()?;
        let clock = receipt.clock;
        let source_offset = clock.wgc_offset_100ns(wgc_time_100ns)?;
        let geometry_offset = intake.geometry_after.map(|at| clock.offset_100ns(at)).transpose()?;
        let last_source = intake.last_source;
        let detail = |source_time: Option<SessionTimeUs>, recv_time: Option<SessionTimeUs>| format!(
            "wgc_time_100ns={wgc_time_100ns} received_qpc={} boundary_qpc={} qpc_frequency={} source_offset_100ns={source_offset} \
             geometry_offset_100ns={geometry_offset:?} source_time_us={:?} recv_time_us={:?} source_minus_recv_us={:?} \
             last_source_100ns={last_source:?}",
            received.0, receipt.boundary.qpc.0, clock.qpc_frequency.0, source_time.map(|time| time.0),
            recv_time.map(|time| time.0), source_time.zip(recv_time).map(|(source, recv)| source.0 - recv.0));
        let reason = if received < receipt.boundary.qpc {
            Some(Reject::BeforeBoundary)
        } else if source_offset < 0 {
            Some(Reject::NegativeSource)
        } else if geometry_offset.is_some_and(|offset| source_offset < offset) {
            Some(Reject::BeforeGeometry)
        } else if last_source.is_some_and(|previous| wgc_time_100ns <= previous) {
            Some(Reject::NonMonotonic)
        } else {
            None
        };
        if let Some(reason) = reason {
            intake.log.reject(reason, || detail(clock.wgc_time(wgc_time_100ns).ok(), clock.time(received).ok()));
            return Ok(Err(reason));
        }
        let source_time = clock.wgc_time(wgc_time_100ns)?;
        let dequeued_time = clock.time(received)?;
        // WGC stamps a frame with its DWM present (vblank) time, which can
        // lead the dequeue by up to a refresh interval. The frame's receipt is
        // therefore max(dequeue, source), so source <= receipt holds (§6.5).
        // A lead beyond `MAX_PRESENT_LEAD_US` is not a present time and is rejected.
        let recv_time = dequeued_time.max(source_time);
        if source_time.0 - dequeued_time.0 > MAX_PRESENT_LEAD_US {
            intake.log.reject(Reject::SourceAfterReceipt, || detail(Some(source_time), Some(dequeued_time)));
            return Ok(Err(Reject::SourceAfterReceipt));
        }
        intake.log.accept(|| detail(Some(source_time), Some(dequeued_time)));
        Ok(Ok(Admitted { receipt, wgc_time_100ns, source_time, dequeued_time, recv_time }))
    }

    /// The approval of an admitted frame's content, or none.
    fn await_approval(&self, frame: &Admitted) -> Result<Option<FrameApproval>, ProbeError> {
        let clock = frame.receipt.clock;
        // The content was composed no later than the dequeue or the present
        // time, so the approval must cover that point. It usually leads the
        // latest foreground observation by less than one context poll; an
        // ordinary approval not yet observed through it is awaited briefly.
        let content_time = frame.source_time.min(frame.dequeued_time);
        let mut approval_at = clock.time(frame.receipt.qpc)?;
        let approval = loop {
            let waiting = approval_at.0 - content_time.0 < MAX_APPROVAL_CATCH_UP_US;
            match self.frame_policy.try_current(frame.receipt.boundary.epoch, approval_at) {
                Ok(Some(approval)) if approval.contains_source(content_time) || approval.can_stage(content_time, approval_at) =>
                    break Some(approval),
                // A contended or newer-than-`approval_at` read is transient.
                Ok(Some(approval)) if approval.awaits_source(content_time) && waiting => {}
                Err(_) if waiting => {}
                _ => break None,
            }
            thread::park_timeout(APPROVAL_POLL);
            approval_at = clock.time(super::clock::qpc()?)?;
        };
        if approval.is_none() {
            devlog!("monitor", "frame rejected by approval: content_time_us={} approval_at_us={}",
                content_time.0, approval_at.0);
        }
        Ok(approval)
    }

    /// Copy an approved frame out of WGC, then publish it, or stage it while
    /// its elevated receipt awaits certification.
    fn take_frame(
        &mut self, borrowed: wgc::BorrowedFrame, frame: Admitted, approval: FrameApproval,
        timeline: Option<slots::Timeline>, size: SizeInt32, now: Instant, intake: &mut Intake,
    ) -> Result<(), ProbeError> {
        if approval.elevated_visit().is_some() && self.images.native.staging_full() {
            borrowed.close()?;
            intake.scheduled = now + intake.interval;
            self.counters.native_pressure = self.counters.native_pressure.saturating_add(1);
            if let Some(timeline) = timeline {
                self.video.target.evidence.capture_pressure(timeline.containing(frame.source_time)?);
            }
            return Ok(());
        }
        let images = &mut self.images;
        let copied = borrowed.with_texture(size, images.format,
            |source| images.native.copy(source, images.tone_map.as_ref()));
        let closed = borrowed.close();
        intake.scheduled = now + intake.interval;
        let owned = match copied.and_then(|copied| closed.map(|()| copied))? {
            Some(Some(owned)) => owned,
            Some(None) => {
                self.counters.native_pressure = self.counters.native_pressure.saturating_add(1);
                return Ok(());
            }
            None => {
                self.counters.mismatched_size = self.counters.mismatched_size.saturating_add(1);
                return Ok(());
            }
        };
        self.images.native.finish_copy(&owned, &self.painter, self.images.tone_map.as_ref())?;
        intake.last_source = Some(frame.wgc_time_100ns);
        intake.sequence = intake.sequence.checked_add(1).ok_or_else(|| unavailable("The monitor frame sequence overflowed"))?;
        let receipt = frame.receipt;
        let metadata = FrameMetadata {
            id: FrameId { monitor: self.selected.id, epoch: self.mode.capture_epoch, sequence: FrameSequence(intake.sequence) },
            geometry: self.mode.geometry, admission: receipt.boundary.epoch,
            approval_generation: approval.generation, source_t_us: frame.source_time,
            coverage: approval.coverage.clone(),
            recv_t_us: frame.recv_time,
            source_uncertainty_us: None, color: self.mode.conversion, extent: self.images.extent,
        };
        let publish_at = receipt.clock.time(clock::qpc()?)?;
        let current = self.frame_policy.current(receipt.boundary.epoch, publish_at);
        if !permit_m1_publication(&metadata, self.selected, receipt.boundary)
            || current.is_none_or(|approval| !approval.owns(&metadata)) || !metadata.covered()
        {
            self.counters.rejected_approval = self.counters.rejected_approval.saturating_add(1);
            return Ok(());
        }
        if metadata.elevated_visit().is_some() {
            // The existing GPU copy enters private staging. History, thumbnails
            // and conversion see the texture only after its receipt is certified.
            self.images.native.stage(owned, metadata);
            return Ok(());
        }
        self.images.publish(owned, metadata, &self.painter, &mut self.video, timeline, size, now, self.counters)?;
        let now_session = receipt.clock.time(receipt.qpc)?;
        self.step_stills(receipt.boundary.epoch, now_session);
        Ok(())
    }
}

fn finish_stills(
    worker: &WorkerSession, prefix: AdmissionBoundary, cutoff: AdmissionBoundary, stopping: bool,
    capture_available: bool, mode: &MonitorCapture,
    device: &ID3D11Device, images: &native::NativeImages,
    observations: &mut stills::Stills, readbacks: &mut readback::Readbacks, png: &still_png::Inbox,
) -> Result<(), ProbeError> {
    observations.finish_boundary(prefix, cutoff, images, mode, stopping, capture_available);
    let deadline = Instant::now() + STILL_DRAIN_WAIT;
    loop {
        let now = Instant::now();
        let budget = if stopping { worker.stop_deadline() } else { worker.segment_deadline() };
        let expired = now >= budget.map_or(deadline, |global| global.min(deadline));
        let certified = worker.input_prefix_closed(prefix);
        observations.finish_private_prefix(prefix, images, worker.config(), certified || expired);
        if certified && observations.drained() { break; }
        if expired {
            observations.expire_drain(stopping);
            observations.work(device, images, readbacks, png, cutoff.time, now)?;
            break;
        }
        readbacks.poll(images, now)?;
        observations.work(device, images, readbacks, png, cutoff.time, now)?;
        thread::park_timeout(BUSY_POLL);
    }
    if !stopping && worker.pause_pending(cutoff) {
        observations.advance_boundary(cutoff.time);
    }
    Ok(())
}

fn permit_m1_publication(frame: &FrameMetadata, selected: &SelectedMonitor, admission: AdmissionBoundary) -> bool {
    frame.id.monitor == selected.id && frame.geometry.id.monitor == selected.id && frame.admission == admission.epoch
        && frame.source_t_us >= admission.time
}


struct Resources {
    native: native::NativeImages,
    thumbnails: pixels::Thumbnails,
    tone_map: Option<hdr::ToneMap>,
    extent: FrameExtent,
    format: DXGI_FORMAT,
}

impl Resources {
    fn publish(
        &mut self, owned: Texture, metadata: FrameMetadata, painter: &pixels::Painter, video: &mut VideoFeed<'_>,
        timeline: Option<slots::Timeline>, size: SizeInt32, now: Instant, counters: &mut CaptureCounters,
    ) -> Result<(), ProbeError> {
        if !metadata.covered() {
            counters.rejected_approval = counters.rejected_approval.saturating_add(1);
            return Ok(());
        }
        counters.captured = counters.captured.saturating_add(1);
        let sampled = self.thumbnails.submit(painter, self.native.context(), &owned, metadata.id)?;
        if !sampled {
            counters.thumbnail_pressure = counters.thumbnail_pressure.saturating_add(1);
        }
        video.submit(&owned, &metadata, timeline, size, now, counters)?;
        let id = metadata.id;
        self.native.remember(owned, metadata);
        if !sampled {
            self.native.record_change(id, None);
        }
        Ok(())
    }

    /// Publish the staged elevated frames whose receipts the approval now
    /// certifies, in order. A frame the approval no longer owns, or one that
    /// waited past the elevated frame wait, is rejected.
    fn release_staged(
        &mut self, approval: Option<&FrameApproval>, now_session: SessionTimeUs, painter: &pixels::Painter,
        video: &mut VideoFeed<'_>, timeline: Option<slots::Timeline>, size: SizeInt32, now: Instant,
        counters: &mut CaptureCounters,
    ) -> Result<(), ProbeError> {
        while let Some(metadata) = self.native.staged() {
            if !metadata.covered()
                || approval.is_none_or(|approval| !approval.owns(&metadata))
                || now_session.0.saturating_sub(metadata.recv_t_us.0) >= ELEVATED_FRAME_WAIT_US
            {
                self.native.take_staged();
                counters.rejected_approval = counters.rejected_approval.saturating_add(1);
            } else if approval.is_some_and(|approval| approval.matches(&metadata)) {
                let Some((owned, metadata)) = self.native.take_staged() else { break };
                self.publish(owned, metadata, painter, video, timeline, size, now, counters)?;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn new(
        device: &ID3D11Device, painter: &pixels::Painter, size: SizeInt32,
        color: CaptureColor, white: Option<std::num::NonZeroU32>,
    ) -> Result<Self, ProbeError> {
        let extent = FrameExtent::new(size.Width as u32, size.Height as u32)
            .ok_or_else(|| unavailable("The WGC source size is invalid"))?;
        let tone_map = if color == CaptureColor::HdrToneMappedToSdr {
            let white = white.ok_or_else(|| unavailable("The HDR monitor SDR white level is unavailable"))?;
            Some(hdr::ToneMap::new(device, painter, extent, white)?)
        } else {
            None
        };
        let format = if tone_map.is_some() { DXGI_FORMAT_R16G16B16A16_FLOAT } else { DXGI_FORMAT_B8G8R8A8_UNORM };
        Ok(Self {
            native: native::NativeImages::new(device, extent)?,
            thumbnails: pixels::Thumbnails::new(device, painter)?,
            tone_map, extent, format,
        })
    }
}

/// The capture owner's side of the video pipeline: the video owner's
/// target, this owner's converter, the frame in conversion and a frame held
/// for the slot timeline.
struct VideoFeed<'a> {
    target: &'a VideoTarget,
    converter: Option<gpu::Converter>,
    pending: Option<PendingConversion>,
    waiting: TimelineWait,
}

impl VideoFeed<'_> {
    /// The video owner's slot timeline of `epoch`, once published.
    fn timeline(&self, epoch: AdmissionEpoch) -> Option<slots::Timeline> {
        self.target.pacing.try_read().ok()
            .and_then(|pacing| *pacing).filter(|(published, _)| *published == epoch).map(|(_, timeline)| timeline)
    }

    /// Submit a published frame for conversion, or hold it until the slot
    /// timeline arrives. Without a converter the video takes no frames.
    fn submit(
        &mut self, owned: &Texture, metadata: &FrameMetadata, timeline: Option<slots::Timeline>, size: SizeInt32,
        now: Instant, counters: &mut CaptureCounters,
    ) -> Result<(), ProbeError> {
        let Some(converter) = &mut self.converter else { return Ok(()) };
        match timeline {
            Some(timeline) => convert(self.target, converter, &mut self.pending, &mut self.waiting.converted_slot,
                owned, metadata, timeline, size, now, counters),
            None => {
                self.waiting.hold(Arc::clone(owned), metadata.clone(), counters);
                Ok(())
            }
        }
    }

    /// The timeline arrived: mark the slots of replaced held frames, and
    /// submit the held frame if it is still eligible.
    fn release_waiting(
        &mut self, timeline: slots::Timeline, epoch: AdmissionEpoch, size: SizeInt32, now: Instant,
        counters: &mut CaptureCounters,
    ) -> Result<(), ProbeError> {
        let evidence = &self.target.evidence;
        if let Some((first, last)) = self.waiting.replaced.take() {
            let first = timeline.containing(first)?;
            let last = timeline.containing(last)?;
            // The evidence ring holds at most its own length of slots.
            for slot in (first..=last).take(usize::try_from(evidence.slots()).unwrap_or(usize::MAX)) {
                evidence.timeline_pending(slot);
            }
        }
        let Some((owned, metadata)) = self.waiting.frame.take() else { return Ok(()) };
        match &mut self.converter {
            Some(converter) if metadata.admission == epoch => convert(self.target, converter, &mut self.pending,
                &mut self.waiting.converted_slot, &owned, &metadata, timeline, size, now, counters),
            _ => {
                counters.timeline_pending = counters.timeline_pending.saturating_add(1);
                if metadata.admission == epoch {
                    evidence.timeline_pending(timeline.containing(metadata.source_t_us)?);
                }
                Ok(())
            }
        }
    }

    /// Hand a finished conversion to the video worker; a frame not delivered
    /// is counted. Returns true while an elevated frame's finished conversion
    /// awaits its approval within the conversion deadline.
    fn deliver(
        &mut self, open: &Receipt, frame_policy: &FramePolicy, native: &native::NativeImages,
        timeline: Option<slots::Timeline>, now: Instant, counters: &mut CaptureCounters,
    ) -> Result<bool, ProbeError> {
        let Some(converter) = &mut self.converter else { return Ok(false) };
        let Some(PendingConversion { sample, metadata, deadline }) = &self.pending else { return Ok(false) };
        if !converter.ready(sample)? {
            if now >= *deadline {
                return Err(unavailable(CONVERSION_TIMED_OUT));
            }
            return Ok(false);
        }
        let available = open.clock.time(clock::qpc()?)?;
        let approval = frame_policy.try_current(open.boundary.epoch, available);
        if metadata.elevated_visit().is_some() && approval.is_err() && now < *deadline {
            return Ok(true);
        }
        let approved = approval.ok().flatten().is_some_and(|approval| approval.matches(metadata));
        let input = if approved { converter.input(sample)? } else { None };
        if let Some(input) = input {
            let frame = video::Observation { input, metadata: metadata.clone(), available, change: native.change(metadata.id) };
            match self.target.send.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    devlog!("monitor", "video pressure: video queue full source_t_us={}", metadata.source_t_us.0);
                    counters.video_pressure = counters.video_pressure.saturating_add(1);
                    if let Some(timeline) = timeline {
                        self.target.evidence.queue_pressure(timeline.containing(metadata.source_t_us)?);
                    }
                }
                Err(TrySendError::Disconnected(_)) => {
                    counters.video_discarded = counters.video_discarded.saturating_add(1);
                }
            }
        } else if approved {
            // The converted picture could not be read back as encoder input.
            devlog!("monitor", "video pressure: no encoder input for converted frame source_t_us={}", metadata.source_t_us.0);
            counters.video_pressure = counters.video_pressure.saturating_add(1);
            if let Some(timeline) = timeline {
                self.target.evidence.capture_pressure(timeline.containing(metadata.source_t_us)?);
            }
        } else {
            counters.video_discarded = counters.video_discarded.saturating_add(1);
        }
        self.pending = None;
        Ok(false)
    }

    /// Drop the conversion in progress, counted.
    fn discard_conversion(&mut self, counters: &mut CaptureCounters) {
        if self.pending.take().is_some() {
            counters.video_discarded = counters.video_discarded.saturating_add(1);
        }
    }

    /// Drop the video work in progress at a pause or a topology change; each
    /// dropped frame is counted.
    fn discard(&mut self, counters: &mut CaptureCounters) {
        self.discard_conversion(counters);
        self.waiting.discard(counters);
    }

    /// Time asleep does not count against the conversion deadline.
    fn restart_deadline(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.deadline = Instant::now() + CONVERSION_DEADLINE;
        }
    }

    /// The capture loop ended: its conversion and held frame are dropped
    /// uncounted, releasing their textures before the boundary's stills drain.
    fn end_loop(&mut self) {
        self.pending = None;
        self.waiting = TimelineWait::default();
    }
}

/// Submit one approved frame for video conversion. A busy or failed
/// conversion is counted as pressure on the frame's slot.
fn convert(
    target: &VideoTarget, converter: &mut gpu::Converter, pending: &mut Option<PendingConversion>,
    converted_slot: &mut Option<u64>, owned: &Texture, metadata: &FrameMetadata,
    timeline: slots::Timeline, size: SizeInt32, now: Instant, counters: &mut CaptureCounters,
) -> Result<(), ProbeError> {
    let slot = timeline.containing(metadata.source_t_us)?;
    // The software path converts and reads back one frame per video slot,
    // at the encoding rate; thumbnails and stills keep the capture pace.
    if converter.software() && *converted_slot == Some(slot) {
        counters.video_software_paced = counters.video_software_paced.saturating_add(1);
        return Ok(());
    }
    if pending.is_none()
        && let Some(sample) = converter.submit(owned, size.Width, size.Height)?
    {
        *pending = Some(PendingConversion { sample, metadata: metadata.clone(), deadline: now + CONVERSION_DEADLINE });
        *converted_slot = Some(slot);
    } else {
        devlog!("monitor", "video pressure: conversion busy source_t_us={} pending_age_ms={:?}", metadata.source_t_us.0,
            pending.as_ref().map(|pending| (now + CONVERSION_DEADLINE).saturating_duration_since(pending.deadline).as_millis()));
        counters.video_pressure = counters.video_pressure.saturating_add(1);
        target.evidence.capture_pressure(slot);
    }
    Ok(())
}

/// A frame published before the video owner published this epoch's slot
/// timeline, as after Resume. One frame is held and submitted when the
/// timeline arrives, if it is still eligible; every other is counted as
/// `timeline_pending`, and its slot carries that reason.
#[derive(Default)]
struct TimelineWait {
    frame: Option<(Texture, FrameMetadata)>,
    /// Source times of held frames replaced before the timeline arrived.
    replaced: Option<(SessionTimeUs, SessionTimeUs)>,
    /// The latest slot with a frame submitted for conversion.
    converted_slot: Option<u64>,
}

impl TimelineWait {
    fn hold(&mut self, owned: Texture, metadata: FrameMetadata, counters: &mut CaptureCounters) {
        if let Some((_, old)) = self.frame.replace((owned, metadata)) {
            counters.timeline_pending = counters.timeline_pending.saturating_add(1);
            let at = old.source_t_us;
            widen(&mut self.replaced, at);
        }
    }

    fn discard(&mut self, counters: &mut CaptureCounters) {
        if self.frame.take().is_some() {
            counters.timeline_pending = counters.timeline_pending.saturating_add(1);
        }
        self.replaced = None;
        self.converted_slot = None;
    }
}

/// An HDR monitor without an SDR white level is captured through the system
/// BGRA8 conversion; the condition is a health row (VID-8).
fn report_conversion(worker: &WorkerSession, monitor: MonitorId, color: CaptureColor, white: Option<std::num::NonZeroU32>) {
    if color == CaptureColor::HdrSystemBgra8 && white.is_none() {
        worker.report_health(HealthKind::HdrWhiteLevelUnavailable,
            Failure::new(FailureComponent::Capture, "Read the HDR display SDR white level", None), Some(monitor), true);
    }
}

fn next_geometry(old: GeometryEpoch) -> Result<GeometryEpoch, ProbeError> {
    Ok(GeometryEpoch(old.0.checked_add(1).ok_or_else(|| unavailable("The monitor geometry epoch overflowed"))?))
}

fn next_capture(old: CaptureEpoch) -> Result<CaptureEpoch, ProbeError> {
    Ok(CaptureEpoch(old.0.checked_add(1).ok_or_else(|| unavailable("The monitor capture epoch overflowed"))?))
}

fn same_format(old: &Output, new: &Output) -> bool {
    old.same_item(new) && old.rotation == new.rotation
        && old.hdr == new.hdr && old.sdr_white == new.sdr_white && old.bounds.size() == new.bounds.size()
}

fn metadata(
    display: &display::Monitor, selected: &SelectedMonitor, geometry_epoch: GeometryEpoch,
    capture_epoch: CaptureEpoch, capture: &wgc::Capture, color: CaptureColor,
    fp16_fallback_hresult: Option<i32>, video_connected: bool,
) -> MonitorCapture {
    MonitorCapture {
        device_path: display.key.as_str().to_owned(),
        geometry: display.geometry(selected.id, geometry_epoch),
        capture_epoch,
        refresh_numerator: display.refresh.map(|refresh| refresh.0),
        refresh_denominator: display.refresh.map(|refresh| refresh.1),
        rotation: display.rotation,
        primary: display.primary,
        hdr: display.hdr,
        sdr_white_level: display.sdr_white,
        conversion: color,
        tone_map: (color == CaptureColor::HdrToneMappedToSdr).then(|| hdr::TONE_MAP.to_owned()),
        fp16_fallback_hresult,
        border_visible: capture.border_visible,
        cursor_in_pixels: false,
        video_connected,
        counters: CaptureCounters::default(),
        unavailable: display.unavailable.clone(),
    }
}
