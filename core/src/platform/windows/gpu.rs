use std::{mem::ManuallyDrop, sync::{Arc, Weak}};

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

use super::{ProbeError, encode, pixels, stage, unavailable, video};

/// The picture in conversion, which the converter itself holds.
const IN_CONVERSION: usize = 1;
/// Converted pictures that can be in use at once: those the video worker's
/// slot selection holds, those queued to it, the encoder's inputs in flight
/// and the one in conversion. Each converter allocates up to this many NV12
/// surfaces, or keeps this many software pictures, on demand.
const VIDEO_INPUTS: usize = video::SELECTION_FRAMES + video::OBSERVATION_QUEUE + encode::MAX_INPUTS + IN_CONVERSION;

#[derive(Clone, Copy)]
pub(super) struct VideoFormat {
    pub source: [u32; 2],
    pub content: [u32; 2],
    pub encoded: [u32; 2],
    pub fps: u32,
}

impl VideoFormat {
    pub(super) fn fit(source: [u32; 2], limit: u32, fps: u32) -> Result<Self, ProbeError> {
        let longest = source[0].max(source[1]);
        let limit = limit & !1;
        if source.contains(&0) || limit < 2 || fps == 0 {
            return Err(unavailable("Invalid native video format"));
        }
        let content = if longest <= limit {
            source
        } else {
            source.map(|side| ((u64::from(side) * u64::from(limit) / u64::from(longest)) as u32).max(1))
        };
        let encoded = content.map(|side| (side + 1) & !1);
        Ok(Self { source, content, encoded, fps })
    }

    pub(super) fn bitrate(self) -> u32 {
        let pixels = u64::from(self.encoded[0]) * u64::from(self.encoded[1]);
        if pixels >= 3840 * 2160 {
            20_000_000
        } else if pixels >= 2560 * 1440 {
            12_000_000
        } else {
            8_000_000
        }
    }

    /// The configured mean bitrate: three quarters of the peak.
    pub(super) fn mean_bitrate(self) -> u32 {
        self.bitrate() * 3 / 4
    }
}

pub(super) struct Converter {
    device: ID3D11Device,
    video: ID3D11VideoDevice,
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext1,
    fence_context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    next_fence: u64,
    textures: Vec<Arc<ID3D11Texture2D>>,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    format: VideoFormat,
    staging: Option<ID3D11Texture2D>,
    memory: Vec<Weak<[u8]>>,
}

impl Converter {
    pub(super) fn new(
        device: &ID3D11Device,
        format: VideoFormat,
        software: bool,
    ) -> Result<Self, ProbeError> {
        let [width, height] = format.source;
        let [content_width, content_height] = format.content;
        let [encoded_width, encoded_height] = format.encoded;
        let video: ID3D11VideoDevice = stage(device.cast(), "Get the D3D11 video device")?;
        let context = stage(unsafe { device.GetImmediateContext() }, "Get the D3D11 context")?;
        let video_context: ID3D11VideoContext1 =
            stage(context.cast(), "Get the D3D11 conversion context")?;
        let fence_device: ID3D11Device5 = stage(device.cast(), "Get the GPU fence interface")?;
        let fence_context: ID3D11DeviceContext4 = stage(context.cast(), "Get the GPU fence context")?;
        let mut fence: Option<ID3D11Fence> = None;
        stage(
            unsafe { fence_device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) },
            "Create the GPU fence",
        )?;
        let fence = fence.ok_or_else(|| unavailable("D3D11 did not return a GPU fence"))?;

