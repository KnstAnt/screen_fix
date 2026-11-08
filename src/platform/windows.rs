mod gpu;
mod encode;
mod mp4;
mod input;
mod foreground;
mod clipboard;
mod export;
mod drag;

use std::{fmt, ptr, sync::Arc, time::{Duration, Instant}};

use windows::{
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
            },
            Dxgi::{CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1},
        },
        Media::MediaFoundation::*,
        System::{
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Com::{CoTaskMemFree, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
    },
    core::{self, Interface},
};

pub(crate) struct ProbeError {
    stage: &'static str,
    source: Option<core::Error>,
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.stage)?;
        if let Some(source) = &self.source {
            write!(f, " ({:#010x}): {source}", source.code().0 as u32)?;
        }
        Ok(())
    }
}

fn stage<T>(result: core::Result<T>, name: &'static str) -> Result<T, ProbeError> {
    result.map_err(|source| ProbeError { stage: name, source: Some(source) })
}

fn unavailable(stage: &'static str) -> ProbeError {
    ProbeError { stage, source: None }
}

struct Apartment;

impl Apartment {
    fn initialize() -> Result<Self, ProbeError> {
        stage(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }, "Initialize COM")?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // This guard stays on the thread that initialized COM.
        unsafe { CoUninitialize() };
    }
}

struct MediaFoundation;

impl MediaFoundation {
    fn initialize() -> Result<Self, ProbeError> {
        stage(unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }, "Start Media Foundation")?;
        Ok(Self)
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // The probe releases Media Foundation objects before this guard.
        let _ = unsafe { MFShutdown() };
    }
}

pub(crate) fn record(stop: &crate::session_gate::SessionGate, options: &crate::platform::RecordingOptions) -> Result<String, ProbeError> {
    let _apartment = Apartment::initialize()?;
    let supported = stage(GraphicsCaptureSession::IsSupported(), "Check screen capture support")?;
    if !supported {
        return Err(unavailable("Windows screen capture is unavailable"));
    }

    let (adapter, display) = outputs()?.into_iter()
        .find(|(_, display)| display_id(display) == options.display_id)
        .ok_or_else(|| unavailable("The selected display is no longer available"))?;
    let monitor = display.Monitor;
    let interop = stage(core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>(),
        "Get the display capture API")?;
    let item: GraphicsCaptureItem = stage(unsafe { interop.CreateForMonitor(monitor) },
        "Select the display to capture")?;
    let size = stage(item.Size(), "Read the display size")?;

    let description = stage(unsafe { adapter.GetDesc1() }, "Read the display adapter")?;
    let mut device: Option<ID3D11Device> = None;
    // With an explicit adapter D3D11 requires DRIVER_TYPE_UNKNOWN.
    stage(unsafe {
        D3D11CreateDevice(
            &adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None, D3D11_SDK_VERSION, Some(&mut device), None, None,
        )
    }, "Create the D3D11 video device")?;
    let device = device.ok_or_else(|| unavailable("D3D11 did not return a device"))?;
    let context = stage(unsafe { device.GetImmediateContext() }, "Get the D3D11 context")?;
    let multithread: ID3D11Multithread = stage(context.cast(), "Protect the shared D3D11 context")?;
    // The async MFT and the converter can use the same immediate context.
    unsafe { multithread.SetMultithreadProtected(true) };


    let _media_foundation = MediaFoundation::initialize()?;
    let mut attributes = None;
    stage(unsafe { MFCreateAttributes(&mut attributes, 1) }, "Create the encoder filter")?;
    let attributes = attributes.ok_or_else(|| unavailable("Media Foundation did not return the encoder filter"))?;
    let luid = description.AdapterLuid;
    // Serialize the two 32-bit fields of the Windows LUID in ABI order.
    let mut luid_bytes = [0; 8];
    luid_bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    luid_bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    stage(unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes) },
        "Restrict encoder discovery to the display adapter")?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut entries = ptr::null_mut();
    let mut count = 0;
    stage(unsafe {
        MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input), Some(&output), &attributes, &mut entries, &mut count)
    }, "Find a hardware H.264 encoder")?;
    // On success Media Foundation transfers the array and each COM reference.
    let mut candidates = Vec::new();
    if !entries.is_null() {
        for index in 0..count as usize {
            let entry = unsafe { ptr::read(entries.add(index)) };
            candidates.extend(entry);
        }
        unsafe { CoTaskMemFree(Some(entries.cast())) };
    }
    let mut last_error = unavailable("No hardware NV12-to-H.264 encoder was found on the display adapter");
    let mut configured = None;
    for candidate in candidates {
        match configure_encoder(&candidate, &device) {
            Ok(encoder) => {
                configured = Some(encoder);
                break;
            }
            Err(error) => last_error = error,
        }
    }
    let encoder = configured.ok_or(last_error)?;
    let rect = display.DesktopCoordinates;
    if (rect.right - rect.left, rect.bottom - rect.top) != (size.Width, size.Height) {
        return Err(unavailable("Desktop geometry does not match the WGC source size"));
    }
    let geometry = mp4::Geometry {
        display_id: options.display_id.clone(),
        desktop_rect: [rect.left, rect.top, rect.right, rect.bottom],
        source_size: [size.Width, size.Height],
        video_size: [gpu::WIDTH, gpu::HEIGHT],
    };
    let (count, recording, index) = capture_recording(
        &item, &device, size, &encoder.transform, stop, options, geometry,
    )?;
    encoder.finish()?;
    if count == 0 {
        return Ok("Recording stopped before the first frame.".into());
    }
    let recording = recording.ok_or_else(|| unavailable("The encoder did not produce samples for the MP4"))?;
    let output = recording.finish(count, index.ok_or_else(|| unavailable("The frame index is missing"))?)?;
    let folder = output.parent().ok_or_else(|| unavailable("The recording directory is missing"))?;
    export::run(folder, false).map_err(|error| mp4::io_error(
        "Export PNG images",
        std::io::Error::other(format!("{error}. The original recording was saved to {}", output.display())),
    ))?;
    Ok(format!("MP4, index, and export saved. Frames: {count}.\n{}", output.display()))
}

