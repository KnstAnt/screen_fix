mod display;
pub(crate) mod ui_window;
mod os;
pub(crate) mod preflight;
mod hdr;
mod monitor;
mod native;
mod pixels;
mod readback;
mod still_time;
mod still_png;
mod still_rows;
mod stills;
mod wgc;
mod gpu;
mod h264;
mod slots;
mod video;
mod encode;
mod encoder;
mod mp4;
mod mf;
mod input;
pub(crate) use input::on_input_desktop;
mod input_helper;
pub(crate) mod elevated;
pub(crate) mod hotkeys;
mod context;
mod clipboard;
pub(crate) mod clock;

use std::{collections::BTreeMap, fmt, sync::{atomic::{AtomicBool, Ordering}, mpsc}, thread};

use crate::{
    CaptureError, CaptureErrorKind,
    package::{
        ArtifactKind,
        diagnostics::{DiagnosticContext, DiagnosticSnapshot, FailureComponent, ReportOutcome},
        journal::{FinalHealth, JournalError, JournalIngress, JournalOwner, JournalStep},
        metadata::{AttemptLayout, CaptureDisposition, InputBlind, MetadataOwner, MonitorCapture, MonitorVideo, PackageError, PngCounters},
    },
    protocol::StreamId,
    session::{DiskPreflight, SessionOutcome, SessionResult, StartupStep, windows::{Admission, InputFailure, MAX_INPUT_FAILURES, VideoFailure, WorkerSession, disk}},
};

use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
            },
            Dxgi::IDXGIAdapter1,
        },
        System::{
            Com::{ COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
        },
    },
    core::{self, Interface},
};

pub(crate) struct ProbeError {
    stage: &'static str,
    source: Option<core::Error>,
    journal: Option<JournalError>,
    raw_input_registration: Option<crate::protocol::ipc::RawInputRegistrationMismatch>,
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(journal) = &self.journal {
            return fmt::Display::fmt(journal, f);
        }
        f.write_str(self.stage)?;
        if let Some(source) = &self.source {
            write!(f, " ({:#010x}): {source}", source.code().0 as u32)?;
        }
        Ok(())
    }
}

impl From<ProbeError> for crate::CaptureError {
    fn from(error: ProbeError) -> Self {
        let mut error = error;
        if let Some(journal) = error.journal.take() {
            return Self::journal(error.stage, journal);
        }
        let code = error.source.as_ref().map(|source| source.code().0);
        Self::backend(error.stage, code, error.to_string())
            .with_raw_input_registration(error.raw_input_registration)
    }
}

fn stage<T>(result: core::Result<T>, name: &'static str) -> Result<T, ProbeError> {
    result.map_err(|source| ProbeError { stage: name, source: Some(source), journal: None, raw_input_registration: None })
}

pub(crate) fn unavailable(stage: &'static str) -> ProbeError {
    ProbeError { stage, source: None, journal: None, raw_input_registration: None }
}

struct Apartment;

impl Apartment {
    fn initialize() -> Result<Self, ProbeError> {
        stage(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }, "Initialize COM")?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // This guard stays on the thread that initialized COM.
        unsafe { CoUninitialize() };
    }
}

pub(super) fn record_attempt(worker: &WorkerSession) -> Result<SessionResult, CaptureError> {
    let cancelled = || Ok(SessionResult { outcome: SessionOutcome::Cancelled, message: "Recording cancelled before capture started.".into() });
    if worker.stop_requested() {
        return cancelled();
    }
    let windows = os::check()?;
    worker.windows_evidence(windows.clone());
    if worker.stop_requested() {
        return cancelled();
    }
    worker.startup_step(StartupStep::ApprovedApplication);
    preflight::validate_processes(worker.config())?;
    worker.window_coverage(preflight::window_coverage(worker.config()));
    if worker.stop_requested() {
        return cancelled();
    }
    worker.startup_step(StartupStep::OutputSpace);
    let (disk, preflight) = disk::Monitor::start(worker)?;
    if worker.stop_requested() {
        return cancelled();
    }
    record_with_disk(worker, preflight, disk, windows)
}

