//! Each selected monitor owns its encoder and locally verified MP4 files.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, RwLock, atomic::{AtomicBool, Ordering}, mpsc::{self, Receiver, SyncSender}},
    thread,
    time::Duration,
};

use windows::Win32::Media::MediaFoundation::*;

use crate::{
    capture::frame::FrameMetadata,
    config::SelectedMonitor,
    package::{
        ArtifactKind,
        diagnostics::{Failure, FailureComponent, ReportOutcome},
        journal::{Flush, JournalIngress},
        metadata::{AttemptLayout, CaptureCounters, Geometry, MonitorCapture, MonitorVideo, SegmentEncoding, SegmentSummary},
    },
    protocol::{AdmissionEpoch, MonitorId, SegmentId, SessionTimeUs},
    session::{AdmissionBoundary, windows::{Admission, PausedVideo, WorkerSession}},
};

use super::{context::{ApprovalReadError, FramePolicy}, Apartment, ProbeError, RecordingReport, capture_device, clock_error, display, encode, encoder, h264, monitor, mp4, package_journal_error, slots, stage, unavailable};

const ROTATION_US: i64 = 60_000_000;
const IDR_ALLOWANCE_US: i64 = 1_000_000;
const MAX_SEGMENT_PICTURES: usize = 61 * 30 + 2;
const MAX_STAGED_SLOTS: usize = MAX_SEGMENT_PICTURES + 4;

#[derive(Clone)]
pub(super) struct Observation {
    pub input: encode::Input,
    pub metadata: FrameMetadata,
    pub available: SessionTimeUs,
    pub change: Option<f64>,
}

pub(super) struct Frames {
    state: Mutex<(JournalIngress, Option<SessionTimeUs>)>,
}

impl Frames {
    pub(super) fn new(ingress: JournalIngress) -> Self {
        Self { state: Mutex::new((ingress, None)) }
    }

    fn publish(&self, worker: &WorkerSession, row: &slots::Row) -> Result<(), ProbeError> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // P03 sequences the receipt at this common ingress. The row separately
        // keeps the slot decision time and the native capture receipt.
        let at = worker.session_time().ok_or_else(|| unavailable("The frame index receipt clock is unavailable"))?;
        let row = serde_json::to_value(row).map_err(|error| mp4::io_error("Encode the frame index row", error.into()))?;
        state.0.submit_verified(at, row).map_err(package_journal_error)?;
        state.1 = Some(at);
        Ok(())
    }

    fn flush(&self) -> Result<(), ProbeError> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = state.1 {
            state.0.flush_wait(Flush::Os, at).map_err(package_journal_error)?;
        }
        Ok(())
    }
}

pub(super) struct Finished {
    pub video: MonitorVideo,
    pub capture: CaptureCounters,
    pub epochs: Vec<MonitorCapture>,
    pub outcome: Result<Option<AdmissionBoundary>, ProbeError>,
    pub required_finalization_failure: Option<&'static str>,
}