pub(crate) fn export_session(folder: &std::path::Path) -> Result<std::path::PathBuf, ProbeError> {
    let _apartment = Apartment::initialize()?;
    let _media_foundation = MediaFoundation::initialize()?;
    export::run(folder, true)
}

fn capture_recording(
    item: &GraphicsCaptureItem,
    device: &ID3D11Device,
    size: windows::Graphics::SizeInt32,
    encoder: &IMFTransform,
    stop: &crate::session_gate::SessionGate,
    options: &crate::platform::RecordingOptions,
    geometry: mp4::Geometry,
) -> Result<(usize, Option<mp4::Recording>, Option<mp4::FrameIndex>), ProbeError> {
    let dxgi: IDXGIDevice = stage(device.cast(), "Get the DXGI device")?;
    let winrt = stage(
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) },
        "Create the WinRT capture device",
    )?;
    let winrt: IDirect3DDevice = stage(winrt.cast(), "Get the WinRT D3D11 interface")?;
    let pool = stage(
        Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size,
        ),
        "Create the WGC frame pool",
    )?;
    let result = gpu::Converter::new(device, size.Width, size.Height)
        .and_then(|mut converter| record_session(&pool, item, &mut converter, encoder, stop, options, geometry));
    let close = stage(pool.Close(), "Close the WGC frame pool");
    result.and_then(|frame| close.map(|()| frame))
}