fn record_with_disk(worker: &WorkerSession, preflight: DiskPreflight, disk: disk::Monitor, windows: crate::platform::WindowsEvidence) -> Result<SessionResult, CaptureError> {
    let config = worker.config();
    let mut metadata = MetadataOwner::create(config, preflight, windows)
        .map_err(package_error)?;
    let layout = metadata.layout().clone();
    let mut diagnostics = DiagnosticContext::for_config(config);
    diagnostics.attempt = Some(layout.id().clone());
    let mut owners = Vec::new();
    let mut monitor_capture = Vec::new();
    let mut monitor_epochs = Vec::new();
    let output: Result<RecordedVideo, CaptureError> = (|| {
        let mut stream = |id| {
            let (ingress, owner) = spawn_journal(worker, id, layout.path(ArtifactKind::Journal(id)))?;
            owners.push((id, owner));
            Ok::<_, CaptureError>(ingress)
        };
        let input = stream(StreamId::Input)?;
        let context = stream(StreamId::Context)?;
        let frames = stream(StreamId::Frames)?;
        let stills = stream(StreamId::Stills)?;
        record(worker, &layout, input, context, video::Frames::new(frames), stills, &mut monitor_capture, &mut monitor_epochs)
            .map_err(Into::into)
    })();

    let (video, png, mut producer_prefix, mut failure, required_finalization_failure) = match output {
        Ok(output) => (output.video, output.png, output.input, output.failure.map(Into::into), output.required_finalization_failure),
        Err(error) => (Vec::new(), None, None, Some(error), None),
    };
    let mut streams = BTreeMap::new();
    let mut context_owner = None;
    for (stream, owner) in owners {
        if stream == StreamId::Context {
            context_owner = Some(owner);
            continue;
        }
        match owner.finish() {
            Ok(summary) => { streams.insert(stream, summary); }
            Err(error) => {
                let error = CaptureError::from(package_journal_error(error));
                if failure.as_ref().is_none_or(|previous| previous.native_code.is_none()) {
                    failure = Some(error);
                }
            }
        }
    }
    if let Some(owner) = context_owner {
        let mut health: Vec<_> = worker.video_failures_after(0).into_iter().filter(|entry| {
            producer_prefix.as_ref().is_none_or(|prefix| !prefix.video_health.contains(&entry.monitor))
        }).map(|entry| FinalHealth { at: entry.at, failure: entry.failure, monitor: Some(entry.monitor), blind: None }).collect();
        let source_error = worker.source_failure().map(CaptureError::elevated);
        if let Some(error) = source_error.as_ref().or(failure.as_ref()) {
            health.push(FinalHealth { at: worker.session_time(), failure: error.failure(), monitor: None, blind: None });
        }
        for input in worker.input_failures() {
            worker.update_standard_evidence(|evidence| { evidence.blind_reason.get_or_insert(input.reason); });
            let end_t_us = producer_prefix.as_ref().and_then(|prefix| prefix.cutoff)
                .map(|boundary| boundary.time).or_else(|| worker.session_time()).unwrap_or(input.at);
            let blind = InputBlind { start_t_us: input.at.min(end_t_us), end_t_us, reason: input.reason };
            health.push(FinalHealth { at: Some(input.at), failure: input.failure, monitor: None, blind: Some(blind) });
            if let Some(capture) = producer_prefix.as_mut().and_then(|prefix| prefix.capture.as_mut()) {
                capture.blind.get_or_insert(blind);
            }
        }
        match owner.finish_with_health(health) {
            Ok(summary) => { streams.insert(StreamId::Context, summary); }
            Err(error) => {
                let error = CaptureError::from(package_journal_error(error));
                if failure.as_ref().is_none_or(|previous| previous.native_code.is_none()) {
                    failure = Some(error);
                }
            }
        }
    }
    diagnostics.input_registration = worker.input_registration();
    metadata.set_input_capture(
        producer_prefix.as_mut().and_then(|prefix| prefix.capture.take()), diagnostics.input_registration,
    );
    if failure.is_none() && let Some(prefix) = producer_prefix {
        let matches_prefix = |id, prefix: &input::ReceiptPrefix| streams.get(&id).is_some_and(|summary| {
            summary.data_rows == prefix.queued && summary.last_data_receipt == prefix.last_queued
        });
        let frame_count = video.iter().map(|monitor| monitor.slots).sum::<u64>();
        let frames_match = streams.get(&StreamId::Frames).is_some_and(|summary| summary.data_rows == frame_count);
        if !matches_prefix(StreamId::Input, &prefix.input)
            || !matches_prefix(StreamId::Context, &prefix.context)
            || !frames_match
        {
            failure = Some(unavailable("The written journals do not match the admitted producer prefixes").into());
        } else if let Some(cutoff) = prefix.cutoff {
            failure = worker.complete_prefix(cutoff).err().map(Into::into);
        }
    }
    drop(disk);
    if let Some(operation) = required_finalization_failure {
        let error = CaptureError::from(unavailable(operation));
        if failure.is_some() {
            diagnostics.additional_failures.push(error.failure());
        } else {
            failure = Some(error);
        }
    }
    if let Some(source) = worker.source_failure() {
        if let Some(previous) = failure.replace(CaptureError::elevated(source))
            && previous.source_failure() != Some(source)
        {
            diagnostics.additional_failures.push(previous.failure());
        }
    }
    if let Some(operation) = worker.controller_failure() {
        failure.get_or_insert_with(|| unavailable(operation).into());
    }
    let failed_step = failure.as_ref().map(|error| error.operation);
    let frame_count = video.iter().map(|monitor| monitor.encoded).sum::<u64>();
    let unavailable_video = config.monitors().iter().filter(|selection| {
        !video.iter().any(|monitor| monitor.monitor == selection.id && !monitor.unavailable && monitor.encoded != 0)
    }).count();
    diagnostics.streams = streams.clone();
    diagnostics.capture = monitor_capture.iter().map(|mode| (mode.geometry.id.monitor, mode.counters.clone())).collect();
    diagnostics.video = video.iter().map(|monitor| (monitor.monitor, monitor.counters.clone())).collect();
    diagnostics.png = png.clone();
    diagnostics.session_time_us = worker.session_time();
    diagnostics.source_evidence = worker.source_evidence();
    metadata.set_source_evidence(diagnostics.source_evidence.clone());
    metadata.set_monitor_capture(monitor_capture, monitor_epochs);
    metadata.set_clock(worker.attempt_clock());
    metadata.set_png(png);
    let publication = metadata.finish_recording(
        failed_step, streams, video, || worker.stop_deadline(), || worker.authorize_complete_publication(),
    ).map_err(package_error);
    if let Some(mut error) = failure {
        let saved = matches!(&publication, Ok(CaptureDisposition::Failed));
        diagnostics.failed_manifest_published = Some(saved);
        if let Err(publication_error) = publication {
            diagnostics.additional_failures.push(publication_error.failure());
        }
        if saved && error.source_failure().is_some_and(|source| source.admitted) {
            error = error.with_retained_recording(layout.root().to_owned(), frame_count);
        }
        return Err(error.with_context(diagnostics));
    }
    let outcome = match publication {
        Ok(outcome) => outcome,
        Err(error) => return Err(error.with_context(diagnostics)),
    };
    let root = layout.root().display();
    let mut message = match outcome {
        CaptureDisposition::VideoUnavailable => {
            let mut message = format!("Recording incomplete. Video unavailable on {unavailable_video} selected monitor(s). Saved {frame_count} verified frames.\n{root}");
            for report in worker.video_reports() {
                message.push_str(&format!("\nMonitor {} · {}", report.monitor.0, report.outcome));
            }
            message
        }
        CaptureDisposition::InputUnavailable => format!(
            "Recording incomplete. The input prefix could not be certified. Saved {frame_count} verified video frames.\n{root}"),
        CaptureDisposition::ApprovalSuppressed => format!(
            "Recording incomplete. No video frames met the foreground approval policy.\n{root}"),
        CaptureDisposition::StoppedBeforeFirstFrame if !worker.started() => format!(
            "Recording cancelled before capture started.\n{root}"),
        CaptureDisposition::StoppedBeforeFirstFrame => format!(
            "Recording incomplete. No video frames were verified before Stop.\n{root}"),
        CaptureDisposition::Recorded => format!("Recording complete. Frames: {frame_count}.\n{root}"),
        CaptureDisposition::Pending | CaptureDisposition::Failed =>
            return Err(unavailable("The recording manifest has an unexpected final disposition").into()),
    };
    if let Some(summary) = diagnostics.source_evidence.as_ref().map(|evidence| evidence.summary()) {
        if let Some(mode) = summary.mode_label() {
            message.push_str(&format!("\nCapture source: {mode}."));
        }
    }
    if worker.input_failure().is_some() {
        message.push_str("\nInput capture was degraded.");
        if let Some(report) = worker.input_report() {
            message.push_str(&format!(" {report}"));
        }
    }
    let outcome = match outcome {
        CaptureDisposition::Recorded => SessionOutcome::Complete,
        CaptureDisposition::StoppedBeforeFirstFrame if !worker.started() => SessionOutcome::Cancelled,
        _ => SessionOutcome::Incomplete,
    };
    Ok(SessionResult { outcome, message })
}

