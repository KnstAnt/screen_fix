use std::{ffi::c_void, sync::OnceLock};

use windows::{
    Win32::{
        Foundation::HMODULE,
        Media::MediaFoundation::*,
        System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
    },
    core::{GUID, HRESULT, Interface, PCSTR, PCWSTR, w},
};

use super::{ProbeError, ProbeKind, stage, unavailable};

const MEDIA_FEATURES: &str = "Media Foundation is unavailable. Install the Media Feature Pack in Windows Optional features (ms-settings:optionalfeatures), then reopen Screen Fix";

type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type Shutdown = unsafe extern "system" fn() -> HRESULT;
type CreateAttributes = unsafe extern "system" fn(*mut Option<IMFAttributes>, u32) -> HRESULT;
type CreateMediaType = unsafe extern "system" fn(*mut Option<IMFMediaType>) -> HRESULT;
type CreateDeviceManager = unsafe extern "system" fn(*mut u32, *mut Option<IMFDXGIDeviceManager>) -> HRESULT;
type CreateSurface = unsafe extern "system" fn(*const GUID, *mut c_void, u32, i32, *mut Option<IMFMediaBuffer>) -> HRESULT;
type CreateSample = unsafe extern "system" fn(*mut Option<IMFSample>) -> HRESULT;
type CreateBuffer = unsafe extern "system" fn(u32, u32, *mut Option<IMFMediaBuffer>) -> HRESULT;
type CreateTrackedSample = unsafe extern "system" fn(*mut Option<IMFTrackedSample>) -> HRESULT;
type EnumOnAdapter = unsafe extern "system" fn(GUID, u32, *const MFT_REGISTER_TYPE_INFO, *const MFT_REGISTER_TYPE_INFO, *mut c_void, *mut *mut Option<IMFActivate>, *mut u32) -> HRESULT;
type CreateFile = unsafe extern "system" fn(i32, i32, i32, PCWSTR, *mut Option<IMFByteStream>) -> HRESULT;
type CreateStreamReader = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut Option<IMFSourceReader>) -> HRESULT;
type CreateWriter = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut c_void, *mut Option<IMFSinkWriter>) -> HRESULT;

struct Functions {
    startup: Startup,
    shutdown: Shutdown,
    attributes: CreateAttributes,
    media_type: CreateMediaType,
    device_manager: CreateDeviceManager,
    surface: CreateSurface,
    tracked_sample: CreateTrackedSample,
    sample: CreateSample,
    buffer: CreateBuffer,
    enum_on_adapter: Option<EnumOnAdapter>,
    open_file: CreateFile,
    stream_reader: CreateStreamReader,
    writer: CreateWriter,
}

#[derive(Clone, Copy)]
struct LoadError {
    operation: &'static str,
    code: HRESULT,
    kind: ProbeKind,
}

impl Functions {
    fn load() -> Result<Self, LoadError> {
        // The successful system module loads stay referenced until process exit.
        // MF objects and allocator callbacks may outlive an individual caller.
        let platform = load_system(w!("mfplat.dll"))?;
        let read_write = load_system(w!("mfreadwrite.dll"))?;
        macro_rules! optional {
            ($module:expr, $symbol:ident, $ty:ty) => {{
                let name = concat!(stringify!($symbol), "\0");
                unsafe { GetProcAddress($module, PCSTR(name.as_ptr())) }
                    .map(|address| unsafe { std::mem::transmute::<_, $ty>(address) })
            }};
        }
        macro_rules! required {
            ($module:expr, $symbol:ident, $ty:ty) => {
                optional!($module, $symbol, $ty).ok_or_else(|| load_error(concat!("Resolve Media Foundation ", stringify!($symbol))))?
            };
        }
        Ok(Self {
            startup: required!(platform, MFStartup, Startup),
            shutdown: required!(platform, MFShutdown, Shutdown),
            attributes: required!(platform, MFCreateAttributes, CreateAttributes),
            media_type: required!(platform, MFCreateMediaType, CreateMediaType),
            device_manager: required!(platform, MFCreateDXGIDeviceManager, CreateDeviceManager),
            surface: required!(platform, MFCreateDXGISurfaceBuffer, CreateSurface),
            tracked_sample: required!(platform, MFCreateTrackedSample, CreateTrackedSample),
            sample: required!(platform, MFCreateSample, CreateSample),
            buffer: required!(platform, MFCreateAlignedMemoryBuffer, CreateBuffer),
            enum_on_adapter: optional!(platform, MFTEnum2, EnumOnAdapter),
            open_file: required!(platform, MFCreateFile, CreateFile),
            stream_reader: required!(read_write, MFCreateSourceReaderFromByteStream, CreateStreamReader),
            writer: required!(read_write, MFCreateSinkWriterFromURL, CreateWriter),
        })
    }
}

/// A Media Foundation system module; a load failure means the Media Feature Pack is missing.
/// A Media Foundation `UINT64` attribute that holds two `UINT32`s, such as a
/// frame size or rate: `high` is the high half.
pub(super) const fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn load_system(name: PCWSTR) -> Result<HMODULE, LoadError> {
    unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|error| LoadError { operation: MEDIA_FEATURES, code: error.code(), kind: ProbeKind::MediaFoundationMissing })
}

fn load_error(operation: &'static str) -> LoadError {
    LoadError { operation, code: windows::core::Error::from_thread().code(), kind: ProbeKind::Other }
}

fn functions() -> Result<&'static Functions, ProbeError> {
    static FUNCTIONS: OnceLock<Result<Functions, LoadError>> = OnceLock::new();
    FUNCTIONS.get_or_init(Functions::load).as_ref()
        .map_err(|error| ProbeError::new(error.operation)
            .with_kind(error.kind)
            .with_source(Some(error.code.into()))
            .owned_by(crate::package::diagnostics::FailureComponent::Video))
}