fn record_session(
    pool: &Direct3D11CaptureFramePool,
    item: &GraphicsCaptureItem,
    converter: &mut gpu::Converter<'_>,
    encoder: &IMFTransform,
    stop: &crate::session_gate::SessionGate,
    options: &crate::platform::RecordingOptions,
    geometry: mp4::Geometry,
) -> Result<(usize, Option<mp4::Recording>, Option<mp4::FrameIndex>), ProbeError> {
    let session = stage(pool.CreateCaptureSession(item), "Create the WGC session")?;
    let result = (|| {
        stage(session.SetIsCursorCaptureEnabled(false),
            "Disable cursor capture (requires Windows 10 version 2004 or later)")?;
        let mut frame_index = mp4::FrameIndex::create(&options.folder, geometry)?;
        let folder = frame_index.folder().to_path_buf();
        let ((count, recording), event_count) = input::with_journal(&folder, stop, options.clipboard, || {
            let (start, qpc, qpc_frequency, qpc_uncertainty_100ns) = session_clock()?;
            if !stop.open(start) {
                return Ok((0, None));
            }
            let wgc_start = i128::from(qpc) * 10_000_000 / i128::from(qpc_frequency);
            stage(session.StartCapture(), "Start WGC capture")?;
            let mut stop_offset = None;
            let mut observation_id = 0_u64;
            let mut previous: Option<(Arc<ID3D11Texture2D>, i64, u64, [u8; 32])> = None;
            let mut recording = None;
            let mut pending = None;
            let mut origin = None;
            let mut stopped = false;
            let mut scheduled = start;
            let mut next_texture = |scheduled: Instant| -> Result<Result<(Arc<ID3D11Texture2D>, i64, u64, [u8; 32], bool, Instant), Instant>, ProbeError> {
                if let Some(at) = stop.wait_until(scheduled) {
                    return Ok(Err(at));
                }
                loop {
                    match try_next_frame(pool) {
                        Ok(Some(mut frame)) => {
                            // The pool has two buffers. Prefer the newer queued
                            // observation and release the older lease before conversion.
                            match try_next_frame(pool) {
                                Ok(Some(newer)) => {
                                    stage(frame.Close(), "Release the older WGC frame")?;
                                    frame = newer;
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    let _ = frame.Close();
                                    return stage(Err(error), "Get the next WGC frame");
                                }
                            }
                            let wgc_time = stage(frame.SystemRelativeTime(), "Read the WGC frame timestamp")?.Duration;
                            if i128::from(wgc_time) < wgc_start {
                                stage(frame.Close(), "Release the WGC frame captured before Start")?;
                                if let Some(at) = stop.wait_until(Instant::now()) {
                                    return Ok(Err(at));
                                }
                                continue;
                            }
                            let at = match stop.admit() {
                                Ok(at) => at,
                                Err(at) => {
                                    stage(frame.Close(), "Release the WGC frame received after Stop")?;
                                    return Ok(Err(at));
                                }
                            };
                            match inspect_frame(&frame, converter) {
                                Ok((texture, fingerprint)) => {
                                    stage(frame.Close(), "Return the frame to the WGC pool")?;
                                    observation_id = observation_id.checked_add(1)
                                        .ok_or_else(|| unavailable("The WGC observation counter overflowed"))?;
                                    previous = Some((texture.clone(), wgc_time, observation_id, fingerprint));
                                    return Ok(Ok((texture, wgc_time, observation_id, fingerprint, false, at)));
                                }
                                Err(error) => {
                                    let _ = session.Close();
                                    let _ = pool.Close();
                                    let _ = frame.Close();
                                    return Err(error);
                                }
                            }
                        }
                        Ok(None) => {
                            if let Some((texture, wgc_time, id, fingerprint)) = &previous {
                                return Ok(stop.admit().map(|at| (texture.clone(), *wgc_time, *id, *fingerprint, true, at)));
                            }
                            if start.elapsed() >= Duration::from_secs(3) {
                                return Err(unavailable("No WGC frame arrived within 3 seconds"));
                            }
                            if let Some(at) = stop.wait_until(Instant::now() + Duration::from_millis(10)) {
                                return Ok(Err(at));
                            }
                        }
                        Err(error) => return stage(Err(error), "Get a WGC frame"),
                    }
                }
            };
            let count = encode::record(encoder, &mut frame_index, |index| {
                if stopped {
                    return Ok(None);
                }
                loop {
                    let boundary = match next_texture(scheduled)? {
                        Ok((texture, wgc_time, observation_id, fingerprint, repeated, observed_at)) => {
                            scheduled = observed_at + Duration::from_nanos(1_000_000_000 / 30);
                            let prior = pending.replace((texture, wgc_time, observation_id, fingerprint, repeated, observed_at));
                            if let Some((texture, wgc_time, observation_id, fingerprint, repeated, at)) = prior {
                                (texture, wgc_time, observation_id, fingerprint, repeated, at, observed_at)
                            } else {
                                origin = Some(observed_at);
                                continue;
                            }
                        }
                        Err(stop_at) => {
                            stopped = true;
                            stop_offset = Some(video_ticks(stop_at.saturating_duration_since(start))?);
                            let Some((texture, wgc_time, observation_id, fingerprint, repeated, at)) = pending.take() else {
                                return Ok(None);
                            };
                            (texture, wgc_time, observation_id, fingerprint, repeated, at, stop_at)
                        }
                    };
                    let (texture, wgc_time_100ns, observation_id, fingerprint, repeated, at, end) = boundary;
                    let origin = origin.ok_or_else(|| unavailable("The video time origin is missing"))?;
                    let pts = video_ticks(at.saturating_duration_since(origin))?;
                    let end = video_ticks(end.saturating_duration_since(origin))?;
                    if end <= pts {
                        if stopped {
                            return Ok(None);
                        }
                        return Err(unavailable("Video frame timestamps are not strictly increasing"));
                    }
                    index.push(&mp4::FrameTiming {
                            wgc_time_100ns, observation_id, fingerprint, repeated, pts_100ns: pts, duration_100ns: end - pts,
                            admitted_100ns: video_ticks(at.saturating_duration_since(start))?,
                        })?;
                    return Ok(Some((texture, pts, end - pts)));
                }
            }, |index, encoder, sample| {
                if recording.is_none() {
                    let media_type = stage(unsafe { encoder.GetOutputCurrentType(0) }, "Read the H.264 output format")?;
                    let folder = index.folder();
                    recording = Some(mp4::Recording::open(&media_type, folder)?);
                }
                recording.as_ref().ok_or_else(|| unavailable("The MP4 writer is not open"))?.write(sample)
            })?;
            if count != 0 {
                let origin = origin.ok_or_else(|| unavailable("The video time origin is missing"))?;
                frame_index.set_timeline(mp4::Timeline {
                    qpc_at_start: qpc,
                    qpc_frequency,
                    qpc_uncertainty_100ns,
                    video_origin_100ns: video_ticks(origin.saturating_duration_since(start))?,
                    stop_100ns: stop_offset.ok_or_else(|| unavailable("The Stop boundary was not recorded"))?,
                });
            }
            Ok((count, recording))
        })?;
        frame_index.set_event_count(event_count);
        Ok((count, recording, Some(frame_index)))
    })();
    let stopped = stage(session.Close(), "Stop the WGC session");
    result.and_then(|recording| stopped.map(|()| recording))
}