fn spawn_journal(
    worker: &WorkerSession, stream: StreamId, path: std::path::PathBuf,
) -> Result<(JournalIngress, JournalOwner), CaptureError> {
    let worker = worker.clone();
    JournalOwner::spawn(stream, path, move || worker.request_stop())
        .map_err(package_journal_error).map_err(Into::into)
}

fn package_error(error: PackageError) -> CaptureError {
    let (operation, code) = match &error {
        PackageError::Io { operation, source } => (operation.message(), source.raw_os_error()),
        PackageError::InconsistentInventory => ("Reconcile the closed recording inventory", None),
        _ => ("Read the recording package", None),
    };
    CaptureError::classified(CaptureErrorKind::PackageFailure, FailureComponent::PackageMetadata, operation, code, error.to_string())
}

pub(super) fn package_journal_error(error: JournalError) -> ProbeError {
    let step = match error.step {
        JournalStep::Start => "Start the journal owner",
        JournalStep::Create => "Create the journal file",
        JournalStep::Order => "Order the journal stream",
        JournalStep::Encode => "Encode the journal row",
        JournalStep::Append => "Append the journal row",
        JournalStep::BufferedFlush => "Flush the journal buffer",
        JournalStep::OsFlush => "Flush the journal to disk",
        JournalStep::Close => "Close the journal",
    };
    ProbeError { stage: step, source: None, journal: Some(error), raw_input_registration: None }
}

