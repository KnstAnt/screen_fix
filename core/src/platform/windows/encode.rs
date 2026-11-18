use std::{
    mem::ManuallyDrop,
    sync::{Arc, Mutex, Weak, atomic::{AtomicUsize, Ordering}},
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{Graphics::{Direct3D11::{ID3D11Texture2D, ID3D11Device}, Dxgi::IDXGIResource}, Media::MediaFoundation::*, System::Variant::VARIANT},
    core::{self as win_core, GUID, Interface, Ref, implement},
};

use super::{ProbeError, mf, stage, unavailable};

/// Inputs submitted to the encoder and not yet returned, and inputs whose
/// sample the encoder still holds: each is bounded by this count.
pub(super) const MAX_INPUTS: usize = 3;
/// The encoder must make progress within this interval, sleep excluded.
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_TIMEOUT_MESSAGE: &str = "The H.264 encoder failed to make progress within 5 seconds";
/// Outstanding input requests an asynchronous encoder can issue.
const MAX_INPUT_REQUESTS: u32 = 64;
/// The largest output buffer the caller allocates for a synchronous encoder.
const MAX_OUTPUT_BUFFER_BYTES: u32 = 32 * 1024 * 1024;
/// The wait between polls of an idle encoder.
const POLL: Duration = Duration::from_millis(1);

#[derive(Clone)]
pub(super) enum Input {
    Texture(Arc<ID3D11Texture2D>),
    Memory(Arc<[u8]>),
}

/// One picture for the encoder: its input, timing in 100 ns and whether it
/// must be an IDR.
pub(super) struct Picture {
    pub input: Input,
    pub pts: i64,
    pub duration: i64,
    pub force_idr: bool,
}

pub(super) enum Next {
    Frame(Picture),
    Wait,
    DrainPrefix,
    End,
}

/// The owner of one encoder stream: it supplies pictures and takes the
/// encoded output, on the encoder's thread.
pub(super) trait EncodeOwner {
    /// Returns true after a system suspend: time asleep is not encoder time.
    fn observe_admission(&mut self) -> Result<bool, ProbeError>;
    fn next_frame(&mut self) -> Result<Next, ProbeError>;
    /// The encoder completed the drain that `Next::DrainPrefix` requested.
    fn prefix_drained(&mut self) -> Result<(), ProbeError>;
    /// Take one encoded picture. Returns the submitted inputs it retires.
    fn output(&mut self, transform: &IMFTransform, sample: &IMFSample) -> Result<usize, ProbeError>;
}

pub(super) fn codec_value(codec: &ICodecAPI, key: &GUID, value: u32, operation: &'static str) -> Result<(), ProbeError> {
    // The generated SetValue wrapper accepts any nonnegative HRESULT.
    // S_FALSE denotes a read-only property and is not a successful change.
    let variant = VARIANT::from(value);
    let result = unsafe { (codec.vtable().SetValue)(codec.as_raw(), key, &variant) };
    if result == windows::Win32::Foundation::S_FALSE {
        return Err(unavailable(operation));
    }
    stage(result.ok(), operation)
}

pub(super) fn set_properties(encoder: &IMFTransform, fps: u32, cap: u32) -> Result<(), ProbeError> {
    let codec: ICodecAPI = stage(encoder.cast(), "Get the H.264 encoder controls")?;
    let supported = unsafe { (codec.vtable().IsSupported)(codec.as_raw(), &CODECAPI_AVEncMPVDefaultBPictureCount) };
    if supported == windows::Win32::Foundation::S_OK {
        codec_value(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0, "Disable H.264 B-frames")?;
    }
    let latency = unsafe { (codec.vtable().IsSupported)(codec.as_raw(), &CODECAPI_AVLowLatencyMode) };
    if latency == windows::Win32::Foundation::S_OK {
        // This property expects VT_BOOL, unlike the bitrate and GOP controls.
        let low_latency = VARIANT::from(true);
        let _ = unsafe { codec.SetValue(&CODECAPI_AVLowLatencyMode, &low_latency) };
    }
    codec_value(&codec, &CODECAPI_AVEncCommonRateControlMode,
        eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32, "Set peak-constrained H.264 VBR")?;
    codec_value(&codec, &CODECAPI_AVEncCommonMaxBitRate, cap, "Set the H.264 peak bitrate")?;
    codec_value(&codec, &CODECAPI_AVEncMPVGOPSize, 2 * fps, "Set the two-second H.264 GOP")?;
    Ok(())
}

/// The progress of one encoder stream between its owner and the encoder.
struct Stream {
    /// The last encoder progress, for the progress timeout.
    activity: Instant,
    submitted: usize,
    draining: bool,
    /// The drain ends an approval prefix; the stream restarts after it.
    prefix_drain: bool,
    /// Input requests of an asynchronous encoder not yet served.
    requested: u32,
    /// Inputs submitted and not yet returned.
    outstanding: usize,
    /// The next picture, and since when it waits for the encoder.
    pending: Option<(Picture, Instant)>,
}