pub(super) fn run(
    monitor: &SelectedMonitor, worker: &WorkerSession, layout: &AttemptLayout,
    frames: &Frames, frame_policy: &FramePolicy, topology: &display::topology::Topology, borderless: bool, ready: SyncSender<monitor::Ready>,
    reports: Option<SyncSender<RecordingReport>>,
    png: super::still_png::Inbox, still_rows: super::still_rows::Rows,
) -> Result<Finished, ProbeError> {
    let _apartment = Apartment::initialize()?;
    let display = display::resolve(&monitor.key)?;
    let luid = stage(unsafe { display.adapter.GetDesc1() }, "Read the display adapter")?.AdapterLuid;
    let device = capture_device(&display.adapter)?;
    let geometry = geometry_for(&display, monitor)?;
    let source = [geometry.source_size[0] as u32, geometry.source_size[1] as u32];
    let requested_fps = worker.config().profile().capture_pace().frames_per_second();
    let encoder = encoder::select(&device, &display.adapter, luid, source, requested_fps)?;
    let format = encoder.format;
    let geometry = video_geometry(geometry, format);
    let mut summary = MonitorVideo {
        monitor: monitor.id, fps: format.fps, degraded: encoder.software(),
        slots: 0, encoded: 0, approval_suppressed: 0, unavailable: false, counters: Default::default(), segments: Vec::new(),
    };
    let (send, receive) = mpsc::sync_channel(2);
    let (reconfigure, changes) = mpsc::sync_channel(1);
    let pacing = Arc::new(RwLock::new(None));
    let video_unavailable = Arc::new(AtomicBool::new(false));
    let evidence = Arc::new(slots::Evidence::new());
    let video_input = monitor::VideoTarget {
        send, device, adapter_luid: luid, source_bounds: display.bounds, format, software: encoder.software(), unavailable: Arc::clone(&video_unavailable),
        evidence: Arc::clone(&evidence), reconfigure, pacing: Arc::clone(&pacing),
    };
    thread::scope(|scope| {
        let capture = thread::Builder::new().name(format!("capture-monitor-{}", monitor.id.0))
            .spawn_scoped(scope, move || monitor::run(monitor, worker, frame_policy, topology, borderless, ready, Some(video_input), png, still_rows))
            .map_err(|error| mp4::io_error("Start the monitor capture owner", error))?;
        let recording = record(monitor, worker, layout, frames, frame_policy, topology, receive, changes,
            &pacing, encoder, geometry, &mut summary, &video_unavailable, &evidence, reports.as_ref());
        if recording.is_err() { worker.request_stop(); }
        let capture = capture.join().map_err(|_| unavailable("The monitor capture owner terminated unexpectedly"))?;
        let mut required_finalization_failure = None;
        let outcome = recording.and_then(|closing| {
            required_finalization_failure = closing.required_finalization_failure;
            let monitor::Exit::Session(cutoff) = capture.outcome?;
            if closing.cutoff != cutoff {
                return Err(unavailable("Capture and video closed at different boundaries"));
            }
            Ok(cutoff)
        });
        Ok(Finished { video: summary, capture: capture.counters, epochs: capture.epochs, outcome,
            required_finalization_failure })
    })
}

fn geometry_for(display: &display::Monitor, monitor: &SelectedMonitor) -> Result<Geometry, ProbeError> {
    let (left, top, right, bottom) = display.bounds.edges();
    let width = right.checked_sub(left).filter(|&size| size > 0)
        .ok_or_else(|| unavailable("Invalid physical monitor width"))?;
    let height = bottom.checked_sub(top).filter(|&size| size > 0)
        .ok_or_else(|| unavailable("Invalid physical monitor height"))?;
    Ok(Geometry { display_id: monitor.key.as_str().into(), desktop_rect: [left, top, right, bottom],
        source_size: [width, height], video_size: [0, 0], content_rect: None, video_scale: None })
}

fn video_geometry(mut geometry: Geometry, format: super::gpu::VideoFormat) -> Geometry {
    geometry.video_size = format.encoded;
    geometry.content_rect = Some([0, 0, format.content[0], format.content[1]]);
    geometry.video_scale = Some(f64::from(format.content[0].max(format.content[1]))
        / f64::from(format.source[0].max(format.source[1])));
    geometry
}

struct RecordedSlot {
    row: slots::Row,
    pts: i64,
    duration: i64,
    forced: bool,
}

struct Selection<'a> {
    receive: &'a Receiver<Observation>,
    future: Option<Observation>,
    previous: Option<Observation>,
    missing: bool,
}

impl Selection<'_> {
    fn newest(
        &mut self, epoch: AdmissionEpoch, start: SessionTimeUs, end: SessionTimeUs,
        accepts: impl Fn(&Observation) -> bool,
    ) -> Option<Observation> {
        let mut newest = None;
        for _ in 0..3 {
            let frame = match self.future.take() {
                Some(future) => future,
                None => match self.receive.try_recv() {
                    Ok(frame) => frame,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.missing = true;
                        break;
                    }
                },
            };
            let source = frame.metadata.source_t_us;
            // Conversion or queue delivery can outlive the capture slot.
            if frame.metadata.admission != epoch || source < start { continue; }
            if source >= end {
                self.future = Some(frame);
                break;
            }
            if frame.available < end && accepts(&frame) {
                newest = Some(frame);
            }
        }
        newest
    }
}

