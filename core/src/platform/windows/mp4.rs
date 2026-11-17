use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::{AtomicU8, Ordering}, mpsc::{Receiver, SyncSender}},
};

use windows::{Win32::{Graphics::Dxgi::IDXGIAdapter1, Media::MediaFoundation::*}, core::PCWSTR};

use crate::protocol::SegmentId;

use super::{ProbeError, io_error, mf, stage, unavailable, wide};

const REVOKED: &str = "The elevated video prefix was revoked before publication";

pub(super) struct Recording {
    writer: IMFSinkWriter,
    stream: u32,
    pending: PathBuf,
    path: PathBuf,
}

/// The claim on a handed-off part: the verifier publishes it only if the
/// video worker has not abandoned it, and the worker abandons it only if its
/// publication has not begun.
pub(super) const CLAIM_OPEN: u8 = 0;
const CLAIM_PUBLISHING: u8 = 1;
pub(super) const CLAIM_ABANDONED: u8 = 2;

pub(super) const DEADLINE: &str = "The MP4 publication deadline expired";
const ABANDONED: &str = "The video owner abandoned the overdue segment verification";

/// A finalized and flushed `.part` handed to the segment verifier.
pub(super) struct SegmentToVerify {
    pub segment: SegmentId,
    pending: PathBuf,
    path: PathBuf,
    /// The planned (PTS, duration) of each indexed picture, in 100 ns.
    expected: Vec<(i64, i64)>,
    size: [u32; 2],
    /// Whether the media, with the decoded end once known, is still authorized.
    authorized: Box<dyn Fn(Option<i64>) -> bool + Send>,
    pub claim: Arc<AtomicU8>,
}

/// A verified and published segment: its decoded timing and extent.
pub(super) struct Published {
    pub local_end_100ns: i64,
    pub bytes: u64,
    pub timing: Vec<(i64, i64)>,
}

pub(super) struct Verdict {
    pub segment: SegmentId,
    /// `Ok(None)`: the media was revoked and its part removed.
    pub outcome: Result<Option<Published>, ProbeError>,
}

impl Recording {
    pub(super) fn open(media_type: &IMFMediaType, path: &Path) -> Result<Self, ProbeError> {
        let pending = path.with_extension("mp4.part");
        let path = path.to_owned();
        let filename = wide(&pending);
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

    /// Finalize, release and flush the part on the video worker (the ALG
    /// rotation steps). The returned part still needs verification before
    /// publication. `None`: the media was revoked and its part removed.
    pub(super) fn close(
        self, segment: SegmentId, expected: Vec<(i64, i64)>, size: [u32; 2],
        deadline: impl Fn() -> Option<std::time::Instant>, authorized: Box<dyn Fn(Option<i64>) -> bool + Send>,
    ) -> Result<Option<SegmentToVerify>, ProbeError> {
        let pending = self.pending.clone();
        let check = || {
            if !authorized(None) {
                return Err(unavailable(REVOKED));
            }
            check_deadline(&deadline)
        };
        let closed = (|| {
            check()?;
            stage(unsafe { self.writer.Finalize() }, "Finalize the MP4")?;
            drop(self.writer);
            check()?;
            fs::OpenOptions::new().write(true).open(&pending).and_then(|file| file.sync_all())
                .map_err(|error| io_error("Flush the finalized MP4", error))?;
            check()
        })();
        match closed {
            Ok(()) => Ok(Some(SegmentToVerify {
                segment, pending, path: self.path, expected, size, authorized, claim: Arc::new(AtomicU8::new(CLAIM_OPEN)),
            })),
            Err(error) if error.stage == REVOKED => { remove_part(&pending)?; Ok(None) }
            Err(error) => Err(error),
        }
    }
}

fn check_deadline(deadline: &impl Fn() -> Option<std::time::Instant>) -> Result<(), ProbeError> {
    if deadline().is_some_and(|limit| std::time::Instant::now() >= limit) {
        return Err(unavailable(DEADLINE));
    }
    Ok(())
}

/// The per-monitor segment verifier. It decodes each handed-off part, with a
/// hardware decoder on its own D3D11 device when available, and publishes it
/// by renaming it to `.mp4` only after verification. The video worker keeps
/// at most two parts in flight.
pub(super) fn verify_segments(
    adapter: IDXGIAdapter1, parts: Receiver<SegmentToVerify>, verdicts: SyncSender<Verdict>,
    deadline: impl Fn() -> Option<std::time::Instant>,
) {
    // Without COM the verifier exits; the worker sees it as disconnected.
    let Ok(_apartment) = super::Apartment::initialize() else { return };
    let decoder = super::capture_device(&adapter).and_then(|device| {
        let (token, manager) = mf::device_manager()?;
        stage(unsafe { manager.ResetDevice(&device, token) }, "Attach the segment decoder D3D11 device")?;
        Ok(manager)
    }).ok();
    for part in parts {
        let segment = part.segment;
        let outcome = part.publish(decoder.as_ref(), &deadline);
        if verdicts.send(Verdict { segment, outcome }).is_err() {
            return;
        }
    }
}

impl SegmentToVerify {
    fn publish(
        self, decoder: Option<&IMFDXGIDeviceManager>, deadline: &impl Fn() -> Option<std::time::Instant>,
    ) -> Result<Option<Published>, ProbeError> {
        match self.verify_and_rename(decoder, deadline) {
            Err(error) if error.stage == REVOKED => { remove_part(&self.pending)?; Ok(None) }
            // A rejected or abandoned part stays `.part`.
            result => result.map(Some),
        }
    }

