use std::collections::VecDeque;

use windows::{
    Win32::Graphics::{
        Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob, ID3DInclude},
        Direct3D11::*,
        Dxgi::{DXGI_ERROR_WAS_STILL_DRAWING, Common::{DXGI_FORMAT, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC}},
    },
    core::{PCSTR, s},
};

use crate::protocol::FrameId;

use super::{ProbeError, stage, unavailable};

const THUMB_WIDTH: u32 = 256;
const THUMB_HEIGHT: u32 = 144;
const THUMB_SLOTS: usize = 3;

const DRAW: &str = r#"
float4 cover(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv.x * 2 - 1, 1 - uv.y * 2, 0, 1);
}
Texture2D<float4> source_pixels : register(t0);
float luma(float4 position : SV_Position) : SV_Target {
    uint2 source_size;
    source_pixels.GetDimensions(source_size.x, source_size.y);
    uint2 cell = uint2(position.xy);
    uint2 target_size = uint2(256, 144);
    uint2 begin = cell * source_size / target_size;
    uint2 end = max(begin + 1, (cell + 1) * source_size / target_size);
    float sum = 0;
    for (uint y = begin.y; y < end.y; ++y) {
        for (uint x = begin.x; x < end.x; ++x) {
            sum += dot(source_pixels.Load(int3(x, y, 0)).rgb, float3(0.2126, 0.7152, 0.0722));
        }
    }
    uint2 extent = end - begin;
    return sum / (extent.x * extent.y);
}
"#;

pub(super) struct Painter {
    device: ID3D11Device,
    deferred: ID3D11DeviceContext,
    cover: ID3D11VertexShader,
}

impl Painter {
    pub fn new(device: &ID3D11Device) -> Result<Self, ProbeError> {
        let mut deferred = None;
        stage(unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }, "Create the capture shader context")?;
        let deferred = deferred.ok_or_else(|| unavailable("D3D11 did not return the capture shader context"))?;
        let bytecode = compile(DRAW, s!("cover"), s!("vs_5_0"))?;
        let mut cover = None;
        stage(unsafe { device.CreateVertexShader(blob(&bytecode), None::<&ID3D11ClassLinkage>, Some(&mut cover)) },
            "Create the full-screen capture vertex shader")?;
        let cover = cover.ok_or_else(|| unavailable("D3D11 did not return the capture vertex shader"))?;
        Ok(Self { device: device.clone(), deferred, cover })
    }

    pub fn pixel(&self, source: &str, entry: PCSTR) -> Result<ID3D11PixelShader, ProbeError> {
        let bytecode = compile(source, entry, s!("ps_5_0"))?;
        let mut shader = None;
        stage(unsafe { self.device.CreatePixelShader(blob(&bytecode), None::<&ID3D11ClassLinkage>, Some(&mut shader)) },
            "Create the capture pixel shader")?;
        shader.ok_or_else(|| unavailable("D3D11 did not return the capture pixel shader"))
    }

    pub fn draw(
        &self, immediate: &ID3D11DeviceContext, shader: &ID3D11PixelShader,
        source: &ID3D11Texture2D, target: &ID3D11Texture2D, width: u32, height: u32,
        constants: Option<&ID3D11Buffer>,
    ) -> Result<(), ProbeError> {
        let mut input = None;
        stage(unsafe { self.device.CreateShaderResourceView(source, None, Some(&mut input)) },
            "Create the capture shader input view")?;
        let mut output = None;
        stage(unsafe { self.device.CreateRenderTargetView(target, None, Some(&mut output)) },
            "Create the capture shader output view")?;
        if input.is_none() || output.is_none() {
            return Err(unavailable("D3D11 did not return a capture shader view"));
        }
        let viewport = D3D11_VIEWPORT {
            Width: width as f32, Height: height as f32, MaxDepth: 1.0, ..Default::default()
        };
        let mut commands = None;
        // Record graphics state locally, then restore the shared immediate context
        // after submitting this command list. All GPU operations originate on capture.
        unsafe {
            self.deferred.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.deferred.VSSetShader(&self.cover, None);
            self.deferred.PSSetShader(shader, None);
            self.deferred.PSSetConstantBuffers(0, Some(&[constants.cloned()]));
            self.deferred.PSSetShaderResources(0, Some(&[input]));
            self.deferred.RSSetViewports(Some(&[viewport]));
            self.deferred.OMSetRenderTargets(Some(&[output]), None::<&ID3D11DepthStencilView>);
            self.deferred.Draw(3, 0);
            stage(self.deferred.FinishCommandList(false, Some(&mut commands)), "Finish capture shader commands")?;
            let commands = commands.ok_or_else(|| unavailable("D3D11 did not return capture shader commands"))?;
            immediate.ExecuteCommandList(&commands, true);
        }
        Ok(())
    }
}

fn compile(source: &str, entry: PCSTR, profile: PCSTR) -> Result<ID3DBlob, ProbeError> {
    let mut code = None;
    stage(unsafe {
        D3DCompile(source.as_ptr().cast(), source.len(), PCSTR::null(), None, None::<&ID3DInclude>,
            entry, profile, 0, 0, &mut code, None)
    }, "Compile the built-in capture GPU shader")?;
    code.ok_or_else(|| unavailable("D3D11 did not return capture shader bytecode"))
}

