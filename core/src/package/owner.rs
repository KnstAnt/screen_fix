//! The metadata owner is the only live publisher of an attempt manifest.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    config::ResolvedCaptureConfig,
    protocol::{AttemptId, StreamId},
};

use super::{
    ArtifactKind, ArtifactRef, RAW_CURSORS, RAW_IMAGES, RAW_VIDEO,
    error::{PackageError, PackageOperation, PackageResult},
    journal::StreamSummary,
    manifest::{
        ActivityLoss, AttemptClock, AttemptManifest, CaptureDisposition, HealthSummary, ImageFile, InputCapture,
        MANIFEST_VERSION, MAX_MANIFEST_BYTES, MonitorCapture, MonitorVideo, PackageState, PngCounters, SourceEvidence,
        UndeliveredFrames,
    },
};

#[derive(Debug, Clone)]
pub(crate) struct AttemptLayout {
    id: AttemptId,
    root: PathBuf,
}

impl AttemptLayout {
    pub(super) fn new(id: AttemptId, root: PathBuf) -> Self {
        Self { id, root }
    }

    pub(crate) fn id(&self) -> &AttemptId {
        &self.id
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, kind: ArtifactKind) -> PathBuf {
        self.root.join(ArtifactRef { attempt: self.id.clone(), kind }.relative_uri())
    }
}

/// The controller's fault outcome for one attempt.
pub(crate) struct AttemptFaults {
    /// The operation of the first attempt failure.
    pub(crate) failed_step: Option<&'static str>,
    pub(crate) activity_losses: Vec<ActivityLoss>,
    pub(crate) omitted_health: u64,
    pub(crate) undelivered_frames: Vec<UndeliveredFrames>,
}

pub(crate) struct MetadataOwner {
    layout: AttemptLayout,
    manifest: AttemptManifest,
}

impl MetadataOwner {
    pub(crate) fn create(
        config: &ResolvedCaptureConfig, disk: super::source::DiskPreflight, os: crate::platform::WindowsEvidence,
    ) -> PackageResult<Self> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let attempts = config.output_root().join("attempts");
        fs::create_dir_all(&attempts).map_err(|e| PackageError::io(PackageOperation::CreateAttempt, e))?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let mut layout = None;
        for suffix in 0..32 {
            let candidate = format!("{stamp:x}-{:x}-{suffix:x}", std::process::id());
            let id = AttemptId::new(&candidate).map_err(|error| {
                PackageError::io(PackageOperation::CreateAttempt, io::Error::new(io::ErrorKind::InvalidInput, error))
            })?;
            let root = attempts.join(format!("attempt_{}", id.as_str()));
            match fs::create_dir(&root) {
                Ok(()) => {
                    // Development-only and not inventoried (`dev-log` feature).
                    crate::devlog::open(&root);
                    layout = Some(AttemptLayout { id, root });
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(PackageError::io(PackageOperation::CreateAttempt, e)),
            }
        }
        let layout = layout.ok_or_else(|| PackageError::io(
            PackageOperation::CreateAttempt, io::Error::new(io::ErrorKind::AlreadyExists, "Attempt IDs are already in use"),
        ))?;
        let owner = Self {
            manifest: AttemptManifest {
                version: MANIFEST_VERSION, attempt_id: layout.id.clone(),
                state: PackageState::Incomplete, capture: CaptureDisposition::Pending,
                selected_displays: config.monitors().iter().map(|selected| selected.id).collect(),
                profile: Some(config.into()), clock: None, os: Some(os), monitor_capture: Vec::new(), monitor_epochs: Vec::new(), failed_step: None,
                streams: Default::default(), health: None, monitor_video: Vec::new(), input_capture: None, input_registration: None, source_evidence: None, png: None, images: None, cursors: None,
                disk_preflight: Some(disk),
            },
            layout,
        };
        owner.publish(|| Ok(()))?;
        if let Err(error) = owner.create_layout() {
            let _ = owner.finish(Some(PackageOperation::CreateLayout.message()), Default::default(), Vec::new());
            return Err(error);
        }
        Ok(owner)
    }

