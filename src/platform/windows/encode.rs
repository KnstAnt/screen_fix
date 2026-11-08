use std::{mem::ManuallyDrop, sync::{Arc, Mutex}, thread, time::{Duration, Instant}};

use windows::{
    Win32::{Graphics::Direct3D11::ID3D11Texture2D, Media::MediaFoundation::*},
    core::{self as win_core, Interface, Ref, implement},
};

use super::{ProbeError, stage, unavailable};

pub(super) fn record<S>(
    encoder: &IMFTransform,
    state: &mut S,
    mut next_frame: impl FnMut(&mut S) -> Result<Option<(Arc<ID3D11Texture2D>, i64, i64)>, ProbeError>,
    mut output: impl FnMut(&mut S, &IMFTransform, &IMFSample) -> Result<(), ProbeError>,
) -> Result<usize, ProbeError> {
    let events: IMFMediaEventGenerator =
        stage(encoder.cast(), "Get hardware encoder events")?;
    message(encoder, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING)?;
    message(encoder, MFT_MESSAGE_NOTIFY_START_OF_STREAM)?;
    let mut deadline = Instant::now() + Duration::from_secs(5);
    let mut submitted = 0;
    let mut draining = false;
    loop {
        if Instant::now() >= deadline {
            return Err(unavailable("The encoder did not finish processing a frame within 5 seconds"));
        }
        let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            result => stage(result, "Get an encoder event")?,
        };
        let status = stage(unsafe { event.GetStatus() }, "Read the encoder event status")?;
        stage(status.ok(), "Hardware encoder processing failed")?;
        let kind = stage(unsafe { event.GetType() }, "Read the encoder event type")?;
        deadline = Instant::now() + Duration::from_secs(5);
        if kind == METransformNeedInput.0 as u32 && !draining {
            if let Some((texture, pts, duration)) = next_frame(state)? {
                let sample = input_sample(texture, pts, duration)?;
                stage(unsafe { encoder.ProcessInput(0, &sample, 0) }, "Submit a GPU frame to the encoder")?;
                submitted += 1;
            } else {
                draining = true;
                message(encoder, MFT_MESSAGE_NOTIFY_END_OF_STREAM)?;
                message(encoder, MFT_MESSAGE_COMMAND_DRAIN)?;
            }
            deadline = Instant::now() + Duration::from_secs(5);
        } else if kind == METransformHaveOutput.0 as u32 {
            if let Some(sample) = read_output(encoder)? {
                if stage(unsafe { sample.GetTotalLength() }, "Read the H.264 sample size")? != 0 {
                    output(state, encoder, &sample)?;
                }
            }
        } else if kind == METransformDrainComplete.0 as u32 && draining {
            break;
        }
    }
    message(encoder, MFT_MESSAGE_NOTIFY_END_STREAMING)?;
    Ok(submitted)
}

fn input_sample(texture: Arc<ID3D11Texture2D>, pts: i64, duration: i64) -> Result<IMFSample, ProbeError> {
    // The sample owns a reference to the GPU texture. No texture is overwritten.
    let buffer = stage(
        unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture.as_ref(), 0, false) },
        "Create the Media Foundation NV12 buffer",
    )?;
    let layout: IMF2DBuffer = stage(buffer.cast(), "Get the NV12 buffer layout")?;
    let length = stage(unsafe { layout.GetContiguousLength() }, "Read the NV12 buffer size")?;
    stage(unsafe { buffer.SetCurrentLength(length) }, "Set the NV12 buffer size")?;
    let tracked = stage(unsafe { MFCreateTrackedSample() }, "Create a tracked NV12 sample")?;
    let sample: IMFSample = stage(tracked.cast(), "Get the NV12 sample")?;
    stage(unsafe { sample.AddBuffer(&buffer) }, "Attach the NV12 buffer to the sample")?;
    stage(unsafe { sample.SetSampleTime(pts) }, "Set the sample timestamp")?;
    stage(unsafe { sample.SetSampleDuration(duration) }, "Set the sample duration")?;
    let release: IMFAsyncCallback = ReleaseTexture { texture: Mutex::new(Some(texture)) }.into();
    stage(unsafe { tracked.SetAllocator(&release, None) }, "Register the NV12 texture release callback")?;
    Ok(sample)
}

#[implement(IMFAsyncCallback)]
struct ReleaseTexture {
    texture: Mutex<Option<Arc<ID3D11Texture2D>>>,
}

impl IMFAsyncCallback_Impl for ReleaseTexture_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> win_core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }

    fn Invoke(&self, _: Ref<'_, IMFAsyncResult>) -> win_core::Result<()> {
        let mut texture = self.texture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        texture.take();
        Ok(())
    }
}

fn message(encoder: &IMFTransform, message: MFT_MESSAGE_TYPE) -> Result<(), ProbeError> {
    stage(unsafe { encoder.ProcessMessage(message, 0) }, "Control the hardware encoder stream")
}

fn read_output(encoder: &IMFTransform) -> Result<Option<IMFSample>, ProbeError> {
    let mut output = MFT_OUTPUT_DATA_BUFFER::default();
    let mut status = 0;
    // Async hardware MFT supplies the output sample. Take both transferred
    // COM references even if ProcessOutput returns an error.
    let result = unsafe { encoder.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) };
    let sample = ManuallyDrop::into_inner(output.pSample);
    drop(ManuallyDrop::into_inner(output.pEvents));
    stage(result, "Read encoded H.264 output")?;
    Ok(sample)
}