        let rate = DXGI_RATIONAL { Numerator: format.fps, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: encoded_width,
            OutputHeight: encoded_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = stage(
            unsafe { video.CreateVideoProcessorEnumerator(&content) },
            "Check D3D11 video processor support",
        )?;
        let processor = stage(
            unsafe { video.CreateVideoProcessor(&enumerator, 0) },
            "Create the D3D11 video processor",
        )?;
        let source_rect = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
        let dest_rect = RECT { left: 0, top: 0, right: content_width as i32, bottom: content_height as i32 };
        let target_rect = RECT { left: 0, top: 0, right: encoded_width as i32, bottom: encoded_height as i32 };
        let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 {
            RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 },
        }};
        // The processor belongs to this recording and its geometry is fixed.
        unsafe {
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            video_context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target_rect));
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source_rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest_rect));
        }
        let staging = if software {
            Some(pixels::texture(device, encoded_width, encoded_height, DXGI_FORMAT_NV12, 0, true)?)
        } else {
            None
        };
        Ok(Self {
            device: device.clone(), video, context, video_context, fence_context, fence, next_fence: 0, textures: Vec::new(),
            enumerator, processor, format, staging, memory: Vec::new(),
        })
    }

    pub(super) fn submit(
        &mut self,
        source: &ID3D11Texture2D,
        width: i32,
        height: i32,
    ) -> Result<Option<PendingSample>, ProbeError> {
        if [width as u32, height as u32] != self.format.source {
            return Err(unavailable("The WGC frame size changed during recording"));
        }
        self.memory.retain(|entry| entry.strong_count() != 0);
        if self.memory.len() >= VIDEO_INPUTS {
            return Ok(None);
        }
        let fence_value = self.next_fence.checked_add(1)
            .filter(|value| *value < u64::MAX)
            .ok_or_else(|| unavailable("The GPU fence counter overflowed"))?;
        self.next_fence = fence_value;
        let Some(texture) = self.acquire_texture()? else { return Ok(None) };

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
            "Prepare the BGRA input for the video processor",
        )?;
        let mut output_view = None;
        stage(
            unsafe { self.video.CreateVideoProcessorOutputView(texture.as_ref(), &self.enumerator, &output_desc, Some(&mut output_view)) },
            "Prepare the NV12 output for the video processor",
        )?;
        let output_view = output_view.ok_or_else(|| unavailable("The video processor did not return an NV12 output view"))?;
        let input_view = input_view.ok_or_else(|| unavailable("The video processor did not return a BGRA input view"))?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        };
        // Both views were created above on this device, and `stream` owns the
        // input view, which is released once after the call. The caller keeps
        // `source` alive across the call; the GPU orders later writes to it
        // behind this blit on the same immediate context.
        let blit = unsafe {
            let result = self.video_context.VideoProcessorBlt(&self.processor, &output_view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result
        };
        stage(blit, "Convert BGRA to NV12 on the GPU")?;
        if let Some(staging) = &self.staging {
            unsafe { self.context.CopyResource(staging, texture.as_ref()) };
        }
        stage(unsafe { self.fence_context.Signal(&self.fence, fence_value) }, "Signal GPU video conversion completion")?;
        unsafe { self.context.Flush() };
        Ok(Some(PendingSample { texture, fence_value }))
    }

    /// The software encoder path reads converted frames back to memory.
    pub(super) fn software(&self) -> bool {
        self.staging.is_some()
    }

    pub fn ready(&self, sample: &PendingSample) -> Result<bool, ProbeError> {
        let value = unsafe { self.fence.GetCompletedValue() };
        if value == u64::MAX {
            return Err(unavailable("The GPU device became unavailable after video conversion"));
        }
        Ok(value >= sample.fence_value)
    }

    pub(super) fn input(&mut self, sample: &PendingSample) -> Result<Option<encode::Input>, ProbeError> {
        let Some(staging) = &self.staging else {
            return Ok(Some(encode::Input::Texture(Arc::clone(&sample.texture))));
        };
        let [width, height] = self.format.encoded.map(|side| side as usize);
        let rows = height + height / 2;
        let mut packed = Vec::with_capacity(width * rows);
        if !pixels::poll_rows(&self.context, staging, width, rows, |row| packed.extend_from_slice(row))? {
            return Ok(None);
        }
        let packed: Arc<[u8]> = packed.into();
        self.memory.push(Arc::downgrade(&packed));
        Ok(Some(encode::Input::Memory(packed)))
    }

    fn acquire_texture(&mut self) -> Result<Option<Arc<ID3D11Texture2D>>, ProbeError> {
        if let Some(texture) = self.textures.iter().find(|texture| Arc::strong_count(texture) == 1) {
            return Ok(Some(Arc::clone(texture)));
        }
        if self.textures.len() >= VIDEO_INPUTS {
            return Ok(None);
        }
        let texture = Arc::new(create_nv12_texture(&self.device, self.format.encoded, self.staging.is_none())?);
        self.textures.push(Arc::clone(&texture));
        Ok(Some(texture))
    }
}

pub(super) struct PendingSample {
    texture: Arc<ID3D11Texture2D>,
    fence_value: u64,
}

fn create_nv12_texture(device: &ID3D11Device, [width, height]: [u32; 2], shared: bool) -> Result<ID3D11Texture2D, ProbeError> {
    let texture_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        MiscFlags: if shared { D3D11_RESOURCE_MISC_SHARED.0 as u32 } else { 0 },
        ..Default::default()
    };
    let mut texture = None;
    stage(
        unsafe { device.CreateTexture2D(&texture_desc, None, Some(&mut texture)) },
        "Create an NV12 texture",
    )?;
    texture.ok_or_else(|| unavailable("D3D11 did not return an NV12 texture"))
}
