use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use windows::{Win32::Media::MediaFoundation::*, core::PCWSTR};

use super::{ProbeError, stage, unavailable};

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct FrameTiming {
    pub(super) wgc_time_100ns: i64,
    pub(super) repeated: bool,
    pub(super) observation_id: u64,
    pub(super) fingerprint: [u8; 32],
    pub(super) admitted_100ns: i64,
    pub(super) pts_100ns: i64,
    pub(super) duration_100ns: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Timeline {
    pub(super) qpc_at_start: i64,
    pub(super) qpc_frequency: i64,
    pub(super) qpc_uncertainty_100ns: i64,
    pub(super) video_origin_100ns: i64,
    pub(super) stop_100ns: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Geometry {
    pub(super) display_id: String,
    pub(super) desktop_rect: [i32; 4],
    pub(super) source_size: [i32; 2],
    pub(super) video_size: [u32; 2],
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Manifest {
    pub(super) version: u32,
    frame_count: usize,
    video: String,
    frames: String,
    events: String,
    event_count: u64,
    pub(super) timeline: Timeline,
    #[serde(default)]
    pub(super) geometry: Option<Geometry>,
}

pub(super) struct FrameIndex {
    folder: PathBuf,
    journal: BufWriter<fs::File>,
    timeline: Option<Timeline>,
    geometry: Geometry,
    event_count: u64,
}

impl FrameIndex {
    pub(super) fn create(parent: &Path, geometry: Geometry) -> Result<Self, ProbeError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| unavailable("The system clock could not supply an MP4 directory timestamp"))?;
        let folder = parent.join(format!(
            "screen-fix-video-{}-{}", std::process::id(), timestamp.as_nanos()
        ));
        fs::create_dir(&folder).map_err(|error| ProbeError {
            stage: "Create the MP4 output directory",
            source: Some(windows::core::Error::new(windows::core::HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32), error.to_string())),
        })?;
        let journal = fs::File::create(folder.join("submitted.jsonl"))
            .map_err(|error| io_error("Create the provisional frame index", error))?;
        Ok(Self { folder, journal: BufWriter::new(journal), timeline: None, event_count: 0, geometry })
    }

    pub(super) fn folder(&self) -> &Path { &self.folder }

    pub(super) fn set_event_count(&mut self, count: u64) { self.event_count = count; }

    pub(super) fn set_timeline(&mut self, timeline: Timeline) {
        self.timeline = Some(timeline);
    }

    pub(super) fn push(&mut self, timing: &FrameTiming) -> Result<(), ProbeError> {
        json_line(&mut self.journal, timing)
    }
}

pub(super) fn io_error(stage: &'static str, error: std::io::Error) -> ProbeError {
    ProbeError {
        stage,
        source: Some(windows::core::Error::new(
            windows::core::HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32),
            error.to_string(),
        )),
    }
}

fn json_line(writer: &mut impl Write, timing: &FrameTiming) -> Result<(), ProbeError> {
    serde_json::to_writer(&mut *writer, timing)
        .map_err(|error| io_error("Write the frame index", error.into()))?;
    writer.write_all(b"\n").map_err(|error| io_error("Write the frame index", error))
}

pub(super) struct Recording {
    writer: IMFSinkWriter,
    stream: u32,
    pending: PathBuf,
    path: PathBuf,
}

impl Recording {
    pub(super) fn open(media_type: &IMFMediaType, folder: &Path) -> Result<Self, ProbeError> {
        let pending = folder.join("pending.mp4");
        let path = folder.join("video.mp4");
        let filename: Vec<u16> = pending.as_os_str().encode_wide().chain([0]).collect();
        // The MP4 extension selects the container. Identical compressed media types
        // preserve the existing bitstream.
        let writer = stage(
            unsafe { MFCreateSinkWriterFromURL(PCWSTR(filename.as_ptr()), None, None) },
            "Open the MP4 writer",
        )?;
        let stream = stage(unsafe { writer.AddStream(media_type) }, "Add an H.264 stream to the MP4")?;
        stage(
            unsafe { writer.SetInputMediaType(stream, media_type, None) },
            "Negotiate H.264 for the MP4",
        )?;
        stage(unsafe { writer.BeginWriting() }, "Begin writing the MP4")?;
        Ok(Self { writer, stream, pending, path })
    }

    pub(super) fn write(&self, sample: &IMFSample) -> Result<(), ProbeError> {
        stage(unsafe { self.writer.WriteSample(self.stream, sample) }, "Write an H.264 sample to the MP4")
    }