fn session_clock() -> Result<(Instant, i64, i64, i64), ProbeError> {
    let mut frequency = 0;
    stage(unsafe { QueryPerformanceFrequency(&mut frequency) }, "Read the QPC frequency")?;
    if frequency <= 0 {
        return Err(unavailable("Invalid QPC frequency"));
    }
    let mut counter = 0;
    let before = Instant::now();
    stage(unsafe { QueryPerformanceCounter(&mut counter) }, "Read the QPC time origin")?;
    let after = Instant::now();
    let bracket = after.saturating_duration_since(before);
    let uncertainty = video_ticks(bracket)?.saturating_add(1);
    Ok((before + bracket / 2, counter, frequency, uncertainty))
}

fn video_ticks(duration: Duration) -> Result<i64, ProbeError> {
    i64::try_from(duration.as_nanos() / 100)
        .map_err(|_| unavailable("The recording duration exceeds the MP4 time range"))
}

fn try_next_frame(
    pool: &Direct3D11CaptureFramePool,
) -> core::Result<Option<windows::Graphics::Capture::Direct3D11CaptureFrame>> {
    match pool.TryGetNextFrame() {
        Ok(frame) => Ok(Some(frame)),
        // WGC returns a null frame on an empty pool. The Rust projection
        // represents that successful call as an Error with a success HRESULT.
        Err(error) if error.code().is_ok() => Ok(None),
        Err(error) => Err(error),
    }
}

fn inspect_frame(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    converter: &mut gpu::Converter<'_>,
) -> Result<(Arc<ID3D11Texture2D>, [u8; 32]), ProbeError> {
    let size = stage(frame.ContentSize(), "Read the WGC frame bounds")?;
    let surface = stage(frame.Surface(), "Get the WGC frame surface")?;
    let access: IDirect3DDxgiInterfaceAccess =
        stage(surface.cast(), "Get DXGI access to the WGC surface")?;
    let texture: ID3D11Texture2D =
        stage(unsafe { access.GetInterface() }, "Get the D3D11 texture")?;
    let mut description = D3D11_TEXTURE2D_DESC::default();
    // The texture stays inside the lifetime of the WGC frame lease.
    unsafe { texture.GetDesc(&mut description) };
    if size.Width <= 0 || size.Height <= 0
        || size.Width as u32 > description.Width || size.Height as u32 > description.Height
    {
        return Err(unavailable("The WGC frame bounds exceed the texture dimensions"));
    }
    converter.convert(&texture, size.Width, size.Height)
}

struct Encoder {
    transform: IMFTransform,
    activation: IMFActivate,
    _manager: IMFDXGIDeviceManager,
    shutdown_attempted: bool,
}

impl Encoder {
    fn finish(mut self) -> Result<(), ProbeError> {
        self.shutdown_attempted = true;
        stage(unsafe { self.activation.ShutdownObject() }, "Release the encoder")
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.shutdown_attempted {
            let _ = unsafe { self.activation.ShutdownObject() };
        }
    }
}

