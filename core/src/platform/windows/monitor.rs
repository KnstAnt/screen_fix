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
            Direct3D11::ID3D11Device,
            Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT},
        },
        System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop,
    },
    core,
};

use crate::{
    capture::frame::{CaptureColor, FrameExtent, FrameMetadata},
    package::metadata::{CaptureCounters, MonitorCapture},
    protocol::{AdmissionEpoch, CaptureEpoch, FrameId, FrameSequence, GeometryEpoch, GeometryId, MonitorId, SessionTimeUs},
    config::SelectedMonitor,
    session::{AdmissionBoundary, windows::{Admission, PauseProducer, WorkerSession}},
};

use super::{context::FramePolicy, Apartment, ProbeError, capture_device, clock, clock_error, display, gpu, hdr, native, pixels, readback, slots, stage, still_png, still_rows, stills, unavailable, video, wgc};

pub(super) struct Ready {
    pub capture: MonitorCapture,
    pub readback: readback::Requests,
    pub stills: stills::Requests,
}

pub(super) struct FormatRequest {
    pub at: SessionTimeUs,
    pub output: Option<display::topology::Output>,
    pub existing_device: Option<ID3D11Device>,
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
    pub pacing: Arc<RwLock<Option<(AdmissionEpoch, slots::Timeline)>>>,
}