struct Video<'a> {
    id: MonitorId,
    worker: &'a WorkerSession,
    layout: &'a AttemptLayout,
    frames: &'a Frames,
    frame_policy: &'a FramePolicy,
    topology: &'a display::topology::Topology,
    changes: &'a Receiver<monitor::FormatRequest>,
    change: Option<monitor::FormatRequest>,
    change_ready: bool,
    evidence: &'a slots::Evidence,
    reports: Option<&'a SyncSender<RecordingReport>>,
    timeline: slots::Timeline,
    opening: AdmissionBoundary,
    cutoff: Option<AdmissionBoundary>,
    next: u64,
    selection: Selection<'a>,
    outstanding: VecDeque<RecordedSlot>,
    headers: h264::Headers,
    current: Option<Segment>,
    segment_number: u32,
    force_pending: bool,
    summary: &'a mut MonitorVideo,
    published_through: u64,
    staged: BTreeMap<u64, slots::Row>,
    preserve_rotation_part: bool,
    required_finalization_failure: Option<&'static str>,
    approval_generation: Option<u64>,
    approval_break: bool,
    geometry: Geometry,
    format: Option<(super::gpu::VideoFormat, bool)>,
}

struct Closing {
    cutoff: Option<AdmissionBoundary>,
    required_finalization_failure: Option<&'static str>,
}