fn blob(code: &ID3DBlob) -> &[u8] {
    // The shader consumes this slice during creation while the compiler blob is alive.
    unsafe { std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize()) }
}

pub(super) fn texture(
    device: &ID3D11Device, width: u32, height: u32, format: DXGI_FORMAT, bind: u32, staging: bool,
) -> Result<ID3D11Texture2D, ProbeError> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: width, Height: height, MipLevels: 1, ArraySize: 1, Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: bind,
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        ..Default::default()
    };
    let mut result = None;
    stage(unsafe { device.CreateTexture2D(&description, None, Some(&mut result)) }, "Allocate a capture GPU texture")?;
    result.ok_or_else(|| unavailable("D3D11 did not return a capture GPU texture"))
}

pub(super) fn poll_rows(
    context: &ID3D11DeviceContext, staging: &ID3D11Texture2D,
    row_bytes: usize, height: usize,
    mut row: impl FnMut(&[u8]),
) -> Result<bool, ProbeError> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let map = unsafe { context.Map(staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut mapped)) };
    if map.as_ref().err().is_some_and(|error| error.code() == DXGI_ERROR_WAS_STILL_DRAWING) {
        return Ok(false);
    }
    stage(map, "Map the capture staging texture")?;
    let _mapped = Mapped { context, staging };
    let pitch = mapped.RowPitch as usize;
    let extent = height.checked_sub(1).and_then(|last| last.checked_mul(pitch))
        .and_then(|offset| offset.checked_add(row_bytes))
        .filter(|size| *size <= isize::MAX as usize)
        .ok_or_else(|| unavailable("The capture staging extent overflowed"))?;
    if mapped.pData.is_null() || pitch < row_bytes || (mapped.DepthPitch != 0 && extent > mapped.DepthPitch as usize) {
        return Err(unavailable("The capture staging layout is invalid"));
    }
    for index in 0..height {
        // Pitch and mapped dimensions were checked. Padding never leaves the owner.
        row(unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(index * pitch), row_bytes) });
    }
    Ok(true)
}

struct Mapped<'a> {
    context: &'a ID3D11DeviceContext,
    staging: &'a ID3D11Texture2D,
}

impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe { self.context.Unmap(self.staging, 0) };
    }
}

pub(super) struct Thumbnails {
    shader: ID3D11PixelShader,
    target: ID3D11Texture2D,
    available: Vec<ID3D11Texture2D>,
    pending: VecDeque<(FrameId, ID3D11Texture2D)>,
    previous: Option<(FrameId, Vec<u8>)>,
}

impl Thumbnails {
    pub fn new(device: &ID3D11Device, painter: &Painter) -> Result<Self, ProbeError> {
        let shader = painter.pixel(DRAW, s!("luma"))?;
        let target = texture(device, THUMB_WIDTH, THUMB_HEIGHT, DXGI_FORMAT_R8_UNORM, D3D11_BIND_RENDER_TARGET.0 as u32, false)?;
        let mut available = Vec::with_capacity(THUMB_SLOTS);
        for _ in 0..THUMB_SLOTS {
            available.push(texture(device, THUMB_WIDTH, THUMB_HEIGHT, DXGI_FORMAT_R8_UNORM, 0, true)?);
        }
        Ok(Self { shader, target, available, pending: VecDeque::with_capacity(THUMB_SLOTS), previous: None })
    }

    pub fn submit(
        &mut self, painter: &Painter, context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D, frame: FrameId,
    ) -> Result<bool, ProbeError> {
        let Some(staging) = self.available.pop() else { return Ok(false) };
        painter.draw(context, &self.shader, source, &self.target, THUMB_WIDTH, THUMB_HEIGHT, None)?;
        unsafe {
            context.CopyResource(&staging, &self.target);
            context.Flush();
        }
        self.pending.push_back((frame, staging));
        Ok(true)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn poll(&mut self, context: &ID3D11DeviceContext) -> Result<Option<(FrameId, Option<f64>)>, ProbeError> {
        let Some(&(frame, ref staging)) = self.pending.front() else { return Ok(None) };
        let mut luma = Vec::new();
        if !poll_rows(context, staging, THUMB_WIDTH as usize, THUMB_HEIGHT as usize, |row| {
            luma.extend_from_slice(row);
        })? {
            return Ok(None);
        }
        let (_, stage) = self.pending.pop_front().ok_or_else(|| unavailable("The thumbnail staging queue is empty"))?;
        self.available.push(stage);
        let change = self.previous.take().and_then(|(previous_id, previous)| {
            if previous_id.epoch != frame.epoch || previous_id.sequence.0.checked_add(1) != Some(frame.sequence.0) {
                return None;
            }
            let difference: u32 = previous.iter().zip(&luma).map(|(a, b)| u32::from(a.abs_diff(*b))).sum();
            Some(f64::from(difference) / f64::from(255 * THUMB_WIDTH * THUMB_HEIGHT))
        });
        self.previous = Some((frame, luma));
        Ok(Some((frame, change)))
    }
}
