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
        stage(encoder.cast(), "Получение событий аппаратного кодировщика")?;
    message(encoder, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING)?;
    message(encoder, MFT_MESSAGE_NOTIFY_START_OF_STREAM)?;
    let mut deadline = Instant::now() + Duration::from_secs(5);
    let mut submitted = 0;
    let mut draining = false;
    loop {
        if Instant::now() >= deadline {
            return Err(unavailable("Кодировщик не завершил обработку кадра за 5 секунд"));
        }
        let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            result => stage(result, "Получение события кодировщика")?,
        };
        let status = stage(unsafe { event.GetStatus() }, "Чтение статуса события кодировщика")?;
        stage(status.ok(), "Ошибка обработки в аппаратном кодировщике")?;
        let kind = stage(unsafe { event.GetType() }, "Чтение типа события кодировщика")?;
        deadline = Instant::now() + Duration::from_secs(5);
        if kind == METransformNeedInput.0 as u32 && !draining {
            if let Some((texture, pts, duration)) = next_frame(state)? {
                let sample = input_sample(texture, pts, duration)?;
                stage(unsafe { encoder.ProcessInput(0, &sample, 0) }, "Подача GPU-кадра кодировщику")?;
                submitted += 1;
            } else {
                draining = true;
                message(encoder, MFT_MESSAGE_NOTIFY_END_OF_STREAM)?;
                message(encoder, MFT_MESSAGE_COMMAND_DRAIN)?;
            }
            deadline = Instant::now() + Duration::from_secs(5);
        } else if kind == METransformHaveOutput.0 as u32 {
            if let Some(sample) = read_output(encoder)? {
                if stage(unsafe { sample.GetTotalLength() }, "Чтение размера образца H.264")? != 0 {
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
        "Создание буфера Media Foundation для NV12",
    )?;
    let layout: IMF2DBuffer = stage(buffer.cast(), "Получение раскладки NV12-буфера")?;
    let length = stage(unsafe { layout.GetContiguousLength() }, "Чтение размера NV12-буфера")?;
    stage(unsafe { buffer.SetCurrentLength(length) }, "Установка размера NV12-буфера")?;
    let tracked = stage(unsafe { MFCreateTrackedSample() }, "Создание отслеживаемого образца NV12")?;
    let sample: IMFSample = stage(tracked.cast(), "Получение образца NV12")?;
    stage(unsafe { sample.AddBuffer(&buffer) }, "Подключение NV12 к образцу")?;
    stage(unsafe { sample.SetSampleTime(pts) }, "Установка метки образца")?;
    stage(unsafe { sample.SetSampleDuration(duration) }, "Установка длительности образца")?;
    let release: IMFAsyncCallback = ReleaseTexture { texture: Mutex::new(Some(texture)) }.into();
    stage(unsafe { tracked.SetAllocator(&release, None) }, "Регистрация возврата текстуры NV12")?;
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
    stage(unsafe { encoder.ProcessMessage(message, 0) }, "Управление потоком аппаратного кодировщика")
}

fn read_output(encoder: &IMFTransform) -> Result<Option<IMFSample>, ProbeError> {
    let mut output = MFT_OUTPUT_DATA_BUFFER::default();
    let mut status = 0;
    // Async hardware MFT supplies the output sample. Take both transferred
    // COM references even if ProcessOutput returns an error.
    let result = unsafe { encoder.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) };
    let sample = ManuallyDrop::into_inner(output.pSample);
    drop(ManuallyDrop::into_inner(output.pEvents));
    stage(result, "Чтение закодированного H.264")?;
    Ok(sample)
}