fn configure_encoder(candidate: &IMFActivate, device: &ID3D11Device) -> Result<Encoder, ProbeError> {
    let mut token = 0;
    let mut manager = None;
    stage(
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) },
        "Create the DXGI device manager",
    )?;
    let manager = manager.ok_or_else(|| unavailable("Media Foundation did not return a DXGI device manager"))?;
    stage(unsafe { manager.ResetDevice(device, token) }, "Attach the D3D11 device")?;

    let encoder: IMFTransform = stage(
        unsafe { candidate.ActivateObject() },
        "Activate the hardware H.264 encoder",
    )?;
    let encoder = Encoder {
        transform: encoder,
        activation: candidate.clone(),
        _manager: manager,
        shutdown_attempted: false,
    };
    negotiate_encoder(&encoder.transform, &encoder._manager)?;
    Ok(encoder)
}

fn negotiate_encoder(
    encoder: &IMFTransform,
    manager: &IMFDXGIDeviceManager,
) -> Result<(), ProbeError> {
    let attributes = stage(unsafe { encoder.GetAttributes() }, "Read encoder properties")?;
    let async_enabled = stage(
        unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) },
        "Check for asynchronous hardware encoding",
    )?;
    if async_enabled == 0 {
        return Err(unavailable("The encoder does not support asynchronous operation"));
    }
    stage(
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) },
        "Unlock the asynchronous encoder",
    )?;
    let d3d11_aware = stage(
        unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) },
        "Check encoder support for D3D11",
    )?;
    if d3d11_aware == 0 {
        return Err(unavailable("The encoder does not accept a D3D11 device"));
    }
    // The manager remains alive through negotiation and encoder shutdown.
    stage(
        unsafe { encoder.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) },
        "Pass the D3D11 device to the encoder",
    )?;

    let output = video_type(&MFVideoFormat_H264)?;
    stage(unsafe { output.SetUINT32(&MF_MT_AVG_BITRATE, 8_000_000) }, "Set the H.264 bitrate")?;
    stage(unsafe { output.SetUINT32(&MF_MT_MPEG2_PROFILE, 77) }, "Set the H.264 Main profile")?;
    stage(unsafe { encoder.SetOutputType(0, &output, 0) }, "Negotiate H.264 output")?;
    let input = video_type(&MFVideoFormat_NV12)?;
    stage(unsafe { encoder.SetInputType(0, &input, 0) }, "Negotiate NV12 input")
}

fn video_type(subtype: &core::GUID) -> Result<IMFMediaType, ProbeError> {
    let media_type = stage(unsafe { MFCreateMediaType() }, "Create the video media type")?;
    // Media Foundation packs the first UINT32 into the high half.
    stage((|| unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(gpu::WIDTH) << 32) | u64::from(gpu::HEIGHT))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, (30_u64 << 32) | 1)?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1)?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
    })(), "Configure the 1080p30 format")?;
    Ok(media_type)
}

pub(crate) fn displays() -> Result<Vec<crate::platform::Display>, ProbeError> {
    Ok(outputs()?.into_iter().map(|(_, display)| {
        let id = display_id(&display);
        let rect = display.DesktopCoordinates;
        let label = format!("{id} · {}×{}", rect.right - rect.left, rect.bottom - rect.top);
        crate::platform::Display { id, label }
    }).collect())
}

fn display_id(display: &DXGI_OUTPUT_DESC) -> String {
    let end = display.DeviceName.iter().position(|&unit| unit == 0).unwrap_or(display.DeviceName.len());
    String::from_utf16_lossy(&display.DeviceName[..end])
}

fn outputs() -> Result<Vec<(IDXGIAdapter1, DXGI_OUTPUT_DESC)>, ProbeError> {
    let mut outputs = Vec::new();
    let factory: IDXGIFactory1 = stage(unsafe { CreateDXGIFactory1() }, "Open DXGI")?;
    for adapter_index in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            result => stage(result, "Enumerate DXGI adapters")?,
        };
        for output_index in 0.. {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                result => stage(result, "Enumerate DXGI displays")?,
            };
            let description = stage(unsafe { output.GetDesc() }, "Read the DXGI display description")?;
            if description.AttachedToDesktop.as_bool() && !description.Monitor.0.is_null() {
                outputs.push((adapter.clone(), description));
            }
        }
    }
    Ok(outputs)
}