fn record(
    monitor: &SelectedMonitor, worker: &WorkerSession, layout: &AttemptLayout, frames: &Frames, frame_policy: &FramePolicy,
    topology: &display::topology::Topology, receive: Receiver<Observation>, changes: Receiver<monitor::FormatRequest>,
    pacing: &RwLock<Option<(AdmissionEpoch, slots::Timeline)>>, encoder: encoder::Encoder, mut geometry: Geometry,
    summary: &mut MonitorVideo, unavailable_video: &AtomicBool, evidence: &slots::Evidence, reports: Option<&SyncSender<RecordingReport>>,
) -> Result<Closing, ProbeError> {
    let Some(mut start) = worker.wait_for_start()? else {
        encoder.finish()?;
        return Ok(Closing { cutoff: None, required_finalization_failure: None });
    };
    let id = monitor.id;
    let mut fps = encoder.format.fps;
    let mut encoder = Some(encoder);
    let mut origin = start.boundary.time;
    let mut next = 0;
    let mut segment_number = 0;
    let mut required_finalization_failure = None;
    loop {
        let timeline = slots::Timeline::new(origin, fps)?.with_first(next);
        *pacing.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((start.boundary.epoch, timeline));
        let mut state = Video {
            id, worker, layout, frames, frame_policy, topology, changes: &changes, change: None, change_ready: false,
            evidence, reports, timeline,
            opening: start.boundary, cutoff: None, next,
            selection: Selection { receive: &receive, future: None, previous: None, missing: false },
            outstanding: VecDeque::with_capacity(4), headers: h264::Headers::default(), current: None,
            segment_number, force_pending: false, summary: &mut *summary, geometry: geometry.clone(),
            published_through: next, staged: BTreeMap::new(), preserve_rotation_part: false,
            required_finalization_failure: None,
            approval_generation: None, approval_break: false,
            format: encoder.as_ref().map(|encoder| (encoder.format, encoder.software())),
        };
        if let Some(active) = &encoder {
            let encoded = encode::record(&active.transform, active.device.as_ref(), &mut state,
                Video::observe_admission, Video::next_frame, Video::finish_approval_prefix, Video::output);
            match encoded {
                Err(error) if error.journal.is_some() => return Err(error),
                Err(error) => state.flag_failure(&error, unavailable_video),
                Ok(_) if state.preserve_rotation_part => {
                    let error = unavailable("The H.264 encoder drained without a verified rotation IDR");
                    state.flag_failure(&error, unavailable_video);
                }
                Ok(_) => {}
            }
        }
        while let Some(mut skipped) = state.outstanding.pop_front() {
            skipped.row.miss(if state.summary.unavailable { slots::Missing::EncoderUnavailable } else { slots::Missing::EncoderSkipped });
            state.resolve(skipped.row)?;
        }
        if state.preserve_rotation_part {
            if state.current.take().is_some() {
                state.required_finalization_failure.get_or_insert("Publish the segment interrupted during IDR rotation");
            }
        } else if let Err(error) = state.finish_current() {
            if error.journal.is_some() {
                return Err(error);
            }
            state.flag_failure(&error, unavailable_video);
        }
        if state.summary.unavailable {
            if let Some(active) = encoder.take() {
                let _ = active.finish();
            }
        }
        if encoder.is_none() && !state.change_ready {
            let reason = if state.summary.unavailable { slots::Missing::EncoderUnavailable } else { slots::Missing::MonitorMissing };
            state.complete_unavailable(reason)?;
        }
        if state.published_through != state.next || !state.staged.is_empty() {
            return Err(unavailable("The video slot journal does not cover the complete admitted prefix"));
        }
        next = state.next;
        segment_number = state.segment_number;
        required_finalization_failure = required_finalization_failure.or(state.required_finalization_failure);
        let cutoff = state.cutoff;
        let transition = if state.change_ready {
            state.change.take()
        } else {
            if let Some(change) = state.change.take() {
                let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
            }
            None
        };
        let feedback = transition.as_ref().map(|_| state.timeline.slot(next)).transpose()?;
        drop(state);
        if let Some((change, boundary)) = transition.zip(feedback) {
            origin = boundary.start;
            if !matches!(worker.admit()?, Admission::Open(receipt) if receipt.boundary == start.boundary) {
                let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
                continue;
            }
            if let Some(old) = encoder.take() { old.finish()?; }
            let Some(output) = change.output else {
                let _ = change.reply.try_send(monitor::FormatReply::Missing);
                continue;
            };
            let Some(actual) = display::enumerate()?.into_iter().find(|entry| entry.key == monitor.key) else {
                let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
                continue;
            };
            if display::topology::Output::from_native(&actual)? != output {
                let _ = change.reply.try_send(monitor::FormatReply::ChangedAgain);
                continue;
            }
            let luid = stage(unsafe { actual.adapter.GetDesc1() }, "Read the updated display adapter")?.AdapterLuid;
            let device = if let Some(device) = change.existing_device { device } else { capture_device(&actual.adapter)? };
            let encoding = if unavailable_video.load(Ordering::Acquire) {
                // A known encoder failure cannot be retried by topology.
                // The WGC owner can still rebuild its native-only resources.
                None
            } else {
                let next_geometry = geometry_for(&actual, monitor)?;
                let source = [next_geometry.source_size[0] as u32, next_geometry.source_size[1] as u32];
                let requested_fps = worker.config().profile().capture_pace().frames_per_second();
                let activated = encoder::select(&device, &actual.adapter, luid, source, requested_fps)?;
                let format = activated.format;
                let software = activated.software();
                fps = format.fps;
                summary.fps = fps;
                summary.degraded |= software;
                monitor::report_video_ready(worker, id, format, software);
                geometry = video_geometry(next_geometry, format);
                encoder = Some(activated);
                Some((format, software))
            };
            let _ = change.reply.try_send(monitor::FormatReply::Ready { device, encoding, output });
            continue;
        }
        let Some(closed) = cutoff else { return Err(unavailable("The video owner did not receive its closing boundary")) };
        let boundary = PausedVideo { monitor: id, first_paused_slot: next, fps };
        worker.finish_video_pause(closed, boundary);
        if !worker.pause_pending(closed) {
            if let Some(active) = encoder.take() { active.finish()?; }
            return Ok(Closing { cutoff, required_finalization_failure });
        }
        let Some(opened) = worker.wait_for_reopen(closed)? else {
            if let Some(active) = encoder.take() { active.finish()?; }
            return Ok(Closing { cutoff, required_finalization_failure });
        };
        next = boundary.end_exclusive(closed.time, opened.boundary.time)
            .ok_or_else(|| unavailable("The paused video slot range overflowed"))?;
        start = opened;
        origin = start.boundary.time;
    }
}