    fn create_layout(&self) -> PackageResult<()> {
        for directory in [RAW_VIDEO, RAW_IMAGES, RAW_CURSORS] {
            fs::create_dir_all(self.layout.root.join(directory))
                .map_err(|e| PackageError::io(PackageOperation::CreateLayout, e))?;
        }
        for id in &self.manifest.selected_displays {
            fs::create_dir(self.layout.root.join(ArtifactKind::video_dir(*id)))
                .map_err(|e| PackageError::io(PackageOperation::CreateLayout, e))?;
        }
        Ok(())
    }

    pub(crate) fn layout(&self) -> &AttemptLayout {
        &self.layout
    }

    pub(crate) fn set_clock(&mut self, clock: Option<AttemptClock>) {
        self.manifest.clock = clock;
    }

    pub(crate) fn set_monitor_capture(&mut self, monitors: Vec<MonitorCapture>, epochs: Vec<MonitorCapture>) {
        self.manifest.monitor_capture = monitors;
        self.manifest.monitor_epochs = epochs;
    }

    pub(crate) fn set_png(&mut self, png: Option<PngCounters>) {
        self.manifest.png = png;
    }

    /// The files reported by the PNG and cursor asset owners.
    pub(crate) fn set_image_inventory(&mut self, images: Option<Vec<ImageFile>>, cursors: Option<Vec<ImageFile>>) {
        let sorted = |mut files: Vec<ImageFile>| {
            files.sort_by(|left, right| left.file.cmp(&right.file));
            files.dedup_by(|left, right| left.file == right.file);
            files
        };
        self.manifest.images = images.map(sorted);
        self.manifest.cursors = cursors.map(sorted);
    }

    pub(crate) fn set_source_evidence(&mut self, evidence: Option<SourceEvidence>) {
        self.manifest.source_evidence = evidence.map(|mut evidence| {
            evidence.failure = evidence.failure.map(super::source::SourceFailure::without_report_detail);
            evidence.cleanup_failure = evidence.cleanup_failure.map(super::source::SourceFailure::without_report_detail);
            evidence
        });
    }

    pub(crate) fn set_input_capture(
        &mut self, input: Option<InputCapture>, registration: Option<crate::protocol::ipc::InputRegistration>,
    ) {
        self.manifest.input_capture = input;
        self.manifest.input_registration = registration;
    }

    pub(crate) fn finish(
        self,
        failed_step: Option<&'static str>,
        streams: BTreeMap<StreamId, StreamSummary>,
        video: Vec<MonitorVideo>,
    ) -> PackageResult<()> {
        let faults = AttemptFaults { failed_step, activity_losses: Vec::new(), omitted_health: 0, undelivered_frames: Vec::new() };
        self.finish_recording(faults, streams, video, || None, || {
            Err(io::Error::other("Complete publication requires authorization"))
        }).map(|_| ())
    }