struct RecordedVideo {
    video: Vec<MonitorVideo>,
    png: Option<PngCounters>,
    input: Option<input::InputPrefix>,
    failure: Option<ProbeError>,
    required_finalization_failure: Option<&'static str>,
}

pub(super) enum RecordingReport {
    Video(VideoFailure),
    Input(InputFailure),
}

fn record(
    worker: &WorkerSession, layout: &AttemptLayout, input: JournalIngress,
    context: JournalIngress, frames: video::Frames, stills: JournalIngress, monitor_capture: &mut Vec<MonitorCapture>, monitor_epochs: &mut Vec<MonitorCapture>,
) -> Result<RecordedVideo, ProbeError> {
    let cancelled = || Ok(RecordedVideo {
        video: Vec::new(), png: None, input: Some(input::InputPrefix::cancelled()), failure: None, required_finalization_failure: None,
    });
    if worker.stop_requested() {
        return cancelled();
    }
    worker.startup_step(StartupStep::MediaFoundation);
    let _apartment = Apartment::initialize()?;
    let _media_foundation = mf::Runtime::initialize()?;
    worker.startup_step(StartupStep::DisplaysAndEncoders);
    let borderless = wgc::request_borderless();
    if worker.stop_requested() {
        return cancelled();
    }
    // Optional file resources are read before the native input/capture owners
    // start. The controller remains responsive if Windows blocks this IO.
    let executable_versions = context::ExecutableVersions::read(worker.config());
    if worker.stop_requested() {
        return cancelled();
    }
    let frame_policy = &context::FramePolicy::default();
    let topology = &display::topology::Topology::default();
    let topology_stop = &AtomicBool::new(false);
    thread::scope(|scope| {
        topology.spawn(scope, worker, topology_stop)?;
        let _stop_topology = TopologyStop(topology_stop);
        let (png, mut lanes, still_worker) = still_png::Worker::new(worker.config().monitors().len() + 1);
        let still_rows = lanes.pop().expect("the input still producer has a lane");
        let mut lanes = lanes.into_iter();
        let still_owner = thread::Builder::new().name("native-png".into())
            .spawn_scoped(scope, move || still_worker.run(layout, stills, worker))
            .map_err(|error| mp4::io_error("Start the native PNG worker", error))?;
        // Each selected monitor can report a terminal video failure once.
        // Report writes are independent of admission and capture/video threads.
        let (reports, pending_reports) = mpsc::sync_channel::<RecordingReport>(worker.config().monitors().len() + MAX_INPUT_FAILURES);
        let reporter = thread::Builder::new().name("recording-diagnostic-report".into()).spawn_scoped(scope, move || {
            for failure in pending_reports {
                let mut context = DiagnosticContext::for_config(worker.config());
                context.attempt = Some(layout.id().clone());
                match failure {
                    RecordingReport::Video(failure) => {
                        context.session_time_us = failure.at;
                        context.failed_monitor = Some(failure.monitor);
                        let outcome = DiagnosticSnapshot::new(failure.failure).with_context(context).write();
                        worker.set_video_report(failure.monitor, outcome);
                    }
                    RecordingReport::Input(failure) => {
                        context.session_time_us = Some(failure.at);
                        let outcome = DiagnosticSnapshot::new(failure.failure).with_context(context).write();
                        worker.set_input_report_for(failure, outcome);
                    }
                }
            }
        });
        let reports = reporter.is_ok().then_some(reports);
        let mut owners = Vec::new();
        let mut readiness = Vec::new();
        let mut still_endpoints = BTreeMap::new();
        let frames = &frames;
        let mut startup_error = None;
        for selected in worker.config().monitors() {
            if worker.stop_requested() {
                break;
            }
            let (ready, initialized) = mpsc::sync_channel(1);
            let report = reports.clone();
            let png = png.clone();
            let still_rows = lanes.next().expect("each monitor has a still producer lane");
            match thread::Builder::new().name(format!("video-monitor-{}", selected.id.0))
                .spawn_scoped(scope, move || video::run(selected, worker, layout, frames, frame_policy, topology, borderless, ready, report, png, still_rows))
            {
                Ok(owner) => {
                    owners.push((selected.id, owner));
                    readiness.push(initialized);
                }
                Err(error) => {
                    startup_error = Some(mp4::io_error("Start the monitor video owner", error));
                    break;
                }
            }
        }
        if startup_error.is_none() {
            for ready in readiness {
                match ready.recv() {
                    Ok(monitor::Ready { capture, stills }) => {
                        still_endpoints.insert(capture.geometry.id.monitor, stills);
                        monitor_capture.push(capture);
                    }
                    Err(_) => {
                        startup_error = Some(unavailable("A selected monitor did not pass capture and encoder preflight"));
                        break;
                    }
                }
            }
        }
        let input = if startup_error.is_none() && worker.stop_requested() {
            Ok(Some((None, input::InputPrefix::cancelled())))
        } else if startup_error.is_none() {
            worker.startup_step(StartupStep::InputHelper);
            input::with_journal(layout, frame_policy, input, context, worker, topology, &still_endpoints, &still_rows, reports.as_ref(), executable_versions, || {
                if let Admission::Closed(cutoff) = worker.admit()? {
                    return Ok(cutoff);
                }
                worker.ready()?;
                worker.verify_startup(|| {
                    let config = worker.config();
                    worker.window_coverage(preflight::window_coverage(config));
                    preflight::validate_processes(config)?;
                    preflight::validate_foreground(config)
                });
                worker.wait_for_stop()
            }).map(Some)
        } else {
            Ok(None)
        };
        drop(reports);
        worker.request_stop();
        let mut videos = Vec::new();
        let mut required_finalization_failure = None;
        let mut worker_error = None;
        for (id, owner) in owners {
            match owner.join() {
                Ok(Ok(result)) => {
                    if let Some(mut mode) = result.epochs.last().cloned() {
                        mode.counters = result.capture;
                        if let Some(previous) = monitor_capture.iter_mut().find(|monitor| monitor.geometry.id.monitor == id) {
                            *previous = mode;
                        } else {
                            monitor_capture.push(mode);
                        }
                    }
                    monitor_epochs.extend(result.epochs);
                    match result.outcome {
                        Ok(capture_cutoff) => {
                            if let Ok(Some((cutoff, _))) = &input && capture_cutoff != *cutoff {
                                worker_error.get_or_insert_with(|| unavailable("A monitor closed at the wrong admission boundary"));
                            }
                        }
                        Err(error) => { worker_error.get_or_insert(error); }
                    }
                    required_finalization_failure = required_finalization_failure.or(result.required_finalization_failure);
                    videos.push(result.video);
                }
                Ok(Err(error)) => { worker_error.get_or_insert(error); }
                Err(_) => { worker_error.get_or_insert_with(|| unavailable("A monitor video owner terminated unexpectedly")); }
            }
        }
        if let Ok(reporter) = reporter {
            let _ = reporter.join();
        }
        if worker.input_failure().is_some() && worker.input_report().is_none() {
            worker.set_input_report(ReportOutcome::Unavailable { native_code: None });
        }
        for failure in worker.video_failures_after(0) {
            // A failed or prematurely closed report worker cannot claim a path.
            worker.set_video_report(failure.monitor, ReportOutcome::Unavailable { native_code: None });
        }
        drop(still_rows);
        drop(lanes);
        drop(png);
        let (png, still_error) = match still_owner.join() {
            Ok((counters, result)) => (Some(counters), result.err()),
            Err(_) => (None, Some(unavailable("The still PNG worker terminated unexpectedly"))),
        };
        let mut failure = startup_error.or(worker_error).or(still_error);
        let input = match input {
            Ok(Some((cutoff, input))) => {
                if cutoff != input.cutoff {
                    failure.get_or_insert_with(|| unavailable("The video and input producers closed at different boundaries"));
                }
                Some(input)
            }
            Ok(None) => {
                failure.get_or_insert_with(|| unavailable("The input journal owner did not start"));
                None
            }
            Err(error) => { failure.get_or_insert(error); None }
        };
        Ok(RecordedVideo { video: videos, png, input, failure, required_finalization_failure })
    })
}

