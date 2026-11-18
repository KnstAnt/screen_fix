use std::{borrow::Cow, sync::mpsc::SyncSender, thread, time::{Duration, Instant}};

use windows::{
    Win32::{
        Foundation::LUID,
        Graphics::{Direct3D11::*, Dxgi::{IDXGIAdapter1, Common::DXGI_FORMAT_B8G8R8A8_UNORM}},
        Media::MediaFoundation::*,
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    },
    core::{self, Interface},
};

use crate::package::metadata::RejectedEncoder;

use super::{ProbeError, ProbeKind, capture_device, encode, gpu, h264, mf, monitor, pixels, stage, unavailable};

const MICROSOFT_H264_ENCODER: core::GUID = core::GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);
const SELF_CHECK_PICTURES: u32 = 30;
const SOFTWARE_FPS: u32 = 15;
/// The poll interval while a self-check picture converts.
const PROBE_POLL: Duration = Duration::from_millis(1);
/// The self-check found a B-frame: no smaller shape of this encoder qualifies.
const B_FRAME_FOUND: &str = "The startup encoder probe found a B-frame";
/// An admission change superseded the format request while a self-check
/// picture was awaited: the selection stops without rejecting the candidate.
const PROBE_INTERRUPTED: &str = "The encoder self-check was interrupted by an admission change";

/// The error that ends a selection because its format request was superseded.
pub(super) fn interruption() -> ProbeError {
    unavailable(PROBE_INTERRUPTED).with_kind(ProbeKind::ProbeInterrupted)
}

/// No encoder qualified, or the selection was interrupted. `rejected` holds
/// every hardware candidate rejected before (V4).
pub(super) struct SelectError {
    pub last: ProbeError,
    pub rejected: Vec<RejectedEncoder>,
}

pub(super) struct Encoder {
    pub transform: IMFTransform,
    pub format: gpu::VideoFormat,
    activation: Option<IMFActivate>,
    _manager: Option<IMFDXGIDeviceManager>,
    pub device: Option<ID3D11Device>,
    /// Hardware candidates rejected before this encoder qualified.
    pub rejected: Vec<RejectedEncoder>,
}

/// A request for one self-check picture of a candidate format. The capture
/// owner converts it on its device, so the selecting video thread never uses
/// that device's immediate context (VID-12).
pub(super) struct Probe {
    pub format: gpu::VideoFormat,
    pub software: bool,
    pub reply: SyncSender<Result<encode::Input, ProbeError>>,
}

impl Encoder {
    pub(super) fn software(&self) -> bool {
        self.activation.is_none()
    }

    pub(super) fn finish(mut self) -> Result<(), ProbeError> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), ProbeError> {
        if let Some(activation) = self.activation.take() {
            stage(unsafe { activation.ShutdownObject() }, "Release the H.264 encoder")?;
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The highest frame rate `select` can qualify for a requested pace: a
/// hardware encoder runs at the requested pace, the software fallback at
/// its fixed rate. Every bound sized before selection uses this value.
pub(super) fn max_effective_fps(requested_fps: u32) -> u32 {
    requested_fps.max(SOFTWARE_FPS)
}

/// The lowest frame rate `select` can qualify: it sets the longest slot interval.
pub(super) fn min_effective_fps(requested_fps: u32) -> u32 {
    requested_fps.min(SOFTWARE_FPS)
}

/// Qualify an encoder for a display format: each hardware candidate in turn,
/// then the software encoder at its fixed rate. `probe` supplies the
/// converted self-check picture of a candidate format. An interrupted
/// self-check ends the selection at once.
pub(super) fn select(
    adapter: &IDXGIAdapter1, luid: LUID, source: [u32; 2], requested_fps: u32,
    probe: &mut dyn FnMut(gpu::VideoFormat, bool) -> Result<encode::Input, ProbeError>,
) -> Result<Encoder, SelectError> {
    let filter = discovery_filter(luid).map_err(|last| SelectError { last, rejected: Vec::new() })?;
    let mut rejected = Vec::new();
    let reject = |name: Option<String>, error: ProbeError| RejectedEncoder {
        name, cause: Cow::Borrowed(error.stage), native_code: error.source.as_ref().map(|source| source.code().0),
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let candidates = mf::enum_on_adapter(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        &input, &output, &filter).unwrap_or_else(|error| {
            rejected.push(reject(None, error));
            Vec::new()
        });

    // Probe native shape first. Lower shapes are eligible only after the
    // higher shape failed configuration or the actual encode check.
    if !candidates.is_empty() {
        match capture_device(adapter) {
            Ok(device) => for candidate in &candidates {
                match qualify(source, requested_fps, Some((candidate, &device)), probe) {
                    Ok(mut encoder) => {
                        encoder.rejected = rejected;
                        return Ok(encoder);
                    }
                    Err(error) if error.kind() == ProbeKind::ProbeInterrupted => return Err(SelectError { last: error, rejected }),
                    Err(error) => rejected.push(reject(friendly_name(candidate), error)),
                }
            },
            Err(error) => rejected.push(reject(None, error)),
        }
    }
    match qualify(source, SOFTWARE_FPS, None, probe) {
        Ok(mut encoder) => {
            encoder.rejected = rejected;
            Ok(encoder)
        }
        Err(last) => Err(SelectError { last, rejected }),
    }
}

/// Encoder discovery attributes restricted to the display adapter.
fn discovery_filter(luid: LUID) -> Result<IMFAttributes, ProbeError> {
    let attributes = mf::attributes(1)?;
    // Serialize the two 32-bit fields of the Windows LUID in ABI order.
    let mut luid_bytes = [0; 8];
    luid_bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    luid_bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    stage(unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes) },
        "Restrict encoder discovery to the display adapter")?;
    Ok(attributes)
}

fn friendly_name(candidate: &IMFActivate) -> Option<String> {
    let length = unsafe { candidate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) }.ok().filter(|&length| length <= 256)?;
    let mut name = vec![0_u16; length as usize + 1];
    unsafe { candidate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name, None) }.ok()?;
    Some(String::from_utf16_lossy(&name[..length as usize]))
}

