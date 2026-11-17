use std::{
    cell::Cell,
    fs,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use windows::{Win32::Media::MediaFoundation::*, core::PCWSTR};

use super::{ProbeError, mf, stage, unavailable};

const REVOKED: &str = "The elevated video prefix was revoked before publication";

pub(super) fn io_error(stage: &'static str, error: std::io::Error) -> ProbeError {
    ProbeError {
        stage,
        journal: None,
        raw_input_registration: None,
        source: Some(windows::core::Error::new(
            windows::core::HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32),
            error.to_string(),
        )),
    }
}

pub(super) struct Recording {
    writer: IMFSinkWriter,
    stream: u32,
    pending: PathBuf,
    path: PathBuf,
}

impl Recording {
    pub(super) fn open(media_type: &IMFMediaType, path: &Path) -> Result<Self, ProbeError> {
        let pending = path.with_extension("mp4.part");
        let path = path.to_owned();
        let filename: Vec<u16> = pending.as_os_str().encode_wide().chain([0]).collect();
        // Container attributes select MP4 for the .part file. Identical compressed
        // media types preserve the existing bitstream.
        let settings = mf::attributes(3)?;
        stage(unsafe {
            settings.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)
        }, "Select the MP4 container")?;
        stage(unsafe {
            settings.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)
        }, "Disable MP4 sink media converters")?;
        stage(unsafe {
            settings.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)
        }, "Configure compressed H.264 MP4 passthrough")?;
        let writer = mf::sink_writer(PCWSTR(filename.as_ptr()), Some(&settings))?;
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

    pub(super) fn discard(self) -> Result<(), ProbeError> {
        drop(self.writer);
        remove_part(&self.pending)
    }

    pub(super) fn finish(
        self, expected: &mut [super::slots::Row], size: [u32; 2],
        deadline: impl Fn() -> Option<std::time::Instant>, authorized: impl Fn(Option<i64>) -> bool,
    ) -> Result<Option<(i64, u64)>, ProbeError> {
        let pending = self.pending.clone();
        match self.finish_verified(expected, size, deadline, &authorized) {
            Ok(published) => Ok(Some(published)),
            Err(error) if error.stage == REVOKED => { remove_part(&pending)?; Ok(None) }
            Err(error) => Err(error),
        }
    }

    fn finish_verified(
        self, expected: &mut [super::slots::Row], size: [u32; 2],
        deadline: impl Fn() -> Option<std::time::Instant>, authorized: &impl Fn(Option<i64>) -> bool,
    ) -> Result<(i64, u64), ProbeError> {
        let decoded_end = Cell::new(None);
        let check = || {
            if !authorized(decoded_end.get()) {
                return Err(unavailable(REVOKED));
            }
            if deadline().is_some_and(|limit| std::time::Instant::now() >= limit) {
                return Err(unavailable("The MP4 publication deadline expired"));
            }
            Ok(())
        };
        check()?;
        stage(unsafe { self.writer.Finalize() }, "Finalize the MP4")?;
        drop(self.writer);
        check()?;
        fs::OpenOptions::new().write(true).open(&self.pending).and_then(|file| file.sync_all())
            .map_err(|error| io_error("Flush the finalized MP4", error))?;
        check()?;
        let end = verify_decoded_frames(&self.pending, expected, size, &check)?;
        decoded_end.set(Some(end));
        let bytes = fs::metadata(&self.pending).map_err(|error| io_error("Inspect the verified MP4 extent", error))?.len();
        if bytes == 0 {
            return Err(unavailable("The verified MP4 file is empty"));
        }
        for retry in 0..6 {
            check()?;
            match fs::rename(&self.pending, &self.path) {
                Ok(()) => {
                    if let Err(error) = check() {
                        if error.stage == REVOKED {
                            remove_part(&self.path)?;
                        } else {
                            // This owner can still withdraw a late publication.
                            fs::rename(&self.path, &self.pending)
                                .map_err(|error| io_error("Return the late published MP4 to its part path", error))?;
                        }
                        return Err(error);
                    }
                    return Ok((end, bytes));
                }
                Err(error) if retry < 5 && matches!(error.raw_os_error(), Some(32 | 33)) => {
                    let delay = std::time::Duration::from_millis(25);
                    let delay = deadline().map_or(delay, |limit| delay.min(limit.saturating_duration_since(std::time::Instant::now())));
                    std::thread::sleep(delay);
                }
                Err(error) => return Err(io_error("Publish the MP4", error)),
            }
        }
        unreachable!()
    }
}

fn remove_part(path: &Path) -> Result<(), ProbeError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("Remove the revoked MP4 part", error)),
    }
}

fn verify_decoded_frames(
    path: &Path, expected: &mut [super::slots::Row], size: [u32; 2],
    check: &impl Fn() -> Result<(), ProbeError>,
) -> Result<i64, ProbeError> {
    let filename: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let reader = mf::mp4_part_reader(PCWSTR(filename.as_ptr()))?;
    let output = mf::media_type()?;
    stage(unsafe { output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }, "Select video for verification")?;
    stage(unsafe { output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12) }, "Select NV12 for verification")?;
    stage(unsafe { reader.SetCurrentMediaType(0, None, &output) }, "Configure MP4 decoding")?;
    let media_type = stage(unsafe { reader.GetCurrentMediaType(0) }, "Read the decoded segment format")?;
    let actual = stage(unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }, "Read the decoded segment dimensions")?;
    if actual != ((u64::from(size[0]) << 32) | u64::from(size[1])) {
        return Err(unavailable("The decoded segment dimensions do not match the manifest"));
    }
    let mut decoded = 0;
    let mut previous_pts: Option<i64> = None;
    let mut previous_end = 0;
    loop {
        check()?;
        let (mut timestamp, mut flags, mut sample) = (0, 0, None);
        stage(
            unsafe { reader.ReadSample(0, 0, None, Some(&mut flags), Some(&mut timestamp), Some(&mut sample)) },
            "Read a decoded MP4 frame",
        )?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(unavailable("The decoder reported an error reading the MP4"));
        }
        if let Some(sample) = sample {
            if stage(unsafe { sample.GetTotalLength() }, "Check the decoded frame size")? != 0 {
                let timing = expected.get_mut(decoded).ok_or_else(|| unavailable("The MP4 contains an unindexed frame"))?;
                let pts = timing.pts_100ns.ok_or_else(|| unavailable("The segment frame has no planned PTS"))?;
                let expected_duration = timing.duration_100ns.ok_or_else(|| unavailable("The segment frame has no planned duration"))?;
                let duration = stage(unsafe { sample.GetSampleDuration() }, "Read the MP4 frame duration")?;
                let end = timestamp.checked_add(duration).ok_or_else(|| unavailable("The decoded MP4 duration overflowed"))?;
                if duration <= 0 || previous_pts.is_some_and(|previous| timestamp <= previous)
                    || timestamp.abs_diff(pts) > 10_000 || duration.abs_diff(expected_duration) > 10_000
                    || end.abs_diff(pts.saturating_add(expected_duration)) > 10_000
                {
                    return Err(unavailable("The decoded segment timing does not match the frame index"));
                }
                timing.pts_100ns = Some(timestamp);
                timing.duration_100ns = Some(duration);
                previous_pts = Some(timestamp);
                previous_end = end;
                decoded += 1;
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            if decoded == expected.len() && decoded != 0 {
                return Ok(previous_end);
            }
            return Err(unavailable("The number of decoded MP4 frames does not match the index"));
        }
    }
}