impl Stream {
    /// Restart the progress timeout, also for the waiting picture.
    fn resume(&mut self) {
        self.activity = Instant::now();
        if let Some((_, since)) = &mut self.pending { *since = self.activity; }
    }

    /// Hand an encoded picture to the owner. Time in the owner's output step
    /// (rotation, a wait for a verifier slot) is not encoder time.
    fn retire(&mut self, owner: &mut impl EncodeOwner, encoder: &IMFTransform, sample: &IMFSample) -> Result<(), ProbeError> {
        let retired = owner.output(encoder, sample)?;
        self.outstanding = self.outstanding.checked_sub(retired)
            .ok_or_else(|| unavailable("The H.264 encoder returned an unsolicited picture"))?;
        self.resume();
        Ok(())
    }

    /// The approval prefix drained: restart the stream for the next prefix.
    fn restart_after_prefix(&mut self, owner: &mut impl EncodeOwner, encoder: &IMFTransform) -> Result<(), ProbeError> {
        owner.prefix_drained()?;
        message(encoder, MFT_MESSAGE_NOTIFY_START_OF_STREAM)?;
        self.requested = 0;
        self.outstanding = 0;
        self.draining = false;
        self.prefix_drain = false;
        self.activity = Instant::now();
        Ok(())
    }

    fn drain(&mut self, encoder: &IMFTransform) -> Result<(), ProbeError> {
        self.draining = true;
        self.activity = Instant::now();
        message(encoder, MFT_MESSAGE_COMMAND_DRAIN)
    }
}

pub(super) fn record(
    encoder: &IMFTransform,
    device: Option<&ID3D11Device>,
    owner: &mut impl EncodeOwner,
) -> Result<usize, ProbeError> {
    let asynchronous = unsafe { encoder.GetAttributes() }.ok()
        .and_then(|attributes| unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
        .is_some_and(|enabled| enabled != 0);
    let events = if asynchronous {
        Some(stage(encoder.cast::<IMFMediaEventGenerator>(), "Get encoder events")?)
    } else {
        None
    };
    let codec: ICodecAPI = stage(encoder.cast(), "Get the H.264 encoder controls")?;
    let info = stage(unsafe { encoder.GetOutputStreamInfo(0) }, "Read the H.264 output buffer contract")?;
    let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
    let mut output_buffer = if info.dwFlags & provides != 0 { None } else {
        if info.cbSize == 0 || info.cbSize > MAX_OUTPUT_BUFFER_BYTES {
            return Err(unavailable("The H.264 output buffer contract is outside the supported bounds"));
        }
        Some(allocate_sample(info.cbSize, info.cbAlignment.saturating_sub(1))?)
    };
    message(encoder, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING)?;
    message(encoder, MFT_MESSAGE_NOTIFY_START_OF_STREAM)?;

    let result = (|| {
        let mut stream = Stream {
            activity: Instant::now(), submitted: 0, draining: false, prefix_drain: false, requested: 0, outstanding: 0,
            pending: None,
        };
        let mut imports = Vec::new();
        let active = Arc::new(AtomicUsize::new(0));
        loop {
            if owner.observe_admission()? {
                stream.resume();
            }
            if ((stream.draining || stream.outstanding != 0) && stream.activity.elapsed() >= PROGRESS_TIMEOUT)
                || stream.pending.as_ref().is_some_and(|(_, since)| since.elapsed() >= PROGRESS_TIMEOUT)
            {
                return Err(unavailable(PROGRESS_TIMEOUT_MESSAGE));
            }
            if let Some(events) = &events {
                match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {}
                    result => {
                        let event = stage(result, "Get an encoder event")?;
                        let status = stage(unsafe { event.GetStatus() }, "Read the encoder event status")?;
                        stage(status.ok(), "H.264 encoder processing failed")?;
                        let kind = stage(unsafe { event.GetType() }, "Read the encoder event type")?;
                        if kind == METransformNeedInput.0 as u32 && !stream.draining {
                            stream.requested = stream.requested.checked_add(1)
                                .filter(|value| *value <= MAX_INPUT_REQUESTS)
                                .ok_or_else(|| unavailable("The encoder issued too many outstanding input requests"))?;
                        } else if kind == METransformHaveOutput.0 as u32 {
                            if let Output::Sample(sample) = read_output(encoder, &mut output_buffer)? {
                                stream.retire(owner, encoder, &sample)?;
                            }
                            // Work in output order before taking another input request.
                            continue;
                        } else if kind == METransformDrainComplete.0 as u32 && stream.draining {
                            if !stream.prefix_drain {
                                break;
                            }
                            stream.restart_after_prefix(owner, encoder)?;
                            continue;
                        }
                    }
                }
            } else {
                match read_output(encoder, &mut output_buffer)? {
                    Output::Sample(sample) => {
                        stream.retire(owner, encoder, &sample)?;
                        continue;
                    }
                    Output::NeedInput if stream.draining => {
                        if !stream.prefix_drain {
                            break;
                        }
                        // A synchronous encoder issues no input requests.
                        stream.restart_after_prefix(owner, encoder)?;
                        continue;
                    }
                    Output::Empty if stream.outstanding != 0 => {
                        // The encoder holds pictures but has none ready: wait
                        // for it instead of polling it in a tight loop.
                        thread::sleep(POLL);
                        continue;
                    }
                    Output::Empty | Output::NeedInput => {}
                }
            }
            if !stream.draining && stream.pending.is_none() {
                match owner.next_frame()? {
                    Next::Frame(picture) => stream.pending = Some((picture, Instant::now())),
                    Next::Wait => {}
                    Next::DrainPrefix => {
                        stream.prefix_drain = true;
                        stream.drain(encoder)?;
                        continue;
                    }
                    Next::End => {
                        message(encoder, MFT_MESSAGE_NOTIFY_END_OF_STREAM)?;
                        if stream.submitted == 0 {
                            break;
                        }
                        stream.drain(encoder)?;
                        continue;
                    }
                }
            }
            if !stream.draining && (events.is_none() || stream.requested > 0) && stream.outstanding < MAX_INPUTS
                && active.load(Ordering::Acquire) < MAX_INPUTS
                && let Some((Picture { input, pts, duration, force_idr }, _)) = stream.pending.take()
            {
                let sample = input_sample(input, pts, duration, device, &mut imports, &active)?;
                if force_idr {
                    codec_value(&codec, &CODECAPI_AVEncVideoForceKeyFrame, 1, "Request a boundary IDR")?;
                }
                stage(unsafe { encoder.ProcessInput(0, &sample, 0) }, "Submit an NV12 frame to the encoder")?;
                stream.submitted += 1;
                stream.outstanding += 1;
                stream.requested = stream.requested.saturating_sub(1);
                stream.activity = Instant::now();
                continue;
            }
            thread::sleep(POLL);
        }
        Ok(stream.submitted)
    })();
    if result.is_err() {
        let _ = message(encoder, MFT_MESSAGE_COMMAND_FLUSH);
    }
    let ended = message(encoder, MFT_MESSAGE_NOTIFY_END_STREAMING);
    result.and_then(|count| ended.map(|()| count))
}

