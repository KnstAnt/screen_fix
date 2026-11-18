use std::{
    sync::{Arc, RwLock, atomic::{AtomicBool, Ordering}, mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel}},
    thread,
    time::{Duration, Instant},
};

use windows::{
    Graphics::{Capture::{GraphicsCaptureItem, GraphicsCaptureSession}, SizeInt32},
    Win32::{
        Foundation::LUID,
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
    capture::frame::{CaptureColor, FrameExtent, FrameMetadata},
    package::{diagnostics::{Failure, FailureComponent}, metadata::{CaptureCounters, MonitorCapture}, outcome::HealthKind},
    protocol::{AdmissionEpoch, CaptureEpoch, FrameId, FrameSequence, GeometryEpoch, GeometryId, MonitorId, SessionTimeUs},
    config::SelectedMonitor,
    session::{AdmissionBoundary, fault::FaultReport, windows::{Admission, PauseProducer, Receipt, WorkerSession}},
};

use super::{context::{ELEVATED_FRAME_WAIT_US, FrameApproval, FramePolicy}, Apartment, ProbeError, capture_device, clock, clock_error, display, encoder, gpu, hdr, native, pixels, readback, slots, stage, still_png, still_rows, stills, unavailable, video, wgc};

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
type PendingVideo = Option<(gpu::PendingSample, FrameMetadata, Instant)>;
type Texture = Arc<ID3D11Texture2D>;

pub(super) struct Ready {
    pub capture: MonitorCapture,
    pub stills: stills::Requests,
}

pub(super) struct FormatRequest {
    pub at: SessionTimeUs,
    pub output: Option<display::topology::Output>,
    /// The capture device for the new output, created by this capture owner;
    /// none when the monitor is missing.
    pub device: Option<ID3D11Device>,
    pub reply: SyncSender<FormatReply>,
}

pub(super) enum FormatReply {
    Missing,
    Ready { device: ID3D11Device, encoding: Option<(gpu::VideoFormat, bool)>, output: display::topology::Output },
    ChangedAgain,
}

pub(super) struct Completion {
    pub outcome: Result<Exit, ProbeError>,
    pub counters: CaptureCounters,
    pub epochs: Vec<MonitorCapture>,
}

pub(super) enum Exit {
    Session(Option<AdmissionBoundary>),
}

pub(super) struct VideoTarget {
    pub send: SyncSender<video::Observation>,
    pub device: ID3D11Device,
    pub adapter_luid: LUID,
    pub source_bounds: crate::protocol::PhysicalRect,
    pub format: gpu::VideoFormat,
    pub software: bool,
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
    if matches!(&result, Ok(Exit::Session(_))) {
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

pub(super) fn report_video_ready(worker: &WorkerSession, monitor: MonitorId, format: gpu::VideoFormat, software: bool) {
    worker.video_ready(crate::session::ReadyVideo {
        monitor, physical_size: format.source, encoded_size: format.encoded, fps: format.fps, software,
    });
}

fn capture(
    selected: &SelectedMonitor, worker: &WorkerSession,
    frame_policy: &FramePolicy, topology: &display::topology::Topology, borderless: bool, ready: SyncSender<Ready>, video: VideoTarget,
    png: still_png::Inbox, rows: still_rows::Rows, counters: &mut CaptureCounters, epochs: &mut Vec<MonitorCapture>,
) -> Result<Exit, ProbeError> {
    let _apartment = Apartment::initialize()?;
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
    let adapter_luid = stage(unsafe { display.adapter.GetDesc1() }, "Read the capture adapter")?.AdapterLuid;
    if !same_adapter(adapter_luid, video.adapter_luid) || display.bounds != video.source_bounds {
        return Err(unavailable("The primary display geometry or adapter changed during startup"));
    }
    let mut device = video.device.clone();
    let (capture, color, fallback) = wgc::start_monitor(item, &device, size, display.hdr, display.sdr_white.is_some(), borderless)?;
    report_conversion(worker, selected.id, color, display.sdr_white);
    let mut painter = pixels::Painter::new(&device)?;
    let mut images = Resources::new(&device, &painter, size, color, display.sdr_white)?;
    counters.native_texture_bytes_peak = images.native.allocation_bytes();
    let mut mode = metadata(&display, selected, GeometryEpoch(1), CaptureEpoch(1), &capture, color, fallback, true);
    let mut output = display::topology::Output::from_native(&display)?;
    epochs.push(mode.clone());
    topology.capture(mode.clone());
    let mut converter = Some(gpu::Converter::new(&device, video.format, video.software)?);
    let still_budget = Arc::new(readback::Budget::default());
    let mut readbacks = readback::Readbacks::new(Arc::clone(&still_budget));
    let (stills, mut observations) = stills::Stills::new(rows, still_budget);
    report_video_ready(worker, selected.id, video.format, video.software);
    ready.try_send(Ready { capture: mode.clone(), stills })
        .map_err(|_| unavailable("The capture coordinator is unavailable"))?;
    let mut native_capture = Some(capture);
    // The pending request's reply and the device its self-check pictures use.
    let mut pending_format: Option<(Receiver<FormatReply>, Option<ID3D11Device>)> = None;
    let mut requested_missing = false;
    let mut reopen_after = None;
    let mut reconfiguring = false;
    let mut rejected_snapshot = None;
    let mut frozen_final = None;

    let result = (|| {
        let Some(start) = worker.wait_for_start()? else { return Ok(Exit::Session(None)) };
        observations.arm_start(start.boundary, mode.geometry.id);
        let interval = capture_interval(worker);
        let mut scheduled = Instant::now();
        let mut sequence = 0_u64;
        let mut pending_video: PendingVideo = None;
        let mut suspends = worker.suspend_marks().sequence;
        let mut waiting = TimelineWait::default();
        let mut last_source = None;
        let mut geometry_after = None;
        let mut next_device_check = Instant::now();
        let mut admission_log = AdmissionLog::default();
        loop {
            let open = match worker.admit()? {
                Admission::Closed(Some(cutoff)) if worker.pause_pending(cutoff) => {
                    frozen_final = Some((cutoff, native_capture.as_ref().is_some_and(|capture| !capture.item_closed())
                        && !reconfiguring));
                    discard_video(&mut pending_video, &mut waiting, counters);
                    images.native.clear_staged();
                    finish_stills(worker, cutoff, cutoff, false, false, &mode, &device, &images.native,
                        &mut observations, &mut readbacks, &png)?;
                    worker.finish_pause(cutoff, PauseProducer::Capture(selected.id));
                    let Some(opened) = worker.wait_for_reopen(cutoff)? else {
                        return Ok(Exit::Session(Some(cutoff)));
                    };
                    frozen_final = None;
                    images.native.clear_history();
                    images.thumbnails = pixels::Thumbnails::new(&device, &painter)?;
                    observations.arm_start(opened.boundary, mode.geometry.id);
                    scheduled = Instant::now();
                    continue;
                }
                Admission::Closed(cutoff) => return Ok(Exit::Session(cutoff)),
                Admission::Open(receipt) => receipt,
                Admission::Starting => return Err(unavailable("Capture admission reopened without a boundary")),
            };
            let now = Instant::now();
            if native_capture.is_some() && (now >= next_device_check
                || native_capture.as_ref().is_some_and(wgc::Capture::item_closed))
            {
                stage(unsafe { device.GetDeviceRemovedReason() }, "Check the monitor GPU device")?;
                next_device_check = now + DEVICE_CHECK_INTERVAL;
            }
            let now_session = open.clock.time(open.qpc).map_err(clock_error)?;
            let snapshot = topology.snapshot();
            let latest = snapshot.as_ref().and_then(|snapshot| snapshot.output(&selected.key).map(|output| output.cloned()));
            let item_closed = native_capture.as_ref().is_some_and(wgc::Capture::item_closed);
            if item_closed || matches!(latest, Some(None)) && native_capture.is_some() {
                if item_closed { reopen_after = Some(clock::qpc()?); }
                mode.geometry.id.epoch = next_geometry(mode.geometry.id.epoch)?;
                topology.missing(mode.geometry.id);
                if let Some(capture) = native_capture.take() { capture.close()?; }
                observations.geometry_changed(mode.geometry.id);
                discard_video(&mut pending_video, &mut waiting, counters);
                images.native.clear_staged();
                converter = None;
                reconfiguring = false;
            }
            let mut active_geometry = None;
            if let Some((receiver, _)) = &pending_format {
                match receiver.try_recv() {
                    Ok(FormatReply::Ready { device: configured, encoding, output: target }) => {
                        pending_format = None;
                        requested_missing = false;
                        if latest.as_ref().is_some_and(|current| current.as_ref() == Some(&target))
                            && !native_capture.as_ref().is_some_and(wgc::Capture::item_closed)
                        {
                            // A failed read proves nothing about the monitor: the
                            // request is retried on the next topology tick (VID-9).
                            if let Ok(actual) = display::resolve(&selected.key)
                                && display::topology::Output::from_native(&actual).is_ok_and(|actual| actual == target)
                            {
                                let next = next_geometry(mode.geometry.id.epoch)?;
                                let epoch = next_capture(mode.capture_epoch)?;
                                let size = actual.size()?;
                                let same_item = native_capture.is_some() && output.identity == target.identity
                                    && output.adapter == target.adapter;
                                if !same_item {
                                    if let Some(old) = native_capture.take() { old.close()?; }
                                    observations.expire_native_epoch();
                                    readbacks.cancel_epoch(readback::Rejected::EpochMismatch);
                                    device = configured;
                                    painter = pixels::Painter::new(&device)?;
                                }
                                let (capture, color, fallback) = if same_item {
                                    let mut capture = native_capture.take()
                                        .ok_or_else(|| unavailable("The existing monitor capture item is unavailable"))?;
                                    let (color, fallback) = capture.reconfigure(size, actual.hdr, actual.sdr_white.is_some())?;
                                    (capture, color, fallback)
                                } else {
                                    let item: GraphicsCaptureItem = stage(unsafe { interop.CreateForMonitor(actual.handle) },
                                        "Create the returned monitor capture item")?;
                                    wgc::start_monitor(item, &device, size, actual.hdr, actual.sdr_white.is_some(), borderless)?
                                };
                                mode = metadata(&actual, selected, next, epoch, &capture, color, fallback, encoding.is_some());
                                report_conversion(worker, selected.id, color, actual.sdr_white);
                                if same_item {
                                    observations.expire_native_epoch();
                                    readbacks.cancel_epoch(readback::Rejected::EpochMismatch);
                                }
                                images = Resources::new(&device, &painter, size, color, actual.sdr_white)?;
                                converter = encoding.map(|(format, software)| gpu::Converter::new(&device, format, software)).transpose()?;
                                native_capture = Some(capture);
                                output = target;
                                discard_video(&mut pending_video, &mut waiting, counters);
                                last_source = None;
                                let changed = clock::qpc()?;
                                geometry_after = Some(changed);
                                observations.geometry_changed(mode.geometry.id);
                                epochs.push(mode.clone());
                                topology.capture(mode.clone());
                                counters.native_texture_bytes_peak = counters.native_texture_bytes_peak.max(images.native.allocation_bytes());
                                reconfiguring = false;
                                reopen_after = None;
                                active_geometry = Some(changed);
                            }
                        }
                        if active_geometry.is_none() {
                            rejected_snapshot = snapshot.as_ref().map(|snapshot| snapshot.received);
                        }
                    }
                    Ok(FormatReply::Missing) => {
                        pending_format = None;
                        requested_missing = true;
                    }
                    Ok(FormatReply::ChangedAgain) => {
                        pending_format = None;
                        rejected_snapshot = snapshot.as_ref().map(|snapshot| snapshot.received);
                    }
                    Err(TryRecvError::Disconnected) => {
                        if !video.unavailable.load(Ordering::Acquire) {
                            return Err(unavailable("The monitor's video format owner is unavailable"));
                        }
                        pending_format = None;
                    }
                    Err(TryRecvError::Empty) => {}
                }
            }
            let latest = if requested_missing && reopen_after.zip(snapshot.as_ref())
                .is_some_and(|(closed, snapshot)| snapshot.received <= closed)
            {
                Some(None)
            } else { latest };
            let present = latest.as_ref().and_then(Option::as_ref);
            let format_changed = present.is_some_and(|target| {
                native_capture.is_none() || reconfiguring || !same_format(&output, target)
            });
            let missing = native_capture.is_none();
            let needs_missing = missing && !requested_missing;
            if pending_format.is_none() && (needs_missing || format_changed)
                && snapshot.as_ref().is_none_or(|snapshot| rejected_snapshot != Some(snapshot.received))
            {
                let target = if needs_missing { None } else { present.cloned() };
                // A new adapter's device is created here, by the owner of its
                // immediate context. A failed read is retried on the next topology snapshot.
                let request_device = match &target {
                    None => Some(None),
                    Some(target) if native_capture.is_some() && output.adapter == target.adapter && output.identity == target.identity => {
                        Some(Some(device.clone()))
                    }
                    Some(target) => display::resolve(&target.key).ok()
                        .and_then(|actual| capture_device(&actual.adapter).ok()).map(Some),
                };
                let (reply, result) = sync_channel(1);
                let sent = request_device.clone()
                    .map(|device| video.reconfigure.try_send(FormatRequest { at: now_session, output: target, device, reply }));
                match sent {
                    None => rejected_snapshot = snapshot.as_ref().map(|snapshot| snapshot.received),
                    Some(Ok(())) => {
                        pending_format = Some((result, request_device.flatten()));
                        if !needs_missing && !reconfiguring {
                            topology.reconfiguring(mode.geometry.id);
                            reconfiguring = true;
                        }
                        discard_video(&mut pending_video, &mut waiting, counters);
                        images.native.clear_staged();
                    }
                    Some(Err(TrySendError::Full(_))) => {}
                    Some(Err(TrySendError::Disconnected(_))) => return Err(unavailable("The video format owner closed unexpectedly")),
                }
            }
            if missing || format_changed || pending_format.is_some() {
                if let Some((_, Some(probe_device))) = &pending_format {
                    // The video owner qualifies the new format's encoder with
                    // pictures converted on this thread (VID-12).
                    while let Ok(probe) = video.probes.try_recv() {
                        let _ = probe.reply.try_send(encoder::probe_input(probe_device, probe.format, probe.software));
                    }
                }
                readbacks.poll(&images.native, now)?;
                observations.work(&device, &images.native, &mut readbacks, &png, now_session, now)?;
                observations.advance_idle(now_session);
                thread::park_timeout(RECONFIGURE_POLL);
                continue;
            }
            let Some(capture) = &mut native_capture else { continue };
            if active_geometry.is_none() && let Some(current) = present && output != *current {
                let changed = clock::qpc()?;
                mode.geometry = crate::capture::frame::MonitorGeometry {
                    id: GeometryId { monitor: selected.id, epoch: next_geometry(mode.geometry.id.epoch)? },
                    bounds: current.bounds, dpi_x: current.dpi_x, dpi_y: current.dpi_y,
                };
                mode.refresh_numerator = current.refresh.map(|refresh| refresh.0);
                mode.refresh_denominator = current.refresh.map(|refresh| refresh.1);
                mode.primary = current.primary;
                mode.unavailable = current.unavailable.clone();
                epochs.push(mode.clone());
                topology.capture(mode.clone());
                output = current.clone();
                geometry_after = Some(changed);
                // The pixel size and format are unchanged. A conversion of the
                // old geometry is dropped and counted; a frame held for the
                // timeline stays and is judged when the timeline arrives.
                if pending_video.take().is_some() {
                    counters.video_discarded = counters.video_discarded.saturating_add(1);
                }
                images.native.clear_staged();
                observations.geometry_changed(mode.geometry.id);
            }
            let timeline = video.pacing.try_read().ok()
                .and_then(|pacing| *pacing).filter(|(epoch, _)| *epoch == open.boundary.epoch).map(|(_, timeline)| timeline);
            if let Some(timeline) = timeline {
                waiting.release(&video, timeline, open.boundary.epoch, &mut converter, &mut pending_video,
                    capture.size, now, counters)?;
            }
            let elevated = worker.elevated_epoch(open.boundary.epoch);
            if !elevated {
                observations.step(&images.native, &mode, frame_policy, worker.config(), open.boundary.epoch, now_session);
            }
            readbacks.poll(&images.native, now)?;
            observations.work(&device, &images.native, &mut readbacks, &png, now_session, now)?;
            while let Some((frame, change)) = images.thumbnails.poll(images.native.context())? {
                images.native.record_change(frame, change);
                if change.is_some_and(|score| score > 0.0) && let Some(lease) = images.native.lease(frame)
                    && lease.metadata.covered() && lease.metadata.geometry.id == mode.geometry.id && lease.metadata.admission == open.boundary.epoch
                {
                    observations.changed(lease.metadata);
                }
                if let (Some(timeline), Some(frame)) = (timeline, images.native.lease(frame))
                    && change.is_some_and(|change| change > 0.002) && frame.metadata.geometry.id == mode.geometry.id
                    && frame.metadata.covered() && frame.metadata.admission == open.boundary.epoch
                {
                    video.evidence.changed_late(timeline.containing(frame.metadata.source_t_us)?);
                }
            }
            if video.unavailable.load(Ordering::Acquire) {
                discard_video(&mut pending_video, &mut waiting, counters);
                converter = None;
            }
            // Time asleep does not count against the conversion bound (D-5 A).
            let suspend_sequence = worker.suspend_marks().sequence;
            if std::mem::replace(&mut suspends, suspend_sequence) != suspend_sequence && let Some(pending) = &mut pending_video {
                pending.2 = Instant::now() + CONVERSION_DEADLINE;
            }
            if let Some(converter) = &mut converter
                && deliver_converted(converter, &mut pending_video, &open, frame_policy, &images.native, &video, timeline,
                    now, counters)?
            {
                thread::park_timeout(APPROVAL_POLL);
                continue;
            }
            if let Ok(approval) = frame_policy.try_current(open.boundary.epoch, now_session) {
                images.release_staged(approval.as_ref(), now_session, &painter, &video, timeline, &mut converter,
                    &mut pending_video, &mut waiting, capture.size, now, counters)?;
            }
            if elevated {
                observations.step(&images.native, &mode, frame_policy, worker.config(), open.boundary.epoch, now_session);
            }
            let sleep = if pending_video.is_some() || images.thumbnails.has_pending() || readbacks.has_pending() {
                BUSY_POLL
            } else {
                IDLE_POLL
            };
            if now < scheduled {
                thread::park_timeout(scheduled.duration_since(now).min(sleep));
                continue;
            }
            let Some(borrowed) = capture.newest()? else {
                thread::park_timeout(sleep);
                continue;
            };
            // Receipt is sampled by WGC at dequeue, before this admission check.
            counters.frames_dequeued = counters.frames_dequeued.saturating_add(1);
            let received = borrowed.received;
            let receipt = match worker.admit()? {
                Admission::Open(receipt) if receipt.boundary == open.boundary => receipt,
                Admission::Closed(_) => {
                    admission_log.reject(Reject::AdmissionClosed, || format!("received_qpc={}", received.0));
                    counters.rejected_time_or_epoch = counters.rejected_time_or_epoch.saturating_add(1);
                    borrowed.close()?;
                    continue;
                }
                _ => return Err(unavailable("Monitor capture changed admission during dequeue")),
            };
            let wgc_time_100ns = borrowed.source_100ns()?;
            let clock = receipt.clock;
            let source_offset = clock.wgc_offset_100ns(wgc_time_100ns).map_err(clock_error)?;
            let geometry_offset = geometry_after.map(|at| clock.offset_100ns(at)).transpose().map_err(clock_error)?;
            let before_boundary = received < receipt.boundary.qpc;
            let negative_source = source_offset < 0;
            let before_geometry = geometry_offset.is_some_and(|offset| source_offset < offset);
            let non_monotonic = last_source.is_some_and(|previous| wgc_time_100ns <= previous);
            let detail = |source_time: Option<SessionTimeUs>, recv_time: Option<SessionTimeUs>| format!(
                "wgc_time_100ns={wgc_time_100ns} received_qpc={} boundary_qpc={} qpc_frequency={} source_offset_100ns={source_offset} \
                 geometry_offset_100ns={geometry_offset:?} source_time_us={:?} recv_time_us={:?} source_minus_recv_us={:?} \
                 last_source_100ns={last_source:?}",
                received.0, receipt.boundary.qpc.0, clock.qpc_frequency.0, source_time.map(|time| time.0),
                recv_time.map(|time| time.0), source_time.zip(recv_time).map(|(source, recv)| source.0 - recv.0));
            if before_boundary || negative_source || before_geometry || non_monotonic {
                let reason = if before_boundary {
                    Reject::BeforeBoundary
                } else if negative_source {
                    Reject::NegativeSource
                } else if before_geometry {
                    Reject::BeforeGeometry
                } else {
                    Reject::NonMonotonic
                };
                admission_log.reject(reason, || detail(clock.wgc_time(wgc_time_100ns).ok(), clock.time(received).ok()));
                counters.rejected_time_or_epoch = counters.rejected_time_or_epoch.saturating_add(1);
                borrowed.close()?;
                continue;
            }
            let source_time = clock.wgc_time(wgc_time_100ns).map_err(clock_error)?;
            let dequeued_time = clock.time(received).map_err(clock_error)?;
            // WGC stamps a frame with its DWM present (vblank) time, which can
            // lead the dequeue by up to a refresh interval. The frame's receipt is
            // therefore max(dequeue, source), so source <= receipt holds (§6.5).
            // A lead beyond `MAX_PRESENT_LEAD_US` is not a present time and is rejected.
            let recv_time = dequeued_time.max(source_time);
            if source_time.0 - dequeued_time.0 > MAX_PRESENT_LEAD_US {
                admission_log.reject(Reject::SourceAfterReceipt, || detail(Some(source_time), Some(dequeued_time)));
                counters.rejected_time_or_epoch = counters.rejected_time_or_epoch.saturating_add(1);
                borrowed.close()?;
                continue;
            }
            admission_log.accept(|| detail(Some(source_time), Some(dequeued_time)));
            // The content was composed no later than the dequeue or the present
            // time, so the approval must cover that point. It usually leads the
            // latest foreground observation by less than one context poll; an
            // ordinary approval not yet observed through it is awaited briefly.
            let content_time = source_time.min(dequeued_time);
            let mut approval_at = clock.time(receipt.qpc).map_err(clock_error)?;
            let approval = loop {
                let waiting = approval_at.0 - content_time.0 < MAX_APPROVAL_CATCH_UP_US;
                match frame_policy.try_current(receipt.boundary.epoch, approval_at) {
                    Ok(Some(approval)) if approval.contains_source(content_time) || approval.can_stage(content_time, approval_at) =>
                        break Some(approval),
                    // A contended or newer-than-`approval_at` read is transient.
                    Ok(Some(approval)) if approval.awaits_source(content_time) && waiting => {}
                    Err(_) if waiting => {}
                    _ => break None,
                }
                thread::park_timeout(APPROVAL_POLL);
                approval_at = clock.time(super::clock::qpc()?).map_err(clock_error)?;
            };
            let Some(approval) = approval else {
                devlog!("monitor", "frame rejected by approval: content_time_us={} approval_at_us={}",
                    content_time.0, approval_at.0);
                counters.rejected_approval = counters.rejected_approval.saturating_add(1);
                borrowed.close()?;
                scheduled = now + interval;
                continue;
            };
            if approval.elevated_visit().is_some() && images.native.staging_full() {
                borrowed.close()?;
                scheduled = now + interval;
                counters.native_pressure = counters.native_pressure.saturating_add(1);
                if let Some(timeline) = timeline {
                    video.evidence.capture_pressure(timeline.containing(source_time)?);
                }
                continue;
            }
            let copied = borrowed.with_texture(capture.size, images.format,
                |source| images.native.copy(source, images.tone_map.as_ref()));
            let closed = borrowed.close();
            scheduled = now + interval;
            let owned = match copied.and_then(|copied| closed.map(|()| copied))? {
                Some(Some(owned)) => owned,
                Some(None) => {
                    counters.native_pressure = counters.native_pressure.saturating_add(1);
                    continue;
                }
                None => {
                    counters.mismatched_size = counters.mismatched_size.saturating_add(1);
                    continue;
                }
            };
            images.native.finish_copy(&owned, &painter, images.tone_map.as_ref())?;
            last_source = Some(wgc_time_100ns);
            sequence = sequence.checked_add(1).ok_or_else(|| unavailable("The monitor frame sequence overflowed"))?;
            let metadata = FrameMetadata {
                id: FrameId { monitor: selected.id, epoch: mode.capture_epoch, sequence: FrameSequence(sequence) },
                geometry: mode.geometry, admission: receipt.boundary.epoch,
                approval_generation: approval.generation, source_t_us: source_time,
                coverage: approval.coverage.clone(),
                recv_t_us: recv_time,
                source_uncertainty_us: None, color: mode.conversion, extent: images.extent,
            };
            let publish_at = clock.time(super::clock::qpc()?).map_err(clock_error)?;
            let current = frame_policy.current(receipt.boundary.epoch, publish_at);
            if !permit_m1_publication(&metadata, selected, receipt.boundary)
                || current.is_none_or(|approval| !approval.owns(&metadata)) || !metadata.covered()
            {
                counters.rejected_approval = counters.rejected_approval.saturating_add(1);
                continue;
            }
            if metadata.elevated_visit().is_some() {
                // The existing GPU copy enters private staging. History, thumbnails
                // and conversion see the texture only after its receipt is certified.
                images.native.stage(owned, metadata);
                continue;
            }
            images.publish(owned, metadata, &painter, &video, timeline, &mut converter, &mut pending_video,
                &mut waiting, capture.size, now, counters)?;
            let now_session = receipt.clock.time(receipt.qpc).map_err(clock_error)?;
            observations.step(&images.native, &mode, frame_policy, worker.config(), open.boundary.epoch, now_session);
        }
    })();
    let result = result.and_then(|exit| {
        if let Exit::Session(Some(cutoff)) = &exit {
            let capture_available = frozen_final.filter(|(boundary, _)| boundary == cutoff).map_or_else(
                || native_capture.as_ref().is_some_and(|capture| !capture.item_closed()) && !reconfiguring,
                |(_, available)| available,
            );
            finish_stills(worker, *cutoff, worker.stop_boundary().unwrap_or(*cutoff), true, capture_available, &mode, &device, &images.native,
                &mut observations, &mut readbacks, &png)?;
        }
        Ok(exit)
    });
    readbacks.close(if result.is_ok() { readback::Rejected::Closed } else { readback::Rejected::CaptureFailed });
    let close = native_capture.map_or(Ok(()), wgc::Capture::close);
    result.and_then(|result| close.map(|()| result))
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
            observations.expire_drain(if stopping { "stop_drain_deadline" } else { "pause_drain_deadline" });
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
        &mut self, owned: Texture,
        metadata: FrameMetadata, painter: &pixels::Painter, video: &VideoTarget, timeline: Option<slots::Timeline>,
        converter: &mut Option<gpu::Converter>, pending_video: &mut PendingVideo,
        waiting: &mut TimelineWait, size: SizeInt32, now: Instant, counters: &mut CaptureCounters,
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
        if let Some(converter) = converter {
            match timeline {
                Some(timeline) => submit_video(&owned, &metadata, video, timeline, converter, pending_video, &mut waiting.converted_slot, size, now, counters)?,
                None => waiting.hold(Arc::clone(&owned), metadata.clone(), counters),
            }
        }
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
        video: &VideoTarget, timeline: Option<slots::Timeline>, converter: &mut Option<gpu::Converter>,
        pending_video: &mut PendingVideo, waiting: &mut TimelineWait, size: SizeInt32, now: Instant,
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
                self.publish(owned, metadata, painter, video, timeline, converter, pending_video, waiting, size, now, counters)?;
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

/// Hand a finished video conversion to the video worker; a frame not
/// delivered is counted. Returns true while an elevated frame's finished
/// conversion awaits its approval within the conversion deadline.
fn deliver_converted(
    converter: &mut gpu::Converter, pending_video: &mut PendingVideo, open: &Receipt, frame_policy: &FramePolicy,
    native: &native::NativeImages, video: &VideoTarget, timeline: Option<slots::Timeline>, now: Instant,
    counters: &mut CaptureCounters,
) -> Result<bool, ProbeError> {
    let Some((sample, metadata, deadline)) = pending_video.as_ref() else { return Ok(false) };
    if !converter.ready(sample)? {
        if now >= *deadline {
            return Err(unavailable(CONVERSION_TIMED_OUT));
        }
        return Ok(false);
    }
    let available = open.clock.time(clock::qpc()?).map_err(clock_error)?;
    let approval = frame_policy.try_current(open.boundary.epoch, available);
    if metadata.elevated_visit().is_some() && approval.is_err() && now < *deadline {
        return Ok(true);
    }
    let approved = approval.ok().flatten().is_some_and(|approval| approval.matches(metadata));
    let input = if approved { converter.input(sample)? } else { None };
    if let Some(input) = input {
        let frame = video::Observation { input, metadata: metadata.clone(), available, change: native.change(metadata.id) };
        match video.send.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                devlog!("monitor", "video pressure: video queue full source_t_us={}", metadata.source_t_us.0);
                counters.video_pressure = counters.video_pressure.saturating_add(1);
                if let Some(timeline) = timeline {
                    video.evidence.queue_pressure(timeline.containing(metadata.source_t_us)?);
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
            video.evidence.capture_pressure(timeline.containing(metadata.source_t_us)?);
        }
    } else {
        counters.video_discarded = counters.video_discarded.saturating_add(1);
    }
    *pending_video = None;
    Ok(false)
}

/// Submit one approved frame for video conversion. A busy or failed
/// conversion is counted as pressure on the frame's slot.
fn submit_video(
    owned: &Texture, metadata: &FrameMetadata, video: &VideoTarget,
    timeline: slots::Timeline, converter: &mut gpu::Converter,
    pending_video: &mut PendingVideo, converted_slot: &mut Option<u64>,
    size: SizeInt32, now: Instant, counters: &mut CaptureCounters,
) -> Result<(), ProbeError> {
    let slot = timeline.containing(metadata.source_t_us)?;
    // The software path converts and reads back one frame per video slot,
    // at the encoding rate; thumbnails and stills keep the capture pace.
    if converter.software() && *converted_slot == Some(slot) {
        counters.video_software_paced = counters.video_software_paced.saturating_add(1);
        return Ok(());
    }
    if pending_video.is_none()
        && let Some(sample) = converter.submit(owned, size.Width, size.Height)?
    {
        *pending_video = Some((sample, metadata.clone(), now + CONVERSION_DEADLINE));
        *converted_slot = Some(slot);
    } else {
        devlog!("monitor", "video pressure: conversion busy source_t_us={} pending_age_ms={:?}", metadata.source_t_us.0,
            pending_video.as_ref().map(|pending| (now + CONVERSION_DEADLINE).saturating_duration_since(pending.2).as_millis()));
        counters.video_pressure = counters.video_pressure.saturating_add(1);
        video.evidence.capture_pressure(slot);
    }
    Ok(())
}

/// Drop the video work in progress at a pause or a topology change; each
/// dropped frame is counted.
fn discard_video(
    pending_video: &mut PendingVideo, waiting: &mut TimelineWait,
    counters: &mut CaptureCounters,
) {
    if pending_video.take().is_some() {
        counters.video_discarded = counters.video_discarded.saturating_add(1);
    }
    waiting.discard(counters);
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
    fn hold(
        &mut self, owned: Texture, metadata: FrameMetadata,
        counters: &mut CaptureCounters,
    ) {
        if let Some((_, old)) = self.frame.replace((owned, metadata)) {
            counters.timeline_pending = counters.timeline_pending.saturating_add(1);
            let at = old.source_t_us;
            self.replaced = Some(self.replaced.map_or((at, at), |(first, last)| (first.min(at), last.max(at))));
        }
    }

    fn discard(&mut self, counters: &mut CaptureCounters) {
        if self.frame.take().is_some() {
            counters.timeline_pending = counters.timeline_pending.saturating_add(1);
        }
        self.replaced = None;
        self.converted_slot = None;
    }

    fn release(
        &mut self, video: &VideoTarget, timeline: slots::Timeline, epoch: crate::protocol::AdmissionEpoch,
        converter: &mut Option<gpu::Converter>, pending_video: &mut PendingVideo,
        size: SizeInt32, now: Instant, counters: &mut CaptureCounters,
    ) -> Result<(), ProbeError> {
        if let Some((first, last)) = self.replaced.take() {
            let first = timeline.containing(first)?;
            let last = timeline.containing(last)?;
            // The evidence ring holds at most its own length of slots.
            for slot in (first..=last).take(usize::try_from(video.evidence.slots()).unwrap_or(usize::MAX)) {
                video.evidence.timeline_pending(slot);
            }
        }
        let Some((owned, metadata)) = self.frame.take() else { return Ok(()) };
        match converter {
            Some(converter) if metadata.admission == epoch =>
                submit_video(&owned, &metadata, video, timeline, converter, pending_video, &mut self.converted_slot, size, now, counters),
            _ => {
                counters.timeline_pending = counters.timeline_pending.saturating_add(1);
                if metadata.admission == epoch {
                    video.evidence.timeline_pending(timeline.containing(metadata.source_t_us)?);
                }
                Ok(())
            }
        }
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

fn same_format(old: &display::topology::Output, new: &display::topology::Output) -> bool {
    old.identity == new.identity && old.adapter == new.adapter && old.rotation == new.rotation
        && old.hdr == new.hdr && old.sdr_white == new.sdr_white && old.bounds.size() == new.bounds.size()
}

fn same_adapter(a: LUID, b: LUID) -> bool {
    (a.LowPart, a.HighPart) == (b.LowPart, b.HighPart)
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