    pub(super) fn finish(self, count: usize, mut index: FrameIndex) -> Result<PathBuf, ProbeError> {
        stage(unsafe { self.writer.Finalize() }, "Finalize the MP4")?;
        drop(self.writer);
        index.journal.flush().map_err(|error| io_error("Close the provisional frame index", error))?;
        drop(index.journal);
        let submitted = index.folder.join("submitted.jsonl");
        let verified = index.folder.join("frames.jsonl");
        verify_decoded_frames(&self.pending, count, &submitted, &verified)?;
        fs::remove_file(submitted).map_err(|error| io_error("Remove the provisional frame index", error))?;
        fs::rename(&self.pending, &self.path).map_err(|error| ProbeError {
            stage: "Publish the MP4",
            source: Some(windows::core::Error::new(windows::core::HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32), error.to_string())),
        })?;
        let timeline = index.timeline.ok_or_else(|| unavailable("The session timeline is missing"))?;
        let manifest = Manifest {
            version: 3, frame_count: count, video: "video.mp4".into(), frames: "frames.jsonl".into(),
            events: "events.jsonl".into(), event_count: index.event_count, timeline, geometry: Some(index.geometry),
        };
        let pending_manifest = index.folder.join("manifest.pending.json");
        let file = fs::File::create(&pending_manifest)
            .map_err(|error| io_error("Create the recording manifest", error))?;
        let mut file = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut file, &manifest)
            .map_err(|error| io_error("Write the manifest", error.into()))?;
        file.flush().map_err(|error| io_error("Close the manifest", error))?;
        drop(file);
        fs::rename(&pending_manifest, index.folder.join("manifest.json"))
            .map_err(|error| io_error("Publish the manifest", error))?;
        Ok(self.path)
    }
}

fn verify_decoded_frames(
    path: &Path, expected_count: usize, submitted: &Path, verified: &Path,
) -> Result<(), ProbeError> {
    let mut expected = BufReader::new(fs::File::open(submitted)
        .map_err(|error| io_error("Open the provisional frame index", error))?).lines();
    let mut verified = BufWriter::new(fs::File::create(verified)
        .map_err(|error| io_error("Create the verified frame index", error))?);
    let filename: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let reader = stage(
        unsafe { MFCreateSourceReaderFromURL(PCWSTR(filename.as_ptr()), None) },
        "Open the finalized MP4 for verification",
    )?;
    let output = stage(unsafe { MFCreateMediaType() }, "Create the decoding media type")?;
    stage(unsafe { output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }, "Select video for verification")?;
    stage(unsafe { output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12) }, "Select NV12 for verification")?;
    // This probe writes exactly one video stream.
    stage(unsafe { reader.SetCurrentMediaType(0, None, &output) }, "Configure MP4 decoding")?;
    let mut decoded = 0;
    let mut previous_pts = None;
    loop {
        let mut timestamp = 0;
        let mut flags = 0;
        let mut sample = None;
        stage(
            unsafe { reader.ReadSample(0, 0, None, Some(&mut flags), Some(&mut timestamp), Some(&mut sample)) },
            "Read a decoded MP4 frame",
        )?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(unavailable("The decoder reported an error reading the MP4"));
        }
        if let Some(sample) = sample {
            let length = stage(unsafe { sample.GetTotalLength() }, "Check the decoded frame size")?;
            if length != 0 {
                if decoded >= expected_count || previous_pts.is_some_and(|previous| timestamp <= previous) {
                    return Err(unavailable("Decoded MP4 timestamps are not strictly increasing"));
                }
                let line = expected.next().ok_or_else(|| unavailable("An MP4 frame is missing from the index"))?
                    .map_err(|error| io_error("Read the provisional frame index", error))?;
                let mut timing: FrameTiming = serde_json::from_str(&line)
                    .map_err(|error| io_error("Parse the provisional frame index", error.into()))?;
                let duration = stage(unsafe { sample.GetSampleDuration() }, "Read the MP4 frame duration")?;
                if duration <= 0 || timestamp.abs_diff(timing.pts_100ns) > 10_000
                    || duration.abs_diff(timing.duration_100ns) > 10_000
                    || timestamp.saturating_add(duration).abs_diff(timing.pts_100ns.saturating_add(timing.duration_100ns)) > 10_000
                {
                    return Err(unavailable("The MP4 frame timestamp or duration does not match the index"));
                }
                timing.pts_100ns = timestamp;
                timing.duration_100ns = duration;
                json_line(&mut verified, &timing)?;
                previous_pts = Some(timestamp);
                decoded += 1;
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return if decoded == expected_count && expected.next().is_none() {
                verified.flush().map_err(|error| io_error("Close the verified frame index", error))
            } else {
                Err(unavailable("The number of decoded MP4 frames does not match the number submitted"))
            };
        }
    }
}