fn input_sample(
    input: Input, pts: i64, duration: i64, device: Option<&ID3D11Device>,
    imports: &mut Vec<(Weak<ID3D11Texture2D>, ID3D11Texture2D)>, active: &Arc<AtomicUsize>,
) -> Result<IMFSample, ProbeError> {
    let buffer = match &input {
        Input::Texture(texture) => {
            let device = device.ok_or_else(|| unavailable("The hardware H.264 device is unavailable"))?;
            let imported = open_surface(device, texture, imports)?;
            let buffer = mf::surface(&ID3D11Texture2D::IID, &imported)?;
            let layout: IMF2DBuffer = stage(buffer.cast(), "Get the NV12 surface layout")?;
            let length = stage(unsafe { layout.GetContiguousLength() }, "Read the NV12 surface size")?;
            stage(unsafe { buffer.SetCurrentLength(length) }, "Set the NV12 surface size")?;
            buffer
        }
        Input::Memory(bytes) => {
            let length = u32::try_from(bytes.len()).map_err(|_| unavailable("The NV12 input is too large"))?;
            let buffer = mf::buffer(length, 0)?;
            write_buffer(&buffer, length, |memory| memory.copy_from_slice(bytes))?;
            buffer
        }
    };
    let tracked = mf::tracked_sample()?;
    let sample: IMFSample = stage(tracked.cast(), "Get the tracked NV12 sample")?;
    stage(unsafe { sample.AddBuffer(&buffer) }, "Attach the NV12 buffer to the sample")?;
    stage(unsafe { sample.SetSampleTime(pts) }, "Set the sample timestamp")?;
    stage(unsafe { sample.SetSampleDuration(duration) }, "Set the sample duration")?;
    active.fetch_add(1, Ordering::AcqRel);
    let release: IMFAsyncCallback = ReleaseInput {
        held: Mutex::new(Some(SampleLease { _input: input, active: Arc::clone(active) })),
    }.into();
    stage(unsafe { tracked.SetAllocator(&release, None) }, "Register the NV12 sample release callback")?;
    Ok(sample)
}