impl Video<'_> {
    fn observe_admission(&mut self) -> Result<(), ProbeError> {
        if self.change.is_none() {
            self.change = self.changes.try_recv().ok();
        }
        let at = match self.worker.admit()? {
            Admission::Open(receipt) if receipt.boundary == self.opening => {
                receipt.clock.time(receipt.qpc).map_err(clock_error)?
            }
            Admission::Closed(Some(boundary)) if boundary.epoch == self.opening.epoch => {
                self.cutoff = Some(boundary);
                if self.worker.segment_deadline().is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                    return Err(unavailable("The video segment drain deadline expired"));
                }
                boundary.time
            }
            _ => return Err(unavailable("Video admission changed without closing its slot prefix")),
        };
        if !self.approval_break && self.preserve_rotation_part && self.current.as_ref().is_some_and(|segment| {
            at.0.saturating_sub(segment.start.0) > ROTATION_US + IDR_ALLOWANCE_US
        }) {
            return Err(unavailable("The H.264 encoder did not produce a verified rotation IDR within one second"));
        }
        Ok(())
    }

    fn next_frame(&mut self) -> Result<encode::Next, ProbeError> {
        let slot = self.timeline.slot(self.next)?;
        if self.change_at_slot(slot)? {
            return Ok(encode::Next::End);
        }
        let (at, end, cutoff) = match self.worker.admit()? {
            Admission::Open(receipt) if receipt.boundary == self.opening => {
                let now = receipt.clock.time(receipt.qpc).map_err(clock_error)?;
                if now < slot.end {
                    return Ok(encode::Next::Wait);
                }
                (now, slot.end, None)
            }
            Admission::Closed(Some(boundary)) if boundary.epoch == self.opening.epoch => {
                self.cutoff = Some(boundary);
                if slot.start >= boundary.time {
                    return Ok(encode::Next::End);
                }
                (boundary.time, slot.end.min(boundary.time), Some(boundary.time))
            }
            _ => return Err(unavailable("Video admission changed without closing its slot prefix")),
        };
        let approval = match self.frame_policy.try_current(self.opening.epoch, at) {
            Ok(approval) => approval,
            Err(ApprovalReadError::NewerThanReceipt) if cutoff.is_some() => None,
            Err(_) => return Ok(encode::Next::Wait),
        };
        let approval = approval.filter(|approval| approval.contains_slot(slot.start));
        if self.approval_generation != approval.map(|approval| approval.generation) {
            self.approval_break |= self.approval_generation.is_some();
            self.approval_generation = approval.map(|approval| approval.generation);
        }
        if self.approval_break {
            if !self.outstanding.is_empty() {
                return Ok(encode::Next::DrainPrefix);
            }
            self.finish_approval_prefix()?;
        }
        if self.current.as_ref().is_some_and(|segment| {
            slot.start.0.saturating_sub(segment.start.0) > ROTATION_US + IDR_ALLOWANCE_US
        }) {
            self.preserve_rotation_part = true;
            return Err(unavailable("The H.264 encoder did not produce a verified rotation IDR within one second"));
        }
        let Ok(geometry) = self.topology.geometry(self.id) else { return Ok(encode::Next::Wait) };
        let accepts = |frame: &Observation| geometry == Some(frame.metadata.geometry.id)
            && approval.is_some_and(|approval| approval.generation == frame.metadata.approval_generation);
        if self.selection.previous.as_ref().is_some_and(|frame| !accepts(frame)) {
            self.selection.previous = None;
        }
        let fresh = self.selection.newest(self.opening.epoch, slot.start, end, accepts);
        let repeated = fresh.is_none();
        if let Some(frame) = fresh {
            self.selection.previous = Some(frame);
        }
        let source = self.selection.previous.as_ref().map(|frame| slots::Source {
            metadata: frame.metadata, change: frame.change, repeated,
        });
        let mut row = slots::Row::pending(self.id, self.opening.epoch, slot, Some(at), source, (repeated && source.is_some()).then(|| self.evidence.late(slot.index)));
        self.next = self.next.checked_add(1).ok_or_else(|| unavailable("The video slot count overflowed"))?;
        if approval.is_none() {
            self.summary.approval_suppressed += 1;
            row.miss(slots::Missing::ApprovalSuppressed);
        } else if geometry.is_none() || (self.selection.missing && cutoff.is_none()) {
            row.miss(slots::Missing::CaptureUnavailable);
        } else if slot.end.0 < at.0.saturating_sub(slot.end.0 - slot.start.0) {
            row.miss(slots::Missing::QueuePressure);
        } else if repeated && let Some(pressure) = self.evidence.pressure(slot.index) {
            row.miss(pressure);
        } else if self.selection.previous.is_none() {
            row.miss(slots::Missing::BeforeFirstFrame);
        }
        if row.reason.is_some() {
            self.stage_slot(row)?;
            return Ok(encode::Next::Wait);
        }
        let input = self.selection.previous.as_ref()
            .ok_or_else(|| unavailable("The selected video slot has no input frame"))?.input.clone();
        let pts = slot.pts()?;
        let duration = slot.duration(cutoff)?;
        let due = self.current.as_ref().is_none_or(|segment| {
            slot.start.0.saturating_sub(segment.start.0) >= ROTATION_US
        });
        let force_idr = due && !self.force_pending;
        self.force_pending |= force_idr;
        if self.current.is_some() && force_idr {
            self.preserve_rotation_part = true;
        }
        self.stage_slot(row.clone())?;
        self.outstanding.push_back(RecordedSlot { row, pts, duration, forced: force_idr });
        Ok(encode::Next::Frame { input, pts, duration, force_idr })
    }

    fn output(&mut self, transform: &IMFTransform, sample: &IMFSample) -> Result<usize, ProbeError> {
        let pts = stage(unsafe { sample.GetSampleTime() }, "Read the encoded picture timestamp")?;
        let mut retired = 1;
        while self.outstanding.front().is_some_and(|pending| pending.pts < pts) {
            let Some(mut skipped) = self.outstanding.pop_front() else { break };
            self.force_pending &= !skipped.forced;
            skipped.row.miss(slots::Missing::EncoderSkipped);
            self.resolve(skipped.row)?;
            retired += 1;
        }
        let pending = self.outstanding.pop_front().filter(|pending| pending.pts == pts)
            .ok_or_else(|| unavailable("The encoder produced a frame outside the submitted slot prefix"))?;
        self.force_pending &= !pending.forced;
        let boundary_due = self.current.as_ref().is_none_or(|segment| {
            pending.row.pts_us.0.saturating_sub(segment.start.0) >= ROTATION_US
        });
        if boundary_due && self.current.is_some() {
            self.preserve_rotation_part = true;
        }
        let media = if boundary_due {
            let media = stage(unsafe { transform.GetOutputCurrentType(0) }, "Read the H.264 media type")?;
            self.headers.read_media_type(&media)?;
            Some(media)
        } else {
            None
        };
        let picture = self.headers.inspect(sample)?;
        if picture.b_slice {
            return Err(unavailable("The H.264 encoder returned a B-frame"));
        }
        if let Some(media) = media {
            if picture.idr {
                let sequence = self.headers.segment_header(&picture)?;
                // Keep ownership of the boundary sample and its actual parameter
                // sets while the prior muxer finishes synchronously on this worker.
                // Capture keeps only its bounded queue until we resume consumption.
                self.preserve_rotation_part = false;
                self.finish_current()?;
                self.segment_number = self.segment_number.checked_add(1)
                    .ok_or_else(|| unavailable("The MP4 segment index overflowed"))?;
                let id = SegmentId(self.segment_number);
                let path = self.layout.path(ArtifactKind::VideoSegment { monitor: self.id, segment: id });
                stage(unsafe { media.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &sequence) },
                    "Attach verified SPS and PPS to the new MP4 stream")?;
                let (format, software) = self.format.ok_or_else(|| unavailable("The segment encoding mode is unavailable"))?;
                let encoding = SegmentEncoding {
                    admission_epoch: pending.row.admission_epoch,
                    first_frame: pending.row.frame_id.ok_or_else(|| unavailable("The segment first frame has no capture identity"))?,
                    first_geometry: pending.row.geometry_version.ok_or_else(|| unavailable("The segment first frame has no geometry identity"))?,
                    fps: format.fps, software,
                    encoder_target_bitrate_bps: format.bitrate(),
                    media_mean_bitrate_bps: format.bitrate() * 3 / 4,
                };
                self.current = Some(Segment {
                    id, start: pending.row.pts_us, offset: pts, encoding, mux: mp4::Recording::open(&media, &path)?,
                    index: Vec::new(), pending: None,
                });
            } else if self.current.is_none() {
                return Err(unavailable("The first encoded frame is not an IDR"));
            }
        }
        self.current.as_mut().ok_or_else(|| unavailable("The MP4 segment is unavailable"))?
            .push(pending, sample, picture.idr)?;
        Ok(retired)
    }

    fn finish_approval_prefix(&mut self) -> Result<(), ProbeError> {
        // The MFT either had no pending input or completed its drain.
        // Any input it did not produce is an explicit skipped slot.
        while let Some(mut skipped) = self.outstanding.pop_front() {
            skipped.row.miss(slots::Missing::EncoderSkipped);
            self.resolve(skipped.row)?;
        }
        self.finish_current()?;
        self.approval_break = false;
        self.force_pending = false;
        self.preserve_rotation_part = false;
        Ok(())
    }

    fn finish_current(&mut self) -> Result<(), ProbeError> {
        if let Some(segment) = self.current.take() {
            let (segment, rows) = match segment.finish(self.id, self.geometry.clone(), self.worker) {
                Ok(completed) => completed,
                Err(error) => {
                    self.required_finalization_failure.get_or_insert(error.stage);
                    return Err(error);
                }
            };
            self.summary.encoded += segment.frame_count;
            self.summary.segments.push(segment);
            for row in rows {
                self.resolve(row)?;
            }
            self.frames.flush()?;
            self.worker.request_journal_flush();
        }
        Ok(())
    }

    fn stage_slot(&mut self, row: slots::Row) -> Result<(), ProbeError> {
        if self.staged.len() >= MAX_STAGED_SLOTS || row.slot < self.published_through
            || self.staged.contains_key(&row.slot)
        {
            return Err(unavailable("The staged video slot prefix exceeded its bound or contained a duplicate"));
        }
        self.staged.insert(row.slot, row);
        self.publish_resolved()
    }

    fn resolve(&mut self, row: slots::Row) -> Result<(), ProbeError> {
        let prior = self.staged.get_mut(&row.slot)
            .filter(|prior| !prior.encoded && prior.reason.is_none())
            .ok_or_else(|| unavailable("The H.264 output does not match an unresolved slot"))?;
        *prior = row;
        self.publish_resolved()
    }

    fn publish_resolved(&mut self) -> Result<(), ProbeError> {
        while self.staged.get(&self.published_through).is_some_and(|row| row.encoded || row.reason.is_some()) {
            let mut row = self.staged.remove(&self.published_through)
                .ok_or_else(|| unavailable("The resolved video slot is unavailable"))?;
            if row.repeated == Some(true) {
                row.late = Some(self.evidence.late(row.slot));
            }
            self.frames.publish(self.worker, &row)?;
            self.published_through += 1;
            self.summary.slots += 1;
            let counts = &mut self.summary.counters;
            if let Some(reason) = row.reason {
                *counts.missing_by_reason.entry(reason).or_default() += 1;
            } else if row.encoded {
                counts.indexed_encoded += 1;
                counts.repeated += u64::from(row.repeated == Some(true));
                counts.changed_frame_arrived_late += u64::from(row.late == Some(true));
            }
        }
        Ok(())
    }

    fn flag_failure(&mut self, error: &ProbeError, capture: &AtomicBool) {
        self.summary.unavailable = true;
        capture.store(true, Ordering::Release);
        let code = error.source.as_ref().map(|source| source.code().0);
        if let Some(failure) = self.worker.video_failed(self.id, Failure::new(FailureComponent::Video, error.stage, code))
            && self.reports.is_none_or(|reports| reports.try_send(RecordingReport::Video(failure)).is_err())
        {
            self.worker.set_video_report(self.id, ReportOutcome::Unavailable { native_code: None });
        }
    }

    fn change_at_slot(&mut self, slot: slots::Slot) -> Result<bool, ProbeError> {
        if self.change.as_ref().is_some_and(|change| slot.start >= change.at)
            && self.cutoff.is_none_or(|cutoff| slot.start < cutoff.time)
        {
            self.change_ready = true;
            return Ok(true);
        }
        Ok(false)
    }

    fn complete_unavailable(&mut self, reason: slots::Missing) -> Result<(), ProbeError> {
        for row in self.staged.values_mut() {
            if !row.encoded && row.reason.is_none() {
                row.miss(if self.preserve_rotation_part { slots::Missing::SegmentUnpublished } else { reason });
            }
        }
        self.publish_resolved()?;
        while self.published_through < self.next {
            let slot = self.timeline.slot(self.published_through)?;
            self.publish_unavailable(slot, reason)?;
        }
        loop {
            if self.change.is_none() { self.change = self.changes.try_recv().ok(); }
            let slot = self.timeline.slot(self.next)?;
            match self.worker.admit()? {
                Admission::Closed(Some(boundary)) if boundary.epoch == self.opening.epoch => {
                    self.cutoff = Some(boundary);
                    if self.next >= self.timeline.closed_prefix(boundary.time)? {
                        return Ok(());
                    }
                }
                Admission::Open(receipt) if receipt.boundary == self.opening => {
                    if self.change_at_slot(slot)? { return Ok(()); }
                    if receipt.clock.time(receipt.qpc).map_err(clock_error)? < slot.end {
                        let _ = self.worker.wait_for_close(Duration::from_millis(5))?;
                        continue;
                    }
                }
                _ => return Err(unavailable("Unavailable video did not receive its admission cutoff")),
            }
            self.publish_unavailable(slot, reason)?;
            self.next += 1;
        }
    }

    fn publish_unavailable(&mut self, slot: slots::Slot, reason: slots::Missing) -> Result<(), ProbeError> {
        let mut row = slots::Row::pending(self.id, self.opening.epoch, slot, self.worker.session_time(), None, None);
        row.miss(reason);
        self.stage_slot(row)
    }

}