struct TopologyStop<'a>(&'a AtomicBool);

impl Drop for TopologyStop<'_> {
    fn drop(&mut self) { self.0.store(true, Ordering::Release); }
}

fn capture_device(adapter: &IDXGIAdapter1) -> Result<ID3D11Device, ProbeError> {
    let mut device: Option<ID3D11Device> = None;
    // An explicit adapter requires DRIVER_TYPE_UNKNOWN.
    stage(unsafe {
        D3D11CreateDevice(
            adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None, D3D11_SDK_VERSION, Some(&mut device), None, None,
        )
    }, "Create the D3D11 video device")?;
    let device = device.ok_or_else(|| unavailable("D3D11 did not return a device"))?;
    let context = stage(unsafe { device.GetImmediateContext() }, "Get the D3D11 context")?;
    let multithread: ID3D11Multithread = stage(context.cast(), "Protect the shared D3D11 context")?;
    // The video encoder uses its own device. Protect contexts used by a native callback.
    // The return value reports the previous protection state.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    Ok(device)
}

fn clock_error(_: crate::protocol::ClockRangeError) -> ProbeError {
    unavailable("A timestamp is outside the supported recording time range")
}

pub(crate) fn displays() -> Result<Vec<crate::platform::Display>, ProbeError> {
    Ok(display::enumerate()?.into_iter().map(|display| {
        let (left, top, right, bottom) = display.bounds.edges();
        let label = format!("{} · {}×{}", display.name, i64::from(right) - i64::from(left),
            i64::from(bottom) - i64::from(top));
        crate::platform::Display { key: display.key, label }
    }).collect())
}

pub(crate) fn replace_manifest(
    pending: &std::path::Path, manifest: &std::path::Path,
    before_replace: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH},
        core::PCWSTR,
    };

    let pending: Vec<u16> = pending.as_os_str().encode_wide().chain([0]).collect();
    let manifest: Vec<u16> = manifest.as_os_str().encode_wide().chain([0]).collect();
    before_replace()?;
    unsafe {
        MoveFileExW(
            PCWSTR(pending.as_ptr()), PCWSTR(manifest.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }.map_err(|error| std::io::Error::from_raw_os_error(error.code().0 & 0xffff))
}