    /// The disposition follows §6.2 rule 4: any attempt failure, then a
    /// required video loss or the selected-monitor rule, then a required input
    /// loss or an input-certification shortfall. Auxiliary activity losses and
    /// recorded losses never change it; they appear only in `health`.
    pub(crate) fn finish_recording(
        mut self,
        faults: AttemptFaults,
        streams: BTreeMap<StreamId, StreamSummary>,
        video: Vec<MonitorVideo>,
        deadline: impl Fn() -> Option<Instant>,
        authorize_complete: impl FnOnce() -> io::Result<()>,
    ) -> PackageResult<CaptureDisposition> {
        use super::diagnostics::FailureComponent;

        let AttemptFaults { failed_step, activity_losses, omitted_health, undelivered_frames } = faults;
        let video_loss = activity_losses.iter().any(|loss| loss.required(FailureComponent::Video));
        let input_loss = activity_losses.iter().any(|loss| loss.required(FailureComponent::InputHelper));
        self.manifest.health = Some(HealthSummary::new(&streams, activity_losses, omitted_health, undelivered_frames));
        self.manifest.streams = streams;
        let any_video = video.iter().any(|monitor| monitor.encoded != 0);
        // D-6 A: a selected monitor needs video in each interval in which it
        // was present and admitted; its absence does not affect Complete.
        let video_unavailable = video_loss
            || (any_video && self.manifest.selected_displays.iter().any(|id| {
                !video.iter().any(|monitor| monitor.monitor == *id && monitor.usable())
            }));
        let input_shortfall = self.manifest.clock.is_some() && self.manifest.input_capture.as_ref()
            .is_none_or(|input| input.uncertified_closing_tail.is_some());
        let input_unavailable = input_loss || input_shortfall;
        let approval_suppressed = video.iter().any(|monitor| monitor.slots != 0)
            && video.iter().all(|monitor| monitor.approval_suppressed == monitor.slots);
        self.manifest.monitor_video = video;
        self.manifest.capture = if failed_step.is_some() {
            CaptureDisposition::Failed
        } else if video_unavailable {
            CaptureDisposition::VideoUnavailable
        } else if input_unavailable {
            CaptureDisposition::InputUnavailable
        } else if any_video {
            CaptureDisposition::Recorded
        } else if approval_suppressed {
            CaptureDisposition::ApprovalSuppressed
        } else {
            CaptureDisposition::StoppedBeforeFirstFrame
        };
        let validation = if self.manifest.capture == CaptureDisposition::Recorded {
            self.manifest.validate_recorded_inventory()
        } else {
            Ok(())
        };
        self.manifest.failed_step = failed_step.map(str::to_owned);
        if validation.is_err() {
            self.manifest.capture = CaptureDisposition::Failed;
            self.manifest.failed_step = Some("Reconcile the closed recording inventory".into());
        }
        if self.manifest.capture == CaptureDisposition::Recorded {
            self.manifest.state = PackageState::Complete;
        }
        if self.manifest.state == PackageState::Complete {
            self.publish(authorize_complete)?;
        } else {
            let check_deadline = || {
                if deadline().is_some_and(|limit| Instant::now() >= limit) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "The recording deadline expired"));
                }
                Ok(())
            };
            check_deadline().map_err(|error| PackageError::io(PackageOperation::PublishManifest, error))?;
            // A successful replacement is final (C6): no deadline is checked after it.
            self.publish(&check_deadline)?;
        }
        crate::devlog::devlog!("package", "manifest published state={:?} capture={:?} failed_step={:?} validation_ok={}",
            self.manifest.state, self.manifest.capture, self.manifest.failed_step, validation.is_ok());
        validation?;
        Ok(self.manifest.capture)
    }

    fn publish(&self, before_replace: impl FnOnce() -> io::Result<()>) -> PackageResult<()> {
        let pending = self.layout.root.join("manifest.pending.json");
        self.write_manifest(&pending)?;
        let manifest = self.layout.path(ArtifactKind::Manifest);
        crate::platform::windows::replace_manifest(&pending, &manifest, before_replace)
            .map_err(|error| PackageError::io(PackageOperation::PublishManifest, error))
    }

    fn write_manifest(&self, path: &Path) -> PackageResult<()> {
        let mut file = File::create_new(path)
            .map_err(|e| PackageError::io(PackageOperation::CreateManifest, e))?;
        let encoded = serde_json::to_writer_pretty(&mut file, &self.manifest);
        encoded.map_err(|e| PackageError::io(PackageOperation::EncodeManifest, e.into()))?;
        file.write_all(b"\n").map_err(|e| PackageError::io(PackageOperation::EncodeManifest, e))?;
        if file.metadata().map_err(|e| PackageError::io(PackageOperation::EncodeManifest, e))?.len() > MAX_MANIFEST_BYTES {
            return Err(PackageError::io(PackageOperation::EncodeManifest,
                io::Error::new(io::ErrorKind::InvalidData, "The recording inventory exceeds the manifest size limit")));
        }
        file.sync_all().map_err(|e| PackageError::io(PackageOperation::FlushManifest, e))
    }

}