struct Segment {
    id: SegmentId,
    start: SessionTimeUs,
    offset: i64,
    encoding: SegmentEncoding,
    mux: mp4::Recording,
    index: Vec<slots::Row>,
    pending: Option<(RecordedSlot, IMFSample, bool)>,
}

impl Segment {
    fn push(&mut self, slot: RecordedSlot, sample: &IMFSample, idr: bool) -> Result<(), ProbeError> {
        if let Some((prior, picture, prior_idr)) = self.pending.take() {
            let duration = slot.pts.checked_sub(prior.pts)
                .filter(|&duration| duration > 0).ok_or_else(|| unavailable("The MP4 timestamps are not increasing"))?;
            self.write(prior, picture, duration, prior_idr)?;
        }
        self.pending = Some((slot, sample.clone(), idr));
        Ok(())
    }

    fn write(&mut self, mut slot: RecordedSlot, sample: IMFSample, duration: i64, idr: bool) -> Result<(), ProbeError> {
        let pts = slot.pts.checked_sub(self.offset)
            .filter(|&pts| pts >= 0).ok_or_else(|| unavailable("The segment-relative PTS is invalid"))?;
        stage(unsafe { sample.SetSampleTime(pts) }, "Set the segment-local PTS")?;
        stage(unsafe { sample.SetSampleDuration(duration) }, "Set the segment-local duration")?;
        stage(unsafe {
            sample.SetUINT64(&MFSampleExtension_DecodeTimestamp, pts as u64)
        }, "Set the segment-local DTS")?;
        stage(unsafe {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, u32::from(idr))
        }, "Set verified H.264 sample attributes")?;
        if self.index.len() >= MAX_SEGMENT_PICTURES {
            return Err(unavailable("The open MP4 segment exceeded its frame index bound"));
        }
        self.mux.write(&sample)?;
        slot.row.verified(self.id, self.index.len() as u64, pts, duration);
        self.index.push(slot.row);
        Ok(())
    }

    fn finish(
        mut self, monitor: MonitorId, geometry: Geometry, worker: &WorkerSession,
    ) -> Result<(SegmentSummary, Vec<slots::Row>), ProbeError> {
        if let Some((slot, picture, idr)) = self.pending.take() {
            let duration = slot.duration;
            self.write(slot, picture, duration, idr)?;
        }
        let (local_end_100ns, published_bytes) = self.mux.finish(&mut self.index, geometry.video_size, || worker.segment_deadline())?;
        Ok((SegmentSummary {
            monitor, segment: self.id, common_start_us: self.start, frame_count: self.index.len() as u64,
            local_end_100ns, published_bytes: Some(published_bytes), geometry, encoding: Some(self.encoding),
        }, self.index))
    }
}
