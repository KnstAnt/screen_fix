use std::{mem::ManuallyDrop, sync::Arc, thread, time::{Duration, Instant}};

use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct3D11::*,
            Dxgi::Common::{
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
            },
        },
    },
    core::Interface,
};

use super::{ProbeError, stage, unavailable};

pub(super) const WIDTH: u32 = 1920;
pub(super) const HEIGHT: u32 = 1080;

pub(super) struct Converter<'device> {
    device: &'device ID3D11Device,
    video: ID3D11VideoDevice,
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext1,
    fence_context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    next_fence: u64,
    textures: Vec<Arc<ID3D11Texture2D>>,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    width: i32,
    height: i32,
}

impl<'device> Converter<'device> {
    pub(super) fn new(
        device: &'device ID3D11Device,
        width: i32,
        height: i32,
    ) -> Result<Self, ProbeError> {
        if width <= 0 || height <= 0 {
            return Err(unavailable("Недопустимый размер источника WGC"));
        }
        let video: ID3D11VideoDevice = stage(device.cast(), "Получение видеоустройства D3D11")?;
        let context = stage(unsafe { device.GetImmediateContext() }, "Получение контекста D3D11")?;
        let video_context: ID3D11VideoContext1 =
            stage(context.cast(), "Получение контекста преобразования D3D11")?;
        let fence_device: ID3D11Device5 = stage(device.cast(), "Получение интерфейса GPU fence")?;
        let fence_context: ID3D11DeviceContext4 = stage(context.cast(), "Получение контекста GPU fence")?;
        let mut fence: Option<ID3D11Fence> = None;
        stage(
            unsafe { fence_device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) },
            "Создание GPU fence",
        )?;
        let fence = fence.ok_or_else(|| unavailable("D3D11 не вернул GPU fence"))?;

        let rate = DXGI_RATIONAL { Numerator: 30, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width as u32,
            InputHeight: height as u32,
            OutputFrameRate: rate,
            OutputWidth: WIDTH,
            OutputHeight: HEIGHT,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = stage(
            unsafe { video.CreateVideoProcessorEnumerator(&content) },
            "Проверка видеопроцессора D3D11",
        )?;
        let processor = stage(
            unsafe { video.CreateVideoProcessor(&enumerator, 0) },
            "Создание видеопроцессора D3D11",
        )?;
        let source_rect = RECT { left: 0, top: 0, right: width, bottom: height };
        let dest_rect = RECT { left: 0, top: 0, right: WIDTH as i32, bottom: HEIGHT as i32 };
        // The processor belongs to this recording and its geometry is fixed.
        unsafe {
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            video_context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source_rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest_rect));
        }
        Ok(Self {
            device, video, context, video_context, fence_context, fence, next_fence: 0, textures: Vec::new(),
            enumerator, processor, width, height,
        })
    }

    pub(super) fn convert(
        &mut self,
        source: &ID3D11Texture2D,
        width: i32,
        height: i32,
    ) -> Result<Arc<ID3D11Texture2D>, ProbeError> {
        if (width, height) != (self.width, self.height) {
            return Err(unavailable("Размер области WGC изменился во время записи"));
        }
        let fence_value = self.next_fence.checked_add(1)
            .filter(|value| *value < u64::MAX)
            .ok_or_else(|| unavailable("Исчерпаны значения GPU fence"))?;
        self.next_fence = fence_value;
        let texture = self.acquire_texture()?;

        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut input_view = None;
        stage(
            unsafe { self.video.CreateVideoProcessorInputView(source, &self.enumerator, &input_desc, Some(&mut input_view)) },
            "Подготовка BGRA-кадра для видеопроцессора",
        )?;
        let mut output_view = None;
        stage(
            unsafe { self.video.CreateVideoProcessorOutputView(texture.as_ref(), &self.enumerator, &output_desc, Some(&mut output_view)) },
            "Подготовка NV12-выхода видеопроцессора",
        )?;
        let output_view = output_view.ok_or_else(|| unavailable("Не получен NV12-выход видеопроцессора"))?;
        let input_view = input_view.ok_or_else(|| unavailable("Не получен BGRA-вход видеопроцессора"))?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        };
        // Views refer to resources on this device. The caller holds the WGC frame lease.
        let blit = unsafe {
            let result = self.video_context.VideoProcessorBlt(&self.processor, &output_view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result
        };
        stage(blit, "Преобразование BGRA → NV12 на GPU")?;
        stage(unsafe { self.fence_context.Signal(&self.fence, fence_value) }, "Сигнал завершения преобразования GPU")?;
        unsafe { self.context.Flush() };

        // The converter waits for GPU completion before returning the capture lease.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let value = unsafe { self.fence.GetCompletedValue() };
            if value == u64::MAX {
                return Err(unavailable("Устройство GPU недоступно после преобразования"));
            }
            if value >= fence_value {
                return Ok(texture);
            }
            if Instant::now() >= deadline {
                return Err(unavailable("GPU не завершил преобразование за 3 секунды"));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn acquire_texture(&mut self) -> Result<Arc<ID3D11Texture2D>, ProbeError> {
        if let Some(texture) = self.textures.iter().find(|texture| Arc::strong_count(texture) == 1) {
            // Only this owner issues leases. A tracked sample and the last
            // observation keep independent leases while using the texture.
            return Ok(Arc::clone(texture));
        }
        if self.textures.len() >= 16 {
            return Err(unavailable("Аппаратный кодировщик удерживает все 16 текстур NV12"));
        }
        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        stage(
            unsafe { self.device.CreateTexture2D(&texture_desc, None, Some(&mut texture)) },
            "Создание GPU-текстуры NV12",
        )?;
        let texture = Arc::new(texture.ok_or_else(|| unavailable("D3D11 не вернул текстуру NV12"))?);
        self.textures.push(Arc::clone(&texture));
        Ok(texture)
    }

}