    fn verify_and_rename(
        &self, decoder: Option<&IMFDXGIDeviceManager>, deadline: &impl Fn() -> Option<std::time::Instant>,
    ) -> Result<Published, ProbeError> {
        let decoded_end = Cell::new(None);
        let check = || {
            if self.claim.load(Ordering::Acquire) == CLAIM_ABANDONED {
                return Err(unavailable(ABANDONED));
            }
            if !(self.authorized)(decoded_end.get()) {
                return Err(unavailable(REVOKED));
            }
            check_deadline(deadline)
        };
        let mut timing = self.expected.clone();
        let hardware = decoder.map(|decoder| verify_decoded_frames(&self.pending, &mut timing, self.size, &check, Some(decoder)));
        let end = match hardware {
            Some(Ok(end)) => end,
            _ => {
                // The software decoder decides when hardware decoding is
                // unavailable or failed for a reason other than this check.
                check()?;
                timing.clone_from(&self.expected);
                verify_decoded_frames(&self.pending, &mut timing, self.size, &check, None)?
            }
        };
        decoded_end.set(Some(end));
        let bytes = fs::metadata(&self.pending).map_err(|error| io_error("Inspect the verified MP4 extent", error))?.len();
        if bytes == 0 {
            return Err(unavailable("The verified MP4 file is empty"));
        }
        check()?;
        // Publication is the commit point: once claimed, the worker waits for
        // this verdict, and a published file is never returned to `.part`.
        if self.claim.compare_exchange(CLAIM_OPEN, CLAIM_PUBLISHING, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(unavailable(ABANDONED));
        }
        for retry in 0..6 {
            match fs::rename(&self.pending, &self.path) {
                Ok(()) => return Ok(Published { local_end_100ns: end, bytes, timing }),
                Err(error) if retry < 5 && matches!(error.raw_os_error(), Some(32 | 33)) => {
                    check_deadline(deadline)?;
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
    path: &Path, expected: &mut [(i64, i64)], size: [u32; 2],
    check: &impl Fn() -> Result<(), ProbeError>, decoder: Option<&IMFDXGIDeviceManager>,
) -> Result<i64, ProbeError> {
    let filename = wide(path);
    let reader = mf::mp4_part_reader(PCWSTR(filename.as_ptr()), decoder)?;
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
            // A hardware decoder returns surfaces whose buffer length is not a
            // picture size; every sample it returns is a decoded picture.
            if decoder.is_some() || stage(unsafe { sample.GetTotalLength() }, "Check the decoded frame size")? != 0 {
                let timing = expected.get_mut(decoded).ok_or_else(|| unavailable("The MP4 contains an unindexed frame"))?;
                let (pts, expected_duration) = *timing;
                let duration = stage(unsafe { sample.GetSampleDuration() }, "Read the MP4 frame duration")?;
                let end = timestamp.checked_add(duration).ok_or_else(|| unavailable("The decoded MP4 duration overflowed"))?;
                if duration <= 0 || previous_pts.is_some_and(|previous| timestamp <= previous)
                    || timestamp.abs_diff(pts) > 10_000 || duration.abs_diff(expected_duration) > 10_000
                    || end.abs_diff(pts.saturating_add(expected_duration)) > 10_000
                {
                    return Err(unavailable("The decoded segment timing does not match the frame index"));
                }
                *timing = (timestamp, duration);
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
