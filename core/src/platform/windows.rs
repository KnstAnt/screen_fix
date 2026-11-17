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
        journal::{FinalRow, HealthRecord, JournalError, JournalIngress, JournalLimits, JournalOwner, JournalStep},
        outcome::HealthKind,
        metadata::{AttemptFaults, AttemptLayout, CaptureDisposition, ImageFile, InputBlind, MetadataOwner, MonitorCapture, MonitorVideo, PackageError, PngCounters, UndeliveredFrames},
    },
    protocol::StreamId,
    session::{
        DiskPreflight, SessionOutcome, SessionResult, StartupStep, fault::FaultReport,
        windows::{Admission, InputFailure, MAX_INPUT_FAILURES, VideoFailure, WorkerSession, disk},
    },
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
    /// The owner that first attributed the error at its boundary.
    owner: Option<FailureComponent>,
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

impl ProbeError {
    /// Attribute the error to the owner whose boundary it crosses first. A
    /// deeper owner's attribution is kept.
    pub(crate) fn owned_by(mut self, owner: FailureComponent) -> Self {
        self.owner.get_or_insert(owner);
        self
    }

    /// The error of the owner at this boundary, unless a deeper owner attributed it.
    pub(crate) fn to_error(&self, owner: FailureComponent) -> CaptureError {
        if let Some(journal) = &self.journal {
            return CaptureError::journal(self.stage, journal);
        }
        let code = self.source.as_ref().map(|source| source.code().0);
        CaptureError::owned(self.owner.unwrap_or(owner), self.stage, code, self.to_string())
            .with_raw_input_registration(self.raw_input_registration)
    }

    pub(crate) fn into_error(self, owner: FailureComponent) -> CaptureError {
        self.to_error(owner)
    }
}

fn stage<T>(result: core::Result<T>, name: &'static str) -> Result<T, ProbeError> {
    result.map_err(|source| ProbeError { stage: name, source: Some(source), journal: None, raw_input_registration: None, owner: None })
}

pub(crate) fn unavailable(stage: &'static str) -> ProbeError {
    ProbeError { stage, source: None, journal: None, raw_input_registration: None, owner: None }
}

/// An I/O failure at `stage`. Only an operating-system error carries a native
/// code; any other I/O error has no source and reports the stage alone.
fn io_error(stage: &'static str, error: std::io::Error) -> ProbeError {
    let source = error.raw_os_error().map(|code| core::Error::new(core::HRESULT::from_win32(code as u32), error.to_string()));
    ProbeError { stage, source, journal: None, raw_input_registration: None, owner: None }
}

/// A NUL-terminated UTF-16 copy of `value` for a wide Win32 parameter.
pub(crate) fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_ref().encode_wide().chain([0]).collect()
}