fn qualify(
    source: [u32; 2], fps: u32, hardware: Option<(&IMFActivate, &ID3D11Device)>,
    probe: &mut dyn FnMut(gpu::VideoFormat, bool) -> Result<encode::Input, ProbeError>,
) -> Result<Encoder, ProbeError> {
    let native_limit = (source[0].max(source[1]).min(4096) + 1) & !1;
    let mut low = 2;
    let mut high = native_limit;
    let mut candidate = high;
    let mut best = None;
    let mut last_error = unavailable("No H.264 encoder accepted the display format");
    while low <= high {
        let format = gpu::VideoFormat::fit(source, candidate, fps)?;
        match configure(hardware, format) {
            Ok(encoder) => {
                let check = probe(format, encoder.software()).and_then(|input| self_check(&encoder, input));
                match check {
                    Ok(()) if candidate == native_limit => return Ok(encoder),
                    Ok(()) => {
                        // Drop the earlier successful instance before opening
                        // another hardware encoder session on this adapter.
                        best = Some(format);
                        low = candidate + 2;
                    }
                    Err(error) => {
                        if matches!(error.kind(), ProbeKind::BFrameFound | ProbeKind::ProbeInterrupted) {
                            return Err(error);
                        }
                        last_error = error;
                        high = candidate.saturating_sub(2);
                    }
                }
            }
            Err(error) => {
                last_error = error;
                high = candidate.saturating_sub(2);
            }
        }
        candidate = (low + (high.saturating_sub(low) / 2)) & !1;
    }
    let format = best.ok_or(last_error)?;
    let encoder = configure(hardware, format)?;
    self_check(&encoder, probe(format, encoder.software())?)?;
    Ok(encoder)
}

fn configure(
    hardware: Option<(&IMFActivate, &ID3D11Device)>, format: gpu::VideoFormat,
) -> Result<Encoder, ProbeError> {
    let transform = match hardware {
        Some((candidate, _)) => stage(unsafe { candidate.ActivateObject() }, "Activate the adapter H.264 encoder")?,
        None => stage(unsafe { CoCreateInstance(&MICROSOFT_H264_ENCODER, None, CLSCTX_INPROC_SERVER) },
            "Activate the Microsoft software H.264 encoder")?,
    };
    let mut encoder = Encoder {
        transform, format, activation: hardware.map(|(activation, _)| activation.clone()),
        _manager: None, device: hardware.map(|(_, device)| device.clone()), rejected: Vec::new(),
    };
    if let Some((_, device)) = hardware {
        let attributes = stage(unsafe { encoder.transform.GetAttributes() }, "Read the hardware H.264 encoder properties")?;
        let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if asynchronous {
            stage(unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }, "Unlock the asynchronous encoder")?;
        }
        if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
            return Err(unavailable("The adapter H.264 encoder cannot receive D3D11 surfaces"));
        }
        let (token, manager) = mf::device_manager()?;
        stage(unsafe { manager.ResetDevice(device, token) }, "Attach the H.264 encoder D3D11 device")?;
        stage(unsafe { encoder.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) },
            "Pass the D3D11 device to the adapter H.264 encoder")?;
        encoder._manager = Some(manager);
    }

    encode::set_properties(&encoder.transform, format.fps, format.bitrate())?;
    let output = video_type(&MFVideoFormat_H264, format)?;
    stage(unsafe { output.SetUINT32(&MF_MT_AVG_BITRATE, format.mean_bitrate()) }, "Set the H.264 mean bitrate")?;
    stage(unsafe { output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32) }, "Set the H.264 High profile")?;
    stage(unsafe { encoder.transform.SetOutputType(0, &output, 0) }, "Negotiate H.264 output")?;
    let input = video_type(&MFVideoFormat_NV12, format)?;
    stage(unsafe { encoder.transform.SetInputType(0, &input, 0) }, "Negotiate NV12 input")?;
    Ok(encoder)
}