fn open_surface(
    device: &ID3D11Device, texture: &Arc<ID3D11Texture2D>,
    imports: &mut Vec<(Weak<ID3D11Texture2D>, ID3D11Texture2D)>,
) -> Result<ID3D11Texture2D, ProbeError> {
    imports.retain(|(owner, _)| owner.strong_count() > 0);
    let source = Arc::downgrade(texture);
    if let Some((_, imported)) = imports.iter().find(|(owner, _)| owner.ptr_eq(&source)) {
        return Ok(imported.clone());
    }
    let resource: IDXGIResource = stage(texture.cast(), "Get the shared NV12 surface")?;
    let handle = stage(unsafe { resource.GetSharedHandle() }, "Get the shared NV12 handle")?;
    // This is the legacy DXGI resource handle, not a file or kernel handle.
    // The capture fence and a sample lease prevent access during a write.
    let mut imported = None;
    stage(unsafe { device.OpenSharedResource(handle, &mut imported) }, "Open the NV12 surface on the encoder device")?;
    let imported: ID3D11Texture2D = imported.ok_or_else(|| unavailable("The encoder did not receive its shared NV12 surface"))?;
    imports.push((source, imported.clone()));
    Ok(imported)
}

struct SampleLease {
    _input: Input,
    active: Arc<AtomicUsize>,
}

impl Drop for SampleLease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn write_buffer(
    buffer: &IMFMediaBuffer, length: u32, fill: impl FnOnce(&mut [u8]),
) -> Result<(), ProbeError> {
    let mut data = std::ptr::null_mut();
    let mut capacity = 0;
    stage(unsafe { buffer.Lock(&mut data, Some(&mut capacity), None) }, "Lock the encoder buffer")?;
    let valid = !data.is_null() && capacity >= length && length as usize <= isize::MAX as usize;
    if valid {
        // The buffer owns the locked writable allocation.
        fill(unsafe { std::slice::from_raw_parts_mut(data, length as usize) });
    }
    stage(unsafe { buffer.Unlock() }, "Unlock the encoder buffer")?;
    if !valid {
        return Err(unavailable("The Media Foundation buffer is smaller than requested"));
    }
    stage(unsafe { buffer.SetCurrentLength(length) }, "Set the encoder buffer size")
}

#[implement(IMFAsyncCallback)]
struct ReleaseInput {
    held: Mutex<Option<SampleLease>>,
}

impl IMFAsyncCallback_Impl for ReleaseInput_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> win_core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }

    fn Invoke(&self, _: Ref<'_, IMFAsyncResult>) -> win_core::Result<()> {
        let mut held = self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        held.take();
        Ok(())
    }
}

fn message(encoder: &IMFTransform, message: MFT_MESSAGE_TYPE) -> Result<(), ProbeError> {
    stage(unsafe { encoder.ProcessMessage(message, 0) }, "Control the H.264 encoder stream")
}

fn allocate_sample(size: u32, alignment: u32) -> Result<(IMFSample, IMFMediaBuffer), ProbeError> {
    let sample = mf::sample()?;
    let buffer = mf::buffer(size, alignment)?;
    stage(unsafe { sample.AddBuffer(&buffer) }, "Attach the H.264 output buffer")?;
    Ok((sample, buffer))
}

enum Output {
    Sample(IMFSample),
    Empty,
    NeedInput,
}

fn read_output(
    encoder: &IMFTransform, output_buffer: &mut Option<(IMFSample, IMFMediaBuffer)>,
) -> Result<Output, ProbeError> {
    // The sync software MFT writes into caller buffers. Use a fresh sample
    // because the previous one may still be retained by the MP4 sink.
    let mut output = MFT_OUTPUT_DATA_BUFFER::default();
    if let Some((sample, buffer)) = output_buffer {
        stage(unsafe { buffer.SetCurrentLength(0) }, "Reset the H.264 output buffer")?;
        output.pSample = ManuallyDrop::new(Some(sample.clone()));
    }
    let mut status = 0;
    let result = unsafe { encoder.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) };
    let sample = ManuallyDrop::into_inner(output.pSample);
    drop(ManuallyDrop::into_inner(output.pEvents));
    match result {
        Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(Output::NeedInput),
        result => stage(result, "Read encoded H.264 output")?,
    }
    let Some(sample) = sample else { return Ok(Output::Empty) };
    let length = stage(unsafe { sample.GetTotalLength() }, "Read the H.264 sample size")?;
    if length == 0 {
        return Ok(Output::Empty);
    }
    if output_buffer.is_some() {
        let info = stage(unsafe { encoder.GetOutputStreamInfo(0) }, "Read the H.264 output buffer contract")?;
        *output_buffer = Some(allocate_sample(info.cbSize, info.cbAlignment.saturating_sub(1))?);
    }
    Ok(Output::Sample(sample))
}