/// Write-through `MoveFileExW`. Without `replace` an existing destination
/// fails the move, so a same-directory move publishes a new file only once.
pub(crate) fn move_file(source: &std::path::Path, destination: &std::path::Path, replace: bool) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let flags = if replace { MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH } else { MOVEFILE_WRITE_THROUGH };
    let (source, destination) = (wide(source), wide(destination));
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
            let (ingress, owner) = spawn_journal(worker, id, layout.path(ArtifactKind::Journal(id)), JournalLimits::for_stream(id))?;
            owners.push((id, owner));
            Ok::<_, CaptureError>(ingress)
        };
        let input = stream(StreamId::Input)?;
        let context = stream(StreamId::Context)?;
        let frames = stream(StreamId::Frames)?;
        let stills = stream(StreamId::Stills)?;
        let frames = video::Frames::spawn(worker, frames.into_verified())
            .map_err(|error| error.into_error(FailureComponent::Journal))?;
        record(worker, &layout, input, context, frames, stills, &mut monitor_capture, &mut monitor_epochs)
            .map_err(|error| error.into_error(FailureComponent::Controller))
    })();

    // Every attempt failure found here joins the controller's fault log, which
    // retains the first cause (§6.2 rule 2) and decides the attempt outcome.
    let report = |error: &CaptureError| worker.report_fault(FaultReport::attempt(error));
    let (video, png, images, mut producer_prefix, required_finalization_failure, undelivered_frames) = match output {
        Ok(output) => {
            if let Some(error) = output.failure {
                report(&error.into_error(FailureComponent::Controller));
            }
            (output.video, output.png, output.images, output.input, output.required_finalization_failure, output.undelivered_frames)
        }
        Err(error) => {
            report(&error);
            (Vec::new(), None, None, None, None, Vec::new())
        }
    };
    // An input activity loss: its health row joins the final context rows and
    // its blind interval the final input rows (INP-13).
    let mut input_blinds = Vec::new();
    let mut input_health = Vec::new();
    for input in worker.input_failures() {
        worker.update_standard_evidence(|evidence| { evidence.blind_reason.get_or_insert(input.reason); });
        let end_t_us = producer_prefix.as_ref().and_then(|prefix| prefix.cutoff)
            .map(|boundary| boundary.time).or_else(|| worker.session_time()).unwrap_or(input.at);
        let blind = InputBlind { start_t_us: input.first_missing.min(end_t_us), end_t_us, reason: input.reason };
        input_health.push(FinalRow::Health(HealthRecord {
            kind: HealthKind::InputFailure, failure: input.failure, detected: input.at,
            first_affected: Some(blind.start_t_us), ongoing: true, monitor: None, count: None,
        }));
        input_blinds.push(FinalRow::Blind(blind));
        if let Some(capture) = producer_prefix.as_mut().and_then(|prefix| prefix.capture.as_mut()) {
            capture.blind.get_or_insert(blind);
        }
    }
    let mut streams = BTreeMap::new();
    let mut context_owner = None;
    for (stream, owner) in owners {
        if stream == StreamId::Context {
            context_owner = Some(owner);
            continue;
        }
        let finished = if stream == StreamId::Input { owner.finish_with(std::mem::take(&mut input_blinds)) } else { owner.finish() };
        match finished {
            Ok(summary) => { streams.insert(stream, summary); }
            Err(error) => report(&package_journal_error(error).into_error(FailureComponent::Journal)),
        }
    }
    // Capture health reported after the input loop's last hand-off, for example
    // a still write failure while the PNG worker drains at Stop, is written with
    // the final context rows (§6.1). Without a context journal it is counted.
    let late_health = producer_prefix.as_ref().map_or_else(Vec::new, |prefix| worker.health_after(prefix.health_seen));
    let mut omitted_health = worker.omitted_health();
    if context_owner.is_none() {
        omitted_health = omitted_health.saturating_add(late_health.len() as u64);
    }
    if let Some(owner) = context_owner {
        // A failure detected before the session clock started has no receipt
        // on the common timeline. It is reported only through diagnostics.
        let mut health: Vec<_> = worker.video_failures_after(0).into_iter().filter(|entry| {
            producer_prefix.as_ref().is_none_or(|prefix| !prefix.video_health.contains(&entry.monitor))
        }).filter_map(|entry| entry.at.map(|at| FinalRow::Health(video_failure_health(entry.failure, entry.monitor, at))))
            .collect();
        health.extend(late_health.into_iter().map(FinalRow::Health));
        if let Some(error) = worker.fault_outcome().error && let Some(at) = worker.session_time() {
            health.push(FinalRow::Health(HealthRecord {
                kind: HealthKind::AttemptFailure, failure: error.failure(), detected: at, first_affected: None,
                ongoing: false, monitor: None, count: None,
            }));
        }
        health.extend(input_health);
        match owner.finish_with(health) {
            Ok(summary) => { streams.insert(StreamId::Context, summary); }
            Err(error) => report(&package_journal_error(error).into_error(FailureComponent::Journal)),
        }
    }
    diagnostics.input_registration = worker.input_registration();
    metadata.set_input_capture(
        producer_prefix.as_mut().and_then(|prefix| prefix.capture.take()), diagnostics.input_registration,
    );
    metadata.set_image_inventory(images, producer_prefix.as_mut().and_then(|prefix| prefix.cursors.take()));
    if !worker.attempt_failed() && let Some(prefix) = producer_prefix {
        let matches_prefix = |id, prefix: &input::ReceiptPrefix| streams.get(&id).is_some_and(|summary| {
            summary.data_rows == prefix.queued && summary.last_data_receipt == prefix.last_queued
        });
        let frame_count = video.iter().map(|monitor| monitor.slots).sum::<u64>();
        let frames_match = streams.get(&StreamId::Frames).is_some_and(|summary| summary.data_rows == frame_count);
        if !matches_prefix(StreamId::Input, &prefix.input)
            || !matches_prefix(StreamId::Context, &prefix.context)
            || !frames_match
        {
            report(&unavailable("The written journals do not match the admitted producer prefixes").into_error(FailureComponent::Journal));
        } else if let Some(cutoff) = prefix.cutoff
            && let Err(error) = worker.complete_prefix(cutoff)
        {
            report(&error.into_error(FailureComponent::Controller));
        }
    }
    drop(disk);
    if let Some(operation) = required_finalization_failure {
        report(&unavailable(operation).into_error(FailureComponent::Video));
    }
    let faults = worker.fault_outcome();
    diagnostics.additional_failures.extend(faults.additional);
    diagnostics.omitted_fault_reports = faults.omitted;
    let failure = faults.error;
    let failed_step = failure.as_ref().map(|error| error.operation);
    let frame_count = video.iter().map(|monitor| monitor.encoded).sum::<u64>();
    let unavailable_video = config.monitors().iter().filter(|selection| {
        !video.iter().any(|monitor| monitor.monitor == selection.id && monitor.usable())
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
        AttemptFaults { failed_step, activity_losses: faults.activity_losses, omitted_health, undelivered_frames }, streams, video,
        || worker.stop_deadline(), || worker.authorize_complete_publication(),
    ).map_err(package_error);
    if let Some(mut error) = failure {
        let saved = matches!(&publication, Ok(CaptureDisposition::Failed));
        diagnostics.failed_manifest_published = Some(saved);
        if let Err(publication_error) = publication {
            diagnostics.additional_failures.push(publication_error.failure());
        }
        // Any admitted attempt, standard or elevated, reports its package as
        // an admitted failure rather than as a start error (M1-01).
        if worker.started() || error.source_failure().is_some_and(|source| source.admitted) {
            error = error.with_retained_recording(layout.root().to_owned(), frame_count, saved);
        }
        return Err(error.with_context(diagnostics));
    }
    let outcome = match publication {
        Ok(outcome) => outcome,
        Err(error) => {
            report(&error);
            return Err(error.with_context(diagnostics));
        }
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
        CaptureDisposition::Recorded => {
            // Unsaved stills are recorded losses of a complete recording (C6).
            let unsaved = diagnostics.streams.get(&StreamId::Stills).map_or(0, |stills| {
                stills.unavailable_by_reason.values().fold(stills.dropped, |total, count| total.saturating_add(*count))
            });
            if unsaved == 0 {
                format!("Recording complete. Frames: {frame_count}.\n{root}")
            } else {
                format!("Recording complete with recorded losses ({unsaved} stills not saved). Frames: {frame_count}.\n{root}")
            }
        }
        CaptureDisposition::Pending | CaptureDisposition::Failed =>
            return Err(unavailable("The recording manifest has an unexpected final disposition").into_error(FailureComponent::PackageMetadata)),
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

/// One monitor's video can no longer be produced for the rest of the attempt.
pub(super) fn video_failure_health(failure: crate::package::diagnostics::Failure, monitor: crate::protocol::MonitorId, at: crate::protocol::SessionTimeUs) -> HealthRecord {
    HealthRecord {
        kind: HealthKind::VideoFailure, failure, detected: at, first_affected: Some(at), ongoing: true,
        monitor: Some(monitor), count: None,
    }
}

fn spawn_journal(
    worker: &WorkerSession, stream: StreamId, path: std::path::PathBuf, limits: JournalLimits,
) -> Result<(JournalIngress, JournalOwner), CaptureError> {
    let worker = worker.clone();
    JournalOwner::spawn(stream, path, limits, move |error| {
        worker.report_fault(FaultReport::attempt(&CaptureError::journal(journal_operation(error.step), error)));
    }).map_err(|error| package_journal_error(error).into_error(FailureComponent::Journal))
}

fn package_error(error: PackageError) -> CaptureError {
    let (operation, code) = match &error {
        PackageError::Io { operation, source } => (operation.message(), source.raw_os_error()),
        PackageError::InconsistentInventory => ("Reconcile the closed recording inventory", None),
        _ => ("Read the recording package", None),
    };
    CaptureError::classified(CaptureErrorKind::PackageFailure, FailureComponent::PackageMetadata, operation, code, error.to_string())
}

fn journal_operation(step: JournalStep) -> &'static str {
    match step {
        JournalStep::Start => "Start the journal owner",
        JournalStep::Create => "Create the journal file",
        JournalStep::Order => "Order the journal stream",
        JournalStep::Encode => "Encode the journal row",
        JournalStep::Append => "Append the journal row",
        JournalStep::BufferedFlush => "Flush the journal buffer",
        JournalStep::OsFlush => "Flush the journal to disk",
        JournalStep::Close => "Close the journal",
    }
}

pub(super) fn package_journal_error(error: JournalError) -> ProbeError {
    ProbeError {
        stage: journal_operation(error.step), source: None, journal: Some(error), raw_input_registration: None,
        owner: Some(FailureComponent::Journal),
    }
}

struct RecordedVideo {
    video: Vec<MonitorVideo>,
    png: Option<PngCounters>,
    images: Option<Vec<ImageFile>>,
    input: Option<input::InputPrefix>,
    failure: Option<ProbeError>,
    required_finalization_failure: Option<&'static str>,
    /// Frames rows of monitors whose worker was blocked in a native call (§6.2 row 21).
    undelivered_frames: Vec<UndeliveredFrames>,
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
        video: Vec::new(), png: None, images: Some(Vec::new()), input: Some(input::InputPrefix::cancelled()), failure: None,
        required_finalization_failure: None, undelivered_frames: Vec::new(),
    });
    if worker.stop_requested() {
        return cancelled();
    }
    worker.startup_step(StartupStep::MediaFoundation);
    let _apartment = Apartment::initialize().map_err(|error| error.owned_by(FailureComponent::Video))?;
    let _media_foundation = mf::Runtime::initialize().map_err(|error| error.owned_by(FailureComponent::Video))?;
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
    let recorded = thread::scope(|scope| {
        topology.spawn(scope, worker, topology_stop).map_err(|error| error.owned_by(FailureComponent::Capture))?;
        let _stop_topology = TopologyStop(topology_stop);
        let (png, mut lanes, still_worker) = still_png::Worker::new(worker.config().monitors().len() + 1);
        let still_rows = lanes.pop().expect("the input still producer has a lane");
        let mut lanes = lanes.into_iter();
        let still_owner = thread::Builder::new().name("native-png".into())
            .spawn_scoped(scope, move || still_worker.run(layout, stills, worker))
            .map_err(|error| io_error("Start the native PNG worker", error).owned_by(FailureComponent::Stills))?;
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
                    startup_error = Some(io_error("Start the monitor video owner", error).owned_by(FailureComponent::Video));
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
                        startup_error = Some(unavailable("A selected monitor did not pass capture and encoder preflight")
                            .owned_by(FailureComponent::Video));
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
                let controller = |error: ProbeError| error.owned_by(FailureComponent::Controller);
                if let Admission::Closed(cutoff) = worker.admit().map_err(controller)? {
                    return Ok(cutoff);
                }
                worker.ready().map_err(controller)?;
                worker.verify_startup(|| {
                    let config = worker.config();
                    worker.window_coverage(preflight::window_coverage(config));
                    preflight::validate_processes(config)?;
                    preflight::validate_foreground(config)
                });
                worker.wait_for_stop().map_err(controller)
            }).map(Some).map_err(|error| error.owned_by(FailureComponent::Receiver))
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
                                worker_error.get_or_insert_with(|| unavailable("A monitor closed at the wrong admission boundary")
                                    .owned_by(FailureComponent::Controller));
                            }
                        }
                        Err(error) => { worker_error.get_or_insert(error.owned_by(FailureComponent::Video)); }
                    }
                    required_finalization_failure = required_finalization_failure.or(result.required_finalization_failure);
                    videos.push(result.video);
                }
                Ok(Err(error)) => { worker_error.get_or_insert(error.owned_by(FailureComponent::Video)); }
                Err(_) => {
                    worker_error.get_or_insert_with(|| unavailable("A monitor video owner terminated unexpectedly")
                        .owned_by(FailureComponent::Video));
                }
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
        // The worker returns an error or exits only on a still journal failure
        // (§6.2 row 1, F). An encoder exit is reported by the worker itself as
        // an auxiliary activity loss (row 16) and does not end the worker.
        let (png, images, still_error) = match still_owner.join() {
            Ok((counters, images, result)) => (Some(counters), Some(images), result.err().map(|error| error.owned_by(FailureComponent::Stills))),
            Err(_) => (None, None, Some(unavailable("The still journal worker terminated unexpectedly").owned_by(FailureComponent::Stills))),
        };
        let mut failure = startup_error.or(worker_error).or(still_error);
        let input = match input {
            Ok(Some((cutoff, input))) => {
                if cutoff != input.cutoff {
                    failure.get_or_insert_with(|| unavailable("The video and input producers closed at different boundaries")
                        .owned_by(FailureComponent::Controller));
                }
                Some(input)
            }
            Ok(None) => {
                failure.get_or_insert_with(|| unavailable("The input journal owner did not start").owned_by(FailureComponent::Receiver));
                None
            }
            Err(error) => { failure.get_or_insert(error); None }
        };
        Ok(RecordedVideo { video: videos, png, images, input, failure, required_finalization_failure, undelivered_frames: Vec::new() })
    });
    // Every video worker has exited: the merge owner releases the remaining
    // lane rows and closes the frames ingress.
    let undelivered = frames.finish();
    recorded.map(|mut recorded| {
        match undelivered {
            Ok(undelivered) => recorded.undelivered_frames = undelivered,
            Err(error) => { recorded.failure.get_or_insert(error.owned_by(FailureComponent::Journal)); }
        }
        recorded
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
        let label = format!("{} · {}×{}", display.name, display.bounds.width(), display.bounds.height());
        crate::platform::Display { key: display.key, label }
    }).collect())
}

pub(crate) fn replace_manifest(
    pending: &std::path::Path, manifest: &std::path::Path,
    before_replace: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    before_replace()?;
    move_file(pending, manifest, true)
}