pub(super) struct Runtime(&'static Functions);

impl Runtime {
    pub(super) fn initialize() -> Result<Self, ProbeError> {
        let api = functions()?;
        stage(unsafe { (api.startup)(MF_VERSION, MFSTARTUP_FULL).ok() }, "Start Media Foundation")?;
        Ok(Self(api))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = unsafe { (self.0.shutdown)() };
    }
}

fn create<T>(
    operation: &'static str, call: impl FnOnce(*mut Option<T>) -> HRESULT,
) -> Result<T, ProbeError> {
    let mut value = None;
    stage(call(&mut value).ok(), operation)?;
    value.ok_or_else(|| unavailable(operation))
}

pub(super) fn attributes(capacity: u32) -> Result<IMFAttributes, ProbeError> {
    let api = functions()?;
    create("Create Media Foundation attributes", |out| unsafe { (api.attributes)(out, capacity) })
}

pub(super) fn media_type() -> Result<IMFMediaType, ProbeError> {
    let api = functions()?;
    create("Create a Media Foundation media type", |out| unsafe { (api.media_type)(out) })
}

pub(super) fn device_manager() -> Result<(u32, IMFDXGIDeviceManager), ProbeError> {
    let api = functions()?;
    let mut token = 0;
    let manager = create("Create the DXGI device manager", |out| unsafe { (api.device_manager)(&mut token, out) })?;
    Ok((token, manager))
}

pub(super) fn surface(iid: &GUID, resource: &impl Interface) -> Result<IMFMediaBuffer, ProbeError> {
    let api = functions()?;
    create("Create the Media Foundation NV12 surface buffer", |out| unsafe {
        (api.surface)(iid, resource.as_raw(), 0, 0, out)
    })
}

pub(super) fn tracked_sample() -> Result<IMFTrackedSample, ProbeError> {
    let api = functions()?;
    create("Create a tracked Media Foundation sample", |out| unsafe { (api.tracked_sample)(out) })
}

pub(super) fn sample() -> Result<IMFSample, ProbeError> {
    let api = functions()?;
    create("Create a Media Foundation sample", |out| unsafe { (api.sample)(out) })
}

pub(super) fn buffer(size: u32, alignment_mask: u32) -> Result<IMFMediaBuffer, ProbeError> {
    let api = functions()?;
    create("Allocate a Media Foundation sample buffer", |out| unsafe {
        (api.buffer)(size, alignment_mask, out)
    })
}

pub(super) fn enum_on_adapter(
    flags: MFT_ENUM_FLAG, input: &MFT_REGISTER_TYPE_INFO, output: &MFT_REGISTER_TYPE_INFO,
    filter: &IMFAttributes,
) -> Result<Vec<IMFActivate>, ProbeError> {
    use windows::Win32::System::Com::CoTaskMemFree;
    let call = functions()?.enum_on_adapter
        .ok_or_else(|| unavailable("Adapter-specific Media Foundation encoder discovery is unavailable"))?;
    let mut entries = std::ptr::null_mut();
    let mut count = 0;
    stage(unsafe {
        call(MFT_CATEGORY_VIDEO_ENCODER, flags.0 as u32, input, output, filter.as_raw(), &mut entries, &mut count).ok()
    }, "Find a hardware H.264 encoder")?;
    let mut result = Vec::new();
    if !entries.is_null() {
        for index in 0..count as usize {
            result.extend(unsafe { std::ptr::read(entries.add(index)) });
        }
        unsafe { CoTaskMemFree(Some(entries.cast())) };
    }
    Ok(result)
}

/// A reader of a finalized part. With a device manager the reader may use a
/// hardware decoder; without one it decodes in software.
pub(super) fn mp4_part_reader(url: PCWSTR, decoder: Option<&IMFDXGIDeviceManager>) -> Result<IMFSourceReader, ProbeError> {
    let api = functions()?;
    let settings = decoder.map(|manager| {
        let settings = attributes(1)?;
        stage(unsafe { settings.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, manager) },
            "Attach the decoder device to the MP4 reader")?;
        Ok::<_, ProbeError>(settings)
    }).transpose()?;
    let bytes: IMFByteStream = create("Open the finalized MP4 byte stream", |out| unsafe {
        (api.open_file)(MF_ACCESSMODE_READ.0, MF_OPENMODE_FAIL_IF_NOT_EXIST.0, MF_FILEFLAGS_NONE.0, url, out)
    })?;
    let hints: IMFAttributes = stage(bytes.cast(), "Read the MP4 byte stream attributes")?;
    stage(unsafe {
        hints.SetString(&MF_BYTESTREAM_CONTENT_TYPE, w!("video/mp4"))
    }, "Set the MP4 byte stream content type")?;
    stage(unsafe {
        hints.SetString(&MF_BYTESTREAM_ORIGIN_NAME, w!("segment.mp4"))
    }, "Identify the finalized part as MP4")?;
    create("Open the finalized MP4 reader", |out| unsafe {
        (api.stream_reader)(bytes.as_raw(), settings.as_ref().map_or(std::ptr::null_mut(), Interface::as_raw), out)
    })
}

pub(super) fn sink_writer(
    url: PCWSTR, attributes: Option<&IMFAttributes>,
) -> Result<IMFSinkWriter, ProbeError> {
    let api = functions()?;
    create("Open the Media Foundation video writer", |out| unsafe {
        (api.writer)(url, std::ptr::null_mut(), attributes.map_or(std::ptr::null_mut(), Interface::as_raw), out)
    })
}
