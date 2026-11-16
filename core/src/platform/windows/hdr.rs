use std::{mem::size_of, num::NonZeroU32};

use windows::{
    Win32::Graphics::{Direct3D11::*, Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT},
    core::s,
};

use crate::capture::frame::FrameExtent;

use super::{ProbeError, pixels::{self, Painter}, stage, unavailable};

pub(super) const TONE_MAP: &str = "scrgb_sdr_white_clamp_srgb_v1";

const SHADER: &str = r#"
Texture2D<float4> source_pixels : register(t0);
cbuffer SdrWhite : register(b0) {
    float reference_white;
};
float encode_srgb(float v) {
    return v <= 0.0031308 ? 12.92 * v : 1.055 * pow(v, 1.0 / 2.4) - 0.055;
}
float4 tone_map(float4 position : SV_Position) : SV_Target {
    // WGC floating-point desktop colors are linear scRGB, with 1.0 at 80 nits.
    float3 rgb = saturate(source_pixels.Load(int3(int2(position.xy), 0)).rgb / reference_white);
    return float4(encode_srgb(rgb.r), encode_srgb(rgb.g), encode_srgb(rgb.b), 1.0);
}
"#;

pub(super) struct ToneMap {
    pub scratch: ID3D11Texture2D,
    shader: ID3D11PixelShader,
    constants: ID3D11Buffer,
}

impl ToneMap {
    pub fn new(
        device: &ID3D11Device, painter: &Painter, extent: FrameExtent, sdr_white: NonZeroU32,
    ) -> Result<Self, ProbeError> {
        let scratch = pixels::texture(device, extent.width.get(), extent.height.get(),
            DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE.0 as u32, false)?;
        let shader = painter.pixel(SHADER, s!("tone_map"))?;
        // DISPLAYCONFIG_SDR_WHITE_LEVEL is a ratio to 80 nits in units of 1/1000.
        // D3D11 constant buffers are rounded to a 16-byte register.
        let values = [sdr_white.get() as f32 / 1000.0, 0.0, 0.0, 0.0];
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<[f32; 4]>() as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA { pSysMem: values.as_ptr().cast(), ..Default::default() };
        let mut constants = None;
        stage(unsafe { device.CreateBuffer(&desc, Some(&data), Some(&mut constants)) },
            "Create the HDR SDR-white shader constants")?;
        let constants = constants.ok_or_else(|| unavailable("D3D11 did not return the HDR shader constants"))?;
        Ok(Self { scratch, shader, constants })
    }

    pub fn convert(
        &self, painter: &Painter, context: &ID3D11DeviceContext, target: &ID3D11Texture2D, extent: FrameExtent,
    ) -> Result<(), ProbeError> {
        painter.draw(context, &self.shader, &self.scratch, target,
            extent.width.get(), extent.height.get(), Some(&self.constants))
    }
}