/// Convert one self-check picture of `format` on the capture device. Only the
/// owner of that device's immediate context calls this.
pub(super) fn probe_input(device: &ID3D11Device, format: gpu::VideoFormat, software: bool) -> Result<encode::Input, ProbeError> {
    let [width, height] = format.source;
    let mut converter = gpu::Converter::new(device, format, software)?;
    let source = pixels::texture(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_BIND_RENDER_TARGET.0 as u32, false)?;
    let mut target: Option<ID3D11RenderTargetView> = None;
    stage(unsafe { device.CreateRenderTargetView(&source, None, Some(&mut target)) }, "Create the startup encoder probe surface")?;
    let target = target.ok_or_else(|| unavailable("The startup encoder probe surface is unavailable"))?;
    let context = stage(unsafe { device.GetImmediateContext() }, "Get the startup encoder probe context")?;
    unsafe { context.ClearRenderTargetView(&target, &[0.12, 0.30, 0.70, 1.0]) };
    let pending = converter.submit(&source, width as i32, height as i32)?
        .ok_or_else(|| unavailable("The startup encoder probe could not allocate an NV12 surface"))?;
    let started = Instant::now();
    let input = loop {
        if converter.ready(&pending)? && let Some(input) = converter.input(&pending)? {
            break input;
        }
        if started.elapsed() >= monitor::CONVERSION_DEADLINE {
            return Err(unavailable("The startup encoder probe conversion timed out"));
        }
        thread::sleep(PROBE_POLL);
    };
    Ok(input)
}

fn self_check(encoder: &Encoder, input: encode::Input) -> Result<(), ProbeError> {
    let format = encoder.format;

    struct Check {
        sent: u32,
        received: u32,
        headers: h264::Headers,
    }
    let mut probe = Check { sent: 0, received: 0, headers: h264::Headers::default() };
    let ticks = |index: u32| i64::from(index) * 10_000_000 / i64::from(format.fps);
    let pace_origin = Instant::now();
    let submitted = encode::record(&encoder.transform, encoder.device.as_ref(), &mut probe, |_| Ok(false), |probe| {
        if probe.sent == SELF_CHECK_PICTURES {
            return Ok(encode::Next::End);
        }
        let index = probe.sent;
        let pts = ticks(index);
        if pace_origin.elapsed() < Duration::from_nanos(pts as u64 * 100) {
            return Ok(encode::Next::Wait);
        }
        probe.sent += 1;
        Ok(encode::Next::Frame { input: input.clone(), pts,
            duration: ticks(index + 1) - pts, force_idr: index == SELF_CHECK_PICTURES / 2 })
    }, |_| Ok(()), |probe, transform, sample| {
        let index = probe.received;
        if index >= SELF_CHECK_PICTURES || stage(unsafe { sample.GetSampleTime() }, "Read the startup H.264 PTS")? != ticks(index) {
            return Err(unavailable("The startup encoder probe did not preserve the submitted picture order"));
        }
        if index == 0 {
            let media_type = stage(unsafe { transform.GetOutputCurrentType(0) }, "Read the startup H.264 media type")?;
            let video_size = stage(unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }, "Read the qualified H.264 dimensions")?;
            let rate = stage(unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }, "Read the qualified H.264 frame rate")?;
            if video_size != ((u64::from(format.encoded[0]) << 32) | u64::from(format.encoded[1]))
                || rate != ((u64::from(format.fps) << 32) | 1)
            {
                return Err(unavailable("The H.264 probe returned a different size or frame rate"));
            }
            probe.headers.read_media_type(&media_type)?;
        }
        let picture = probe.headers.inspect(sample)?;
        if picture.b_slice {
            return Err(unavailable(B_FRAME_FOUND).with_kind(ProbeKind::BFrameFound));
        }
        if index == 0 || index == SELF_CHECK_PICTURES / 2 {
            probe.headers.segment_header(&picture)?;
        }
        probe.received += 1;
        Ok(1)
    })?;
    if submitted != SELF_CHECK_PICTURES as usize || probe.received != SELF_CHECK_PICTURES {
        return Err(unavailable("The startup encoder probe did not encode all 30 submitted pictures"));
    }
    // The same negotiated instance is reused after resetting the probe prefix.
    stage(unsafe { encoder.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }, "Clear the startup encoder probe prefix")
}

fn video_type(subtype: &core::GUID, format: gpu::VideoFormat) -> Result<IMFMediaType, ProbeError> {
    let media_type = mf::media_type()?;
    // Media Foundation packs the first UINT32 into the high half.
    stage((|| unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(format.encoded[0]) << 32) | u64::from(format.encoded[1]))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(format.fps) << 32) | 1)?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1)?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_22.0 as u32)?;
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
    })(), "Configure the native video format")?;
    Ok(media_type)
}