pub(super) fn run(
    selected: &SelectedMonitor, worker: &WorkerSession,
    frame_policy: &FramePolicy, topology: &display::topology::Topology, borderless: bool, ready: SyncSender<Ready>, video: Option<VideoTarget>,
    png: still_png::Inbox, rows: still_rows::Rows,
) -> Completion {
    let mut stop = StopOnError(Some(worker));
    let mut counters = CaptureCounters::default();
    let mut epochs = Vec::new();
    let result = capture(selected, worker, frame_policy, topology, borderless, ready, video, png, rows, &mut counters, &mut epochs);
    if matches!(&result, Ok(Exit::Session(_))) {
        stop.0 = None;
    }
    Completion { outcome: result, counters, epochs }
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
    frame_policy: &FramePolicy, topology: &display::topology::Topology, borderless: bool, ready: SyncSender<Ready>, video: Option<VideoTarget>,
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
    if display_size(&display)? != size {
        return Err(unavailable("The monitor physical geometry does not match its capture size"));
    }
    let adapter_luid = stage(unsafe { display.adapter.GetDesc1() }, "Read the capture adapter")?.AdapterLuid;
    let mut device = if let Some(video) = &video {
        if !same_adapter(adapter_luid, video.adapter_luid)
            || display.bounds != video.source_bounds
        {
            return Err(unavailable("The primary display geometry or adapter changed during startup"));
        }
        video.device.clone()
    } else {
        capture_device(&display.adapter)?
    };
    let (capture, color, fallback) = wgc::start_monitor(item, &device, size, display.hdr, borderless)?;
    let mut painter = pixels::Painter::new(&device)?;
    let mut images = Resources::new(&device, &painter, size, color, display.sdr_white)?;
    counters.native_texture_bytes_peak = images.native.allocation_bytes();
    let mut mode = metadata(&display, selected, GeometryEpoch(1), CaptureEpoch(1), &capture, color, fallback, video.is_some());
    let mut output = display::topology::Output::from_native(&display)?;
    epochs.push(mode.clone());
    topology.capture(mode.clone());
    let mut converter = video.as_ref().map(|video| gpu::Converter::new(&device, video.format, video.software)).transpose()?;
    let still_budget = Arc::new(readback::Budget::default());
    let (readback, mut readbacks) = readback::Readbacks::new(Arc::clone(&still_budget));
    let (stills, mut observations) = stills::Stills::new(rows, still_budget);
    if let Some(video) = &video {
        report_video_ready(worker, selected.id, video.format, video.software);
    }
    ready.try_send(Ready { capture: mode.clone(), readback, stills })
        .map_err(|_| unavailable("The capture coordinator is unavailable"))?;
    let mut native_capture = Some(capture);
    let mut pending_format: Option<Receiver<FormatReply>> = None;
    let mut requested_missing = false;
    let mut reopen_after = None;
    let mut reconfiguring = false;
    let mut rejected_snapshot = None;
    let mut frozen_final = None;

    let result = (|| {
        let Some(start) = worker.wait_for_start()? else { return Ok(Exit::Session(None)) };
        observations.arm_start(start.boundary, mode.geometry.id);
        let interval = Duration::from_nanos(1_000_000_000 / u64::from(worker.config().profile().capture_pace().frames_per_second()));
        let mut scheduled = Instant::now();
        let mut sequence = 0_u64;
        let mut pending_video: Option<(gpu::PendingSample, FrameMetadata, Instant)> = None;
        let mut last_source = None;
        let mut geometry_after = None;
        let mut next_device_check = Instant::now();
        loop {
            let open = match worker.admit()? {
                Admission::Closed(Some(cutoff)) if worker.pause_pending(cutoff) => {
                    frozen_final = Some((cutoff, native_capture.as_ref().is_some_and(|capture| !capture.item_closed())
                        && !reconfiguring));
                    pending_video = None;
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
                next_device_check = now + Duration::from_secs(1);
            }
            let now_session = open.clock.time(open.qpc).map_err(clock_error)?;
            let snapshot = topology.snapshot();
            let latest = snapshot.as_ref().and_then(|snapshot| snapshot.outputs.as_ref().map(|outputs| {
                outputs.iter().find(|entry| entry.key == selected.key).cloned()
            }));
            let item_closed = native_capture.as_ref().is_some_and(wgc::Capture::item_closed);
            if item_closed || matches!(latest, Some(None)) && native_capture.is_some() {
                if item_closed { reopen_after = Some(clock::qpc()?); }
                mode.geometry.id.epoch = next_geometry(mode.geometry.id.epoch)?;
                topology.missing(mode.geometry.id);
                if let Some(capture) = native_capture.take() { capture.close()?; }
                observations.geometry_changed(mode.geometry.id);
                pending_video = None;
                converter = None;
                reconfiguring = false;
            }
            let mut active_geometry = None;
            if let Some(receiver) = &pending_format {
                match receiver.try_recv() {
                    Ok(FormatReply::Ready { device: configured, encoding, output: target }) => {
                        pending_format = None;
                        requested_missing = false;
                        if latest.as_ref().is_some_and(|current| current.as_ref() == Some(&target))
                            && !native_capture.as_ref().is_some_and(wgc::Capture::item_closed)
                        {
                            let actual = display::enumerate()?.into_iter().find(|entry| entry.key == selected.key);
                            if let Some(actual) = actual
                                && display::topology::Output::from_native(&actual)? == target
                            {
                                let next = next_geometry(mode.geometry.id.epoch)?;
                                let epoch = next_capture(mode.capture_epoch)?;
                                let size = display_size(&actual)?;
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
                                    let (color, fallback) = capture.reconfigure(size, actual.hdr)?;
                                    (capture, color, fallback)
                                } else {
                                    let item: GraphicsCaptureItem = stage(unsafe { interop.CreateForMonitor(actual.handle) },
                                        "Create the returned monitor capture item")?;
                                    wgc::start_monitor(item, &device, size, actual.hdr, borderless)?
                                };
                                mode = metadata(&actual, selected, next, epoch, &capture, color, fallback, encoding.is_some());
                                if same_item {
                                    observations.expire_native_epoch();
                                    readbacks.cancel_epoch(readback::Rejected::EpochMismatch);
                                }
                                images = Resources::new(&device, &painter, size, color, actual.sdr_white)?;
                                converter = encoding.map(|(format, software)| gpu::Converter::new(&device, format, software)).transpose()?;
                                native_capture = Some(capture);
                                output = target;
                                pending_video = None;
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
                        if video.as_ref().is_none_or(|video| !video.unavailable.load(Ordering::Acquire)) {
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
                && let Some(video) = &video
            {
                let target = if needs_missing { None } else { present.cloned() };
                let existing_device = target.as_ref().filter(|target| {
                    native_capture.is_some() && output.adapter == target.adapter && output.identity == target.identity
                }).map(|_| device.clone());
                let (reply, result) = sync_channel(1);
                let request = FormatRequest { at: now_session, output: target, existing_device, reply };
                match video.reconfigure.try_send(request) {
                    Ok(()) => {
                        pending_format = Some(result);
                        if !needs_missing && !reconfiguring {
                            topology.reconfiguring(mode.geometry.id);
                            reconfiguring = true;
                        }
                        pending_video = None;
                    }
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return Err(unavailable("The video format owner closed unexpectedly")),
                }
            }
            if missing || format_changed || pending_format.is_some() {
                readbacks.step(&device, &images.native, mode.geometry.id, mode.capture_epoch, open.boundary.epoch, now)?;
                observations.work(&device, &images.native, &mut readbacks, &png, now_session, now)?;
                observations.advance_idle(now_session);
                thread::park_timeout(Duration::from_millis(5));
                continue;
            }
            let Some(capture) = &mut native_capture else { continue };
            if active_geometry.is_none() && let Some(current) = present && output != *current {
                let changed = clock::qpc()?;
                mode.geometry = crate::capture::frame::MonitorGeometry {
                    id: GeometryId { monitor: selected.id, epoch: next_geometry(mode.geometry.id.epoch)? },
                    bounds: current.bounds, dpi_x: current.dpi_x, dpi_y: current.dpi_y,
                };
                mode.refresh_numerator = current.refresh.0;
                mode.refresh_denominator = current.refresh.1;
                mode.primary = current.primary;
                epochs.push(mode.clone());
                topology.capture(mode.clone());
                output = current.clone();
                geometry_after = Some(changed);
                pending_video = None;
                observations.geometry_changed(mode.geometry.id);
            }
            let timeline = video.as_ref().and_then(|video| video.pacing.try_read().ok()
                .and_then(|pacing| *pacing).filter(|(epoch, _)| *epoch == open.boundary.epoch).map(|(_, timeline)| timeline));
            observations.step(&images.native, &mode, frame_policy, worker.config(), open.boundary.epoch, now_session);
            readbacks.step(&device, &images.native, mode.geometry.id, mode.capture_epoch, open.boundary.epoch, now)?;
            observations.work(&device, &images.native, &mut readbacks, &png, now_session, now)?;
            while let Some((frame, change)) = images.thumbnails.poll(images.native.context())? {
                images.native.record_change(frame, change);
                if change.is_some_and(|score| score > 0.0) && let Some(lease) = images.native.lease(frame)
                    && lease.metadata.geometry.id == mode.geometry.id && lease.metadata.admission == open.boundary.epoch
                {
                    observations.changed(lease.metadata);
                }
                if let (Some(video), Some(timeline), Some(frame)) = (&video, timeline, images.native.lease(frame))
                    && change.is_some_and(|change| change > 0.002) && frame.metadata.geometry.id == mode.geometry.id
                    && frame.metadata.admission == open.boundary.epoch
                {
                    video.evidence.changed_late(timeline.containing(frame.metadata.source_t_us)?);
                }
            }
            if video.as_ref().is_some_and(|target| target.unavailable.load(Ordering::Acquire)) {
                pending_video = None;
                converter = None;
            }
            if let (Some(video), Some(converter), Some((sample, metadata, deadline))) = (&video, &mut converter, &pending_video) {
                if converter.ready(sample)? {
                    let available = open.clock.time(clock::qpc()?).map_err(clock_error)?;
                    let approved = frame_policy.current(open.boundary.epoch, available)
                        .is_some_and(|approval| approval.generation == metadata.approval_generation);
                    if approved && let Some(input) = converter.input(sample)? {
                        let frame = video::Observation { input, metadata: *metadata, available,
                            change: images.native.change(metadata.id) };
                        match video.send.try_send(frame) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                counters.video_pressure = counters.video_pressure.saturating_add(1);
                                if let Some(timeline) = timeline {
                                    video.evidence.queue_pressure(timeline.containing(metadata.source_t_us)?);
                                }
                            }
                            Err(TrySendError::Disconnected(_)) => {}
                        }
                    }
                    pending_video = None;
                } else if now >= *deadline {
                    return Err(unavailable("GPU video conversion did not complete within 3 seconds"));
                }
            }
            let sleep = if pending_video.is_some() || images.thumbnails.has_pending() || readbacks.has_pending() {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(10)
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
            if received < receipt.boundary.qpc
                || source_offset < 0 || geometry_offset.is_some_and(|offset| source_offset < offset)
                || last_source.is_some_and(|previous| wgc_time_100ns <= previous)
            {
                counters.rejected_time_or_epoch = counters.rejected_time_or_epoch.saturating_add(1);
                borrowed.close()?;
                continue;
            }
            let source_time = clock.wgc_time(wgc_time_100ns).map_err(clock_error)?;
            let approval_at = clock.time(receipt.qpc).map_err(clock_error)?;
            let approval = frame_policy.current(receipt.boundary.epoch, approval_at)
                .filter(|approval| approval.contains_source(source_time));
            let Some(approval) = approval else {
                counters.rejected_approval = counters.rejected_approval.saturating_add(1);
                borrowed.close()?;
                scheduled = now + interval;
                continue;
            };
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
                recv_t_us: clock.time(received).map_err(clock_error)?,
                source_uncertainty_us: None, color: mode.conversion, extent: images.extent,
            };
            // The same foreground interval must still own the frame when
            // it reaches either the encoder conversion or native history.
            if !permit_m1_publication(&metadata, selected, receipt.boundary)
                || !frame_policy.current(receipt.boundary.epoch, clock.time(super::clock::qpc()?).map_err(clock_error)?)
                    .is_some_and(|current| current.generation == approval.generation) {
                counters.rejected_approval = counters.rejected_approval.saturating_add(1);
                continue;
            }
            counters.captured = counters.captured.saturating_add(1);
            let sampled = images.thumbnails.submit(&painter, images.native.context(), &owned, metadata.id)?;
            if !sampled {
                counters.thumbnail_pressure = counters.thumbnail_pressure.saturating_add(1);
            }
            if let (Some(video), Some(timeline), Some(converter)) = (&video, timeline, &mut converter) {
                let slot = timeline.containing(metadata.source_t_us)?;
                if pending_video.is_none()
                    && let Some(sample) = converter.submit(&owned, capture.size.Width, capture.size.Height)?
                {
                    pending_video = Some((sample, metadata, now + Duration::from_secs(3)));
                } else {
                    counters.video_pressure = counters.video_pressure.saturating_add(1);
                    video.evidence.capture_pressure(slot);
                }
            }
            images.native.remember(owned, metadata);
            if !sampled {
                images.native.record_change(metadata.id, None);
            }
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
    let deadline = Instant::now() + Duration::from_secs(3);
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
        readbacks.step(device, images, mode.geometry.id, mode.capture_epoch, cutoff.epoch, now)?;
        observations.work(device, images, readbacks, png, cutoff.time, now)?;
        thread::park_timeout(Duration::from_millis(1));
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

fn display_size(display: &display::Monitor) -> Result<SizeInt32, ProbeError> {
    let (left, top, right, bottom) = display.bounds.edges();
    let width = right.checked_sub(left).ok_or_else(|| unavailable("The physical monitor width overflowed"))?;
    let height = bottom.checked_sub(top).ok_or_else(|| unavailable("The physical monitor height overflowed"))?;
    Ok(SizeInt32 { Width: width, Height: height })
}

fn next_geometry(old: GeometryEpoch) -> Result<GeometryEpoch, ProbeError> {
    Ok(GeometryEpoch(old.0.checked_add(1).ok_or_else(|| unavailable("The monitor geometry epoch overflowed"))?))
}

fn next_capture(old: CaptureEpoch) -> Result<CaptureEpoch, ProbeError> {
    Ok(CaptureEpoch(old.0.checked_add(1).ok_or_else(|| unavailable("The monitor capture epoch overflowed"))?))
}

fn same_format(old: &display::topology::Output, new: &display::topology::Output) -> bool {
    let extent = |bounds: crate::protocol::PhysicalRect| {
        let (l, t, r, b) = bounds.edges();
        (i64::from(r) - i64::from(l), i64::from(b) - i64::from(t))
    };
    old.identity == new.identity && old.adapter == new.adapter && old.rotation == new.rotation
        && old.hdr == new.hdr && old.sdr_white == new.sdr_white && extent(old.bounds) == extent(new.bounds)
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
        refresh_numerator: display.refresh.0,
        refresh_denominator: display.refresh.1,
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
    }
}

