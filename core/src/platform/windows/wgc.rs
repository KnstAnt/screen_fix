use std::{
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};

use windows::{
    Foundation::{Metadata::ApiInformation, TypedEventHandler},
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
            GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus,
    Win32::{
        Graphics::{
            Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC},
            Dxgi::{DXGI_ERROR_UNSUPPORTED, IDXGIDevice, Common::DXGI_FORMAT},
        },
        Foundation::{E_INVALIDARG, E_NOTIMPL},
        System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
    },
    core::{HSTRING, IInspectable, Interface},
};

use crate::{capture::frame::CaptureColor, protocol::QpcTick};

use super::{ProbeError, clock, stage, unavailable};

const POOL_BUFFERS: i32 = 3;
const CREATE_POOL: &str = "Create the free-threaded WGC frame pool";
const CREATE_SESSION: &str = "Create the WGC session";
const START_SESSION: &str = "Start WGC capture";
const RECREATE_POOL: &str = "Recreate the WGC frame pool";

/// Start capture in the monitor's color mode. HDR is tone-mapped from FP16
/// only with a known SDR white level; otherwise, or when FP16 is rejected,
/// it uses the system BGRA8 conversion (VID-8).
pub(super) fn start_monitor(
    item: GraphicsCaptureItem, device: &ID3D11Device, size: SizeInt32,
    hdr: bool, white_known: bool, borderless: bool,
) -> Result<(Capture, CaptureColor, Option<i32>), ProbeError> {
    let mut fallback = None;
    if hdr && white_known {
        match Capture::start(item.clone(), device, size, DirectXPixelFormat::R16G16B16A16Float, borderless) {
            Ok(capture) => return Ok((capture, CaptureColor::HdrToneMappedToSdr, None)),
            Err(error) => {
                if let Some(code) = format_rejection(&error) {
                    fallback = Some(code);
                } else {
                    return Err(error);
                }
            }
        }
    }
    let capture = Capture::start(item, device, size, DirectXPixelFormat::B8G8R8A8UIntNormalized, borderless)?;
    let color = if hdr { CaptureColor::HdrSystemBgra8 } else { CaptureColor::Sdr };
    Ok((capture, color, fallback))
}

fn format_rejection(error: &ProbeError) -> Option<i32> {
    let code = error.source.as_ref()?.code();
    // Never treat device removal, callback and permission failures as format evidence.
    (matches!(error.stage, CREATE_POOL | CREATE_SESSION | START_SESSION | RECREATE_POOL)
        && matches!(code, E_INVALIDARG | E_NOTIMPL | DXGI_ERROR_UNSUPPORTED)).then_some(code.0)
}

pub(super) struct Capture {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: Option<GraphicsCaptureSession>,
    device: IDirect3DDevice,
    arrival_token: Option<i64>,
    closed_token: Option<i64>,
    item_closed: Arc<AtomicBool>,
    pub size: SizeInt32,
    pub border_visible: bool,
    pool_closed: bool,
}

impl Capture {
    fn start(
        item: GraphicsCaptureItem, device: &ID3D11Device, size: SizeInt32,
        format: DirectXPixelFormat, borderless_allowed: bool,
    ) -> Result<Self, ProbeError> {
        let dxgi: IDXGIDevice = stage(device.cast(), "Get the DXGI device")?;
        let winrt = stage(unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }, "Create the WinRT capture device")?;
        let device: IDirect3DDevice = stage(winrt.cast(), "Get the WinRT D3D11 interface")?;
        let pool = stage(Direct3D11CaptureFramePool::CreateFreeThreaded(&device, format, POOL_BUFFERS, size),
            CREATE_POOL)?;
        let mut capture = Self {
            item, pool, session: None, device, arrival_token: None, closed_token: None,
            item_closed: Arc::new(AtomicBool::new(false)), size, border_visible: true, pool_closed: false,
        };
        let wake = thread::current();
        capture.arrival_token = Some(stage(capture.pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
                wake.unpark();
                Ok(())
            })
        ), "Subscribe to WGC frame availability")?);

        let wake = thread::current();
        let item_closed = Arc::clone(&capture.item_closed);
        capture.closed_token = Some(stage(capture.item.Closed(
            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                item_closed.store(true, Ordering::Release);
                wake.unpark();
                Ok(())
            })
        ), "Subscribe to WGC item closure")?);
        let session = stage(capture.pool.CreateCaptureSession(&capture.item), CREATE_SESSION)?;
        capture.session = Some(session);
        let session = capture.session.as_ref().ok_or_else(|| unavailable("The WGC session is unavailable"))?;
        stage(session.SetIsCursorCaptureEnabled(false), "Disable WGC cursor capture")?;
        if borderless_allowed && session.SetIsBorderRequired(false).is_ok() {
            capture.border_visible = false;
        }
        stage(session.StartCapture(), START_SESSION)?;
        Ok(capture)
    }

    pub fn item_closed(&self) -> bool {
        self.item_closed.load(Ordering::Acquire)
    }

    pub fn newest(&self) -> Result<Option<BorrowedFrame>, ProbeError> {
        let mut newest = None;
        // Fixed upper bound even if the producer replenishes while draining.
        for _ in 0..POOL_BUFFERS {
            let frame = match self.pool.TryGetNextFrame() {
                Ok(frame) => frame,
                // The native empty result is a null interface with a success HRESULT.
                Err(error) if error.code().is_ok() => break,
                Err(error) => return stage(Err(error), "Get the next WGC frame"),
            };
            let mut borrowed = BorrowedFrame { frame: Some(frame), received: QpcTick(0) };
            borrowed.received = clock::qpc()?;
            if let Some(old) = newest.replace(borrowed) {
                old.close()?;
            }
        }
        Ok(newest)
    }

    pub fn reconfigure(&mut self, size: SizeInt32, hdr: bool, white_known: bool) -> Result<(CaptureColor, Option<i32>), ProbeError> {
        let mut fallback = None;
        if hdr && white_known {
            match self.recreate(size, DirectXPixelFormat::R16G16B16A16Float) {
                Ok(()) => return Ok((CaptureColor::HdrToneMappedToSdr, None)),
                Err(error) => {
                    if let Some(code) = format_rejection(&error) {
                        fallback = Some(code);
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        self.recreate(size, DirectXPixelFormat::B8G8R8A8UIntNormalized)?;
        let color = if hdr { CaptureColor::HdrSystemBgra8 } else { CaptureColor::Sdr };
        Ok((color, fallback))
    }

    fn recreate(&mut self, size: SizeInt32, format: DirectXPixelFormat) -> Result<(), ProbeError> {
        // BorrowedFrame never escapes a single capture-thread iteration.
        stage(self.pool.Recreate(&self.device, format, POOL_BUFFERS, size), RECREATE_POOL)?;
        self.size = size;
        Ok(())
    }

    pub fn close(mut self) -> Result<(), ProbeError> {
        self.close_resources()
    }

    fn close_resources(&mut self) -> Result<(), ProbeError> {
        let arrival = self.arrival_token.take().map_or(Ok(()), |token| {
            stage(self.pool.RemoveFrameArrived(token), "Unsubscribe from WGC frame availability")
        });
        let closed = self.closed_token.take().map_or(Ok(()), |token| {
            stage(self.item.RemoveClosed(token), "Unsubscribe from WGC item closure")
        });
        let session = self.session.take().map_or(Ok(()), |session| {
            stage(session.Close(), "Close the WGC session")
        });
        let pool = if self.pool_closed {
            Ok(())
        } else {
            self.pool_closed = true;
            stage(self.pool.Close(), "Close the WGC frame pool")
        };
        arrival.and(closed).and(session).and(pool)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.close_resources();
    }
}

pub(super) struct BorrowedFrame {
    frame: Option<Direct3D11CaptureFrame>,
    pub received: QpcTick,
}

impl BorrowedFrame {
    fn frame(&self) -> Result<&Direct3D11CaptureFrame, ProbeError> {
        self.frame.as_ref().ok_or_else(|| unavailable("The WGC frame lease is closed"))
    }

    pub fn source_100ns(&self) -> Result<i64, ProbeError> {
        Ok(stage(self.frame()?.SystemRelativeTime(), "Read the WGC source timestamp")?.Duration)
    }

    pub fn with_texture<T>(
        &self, expected: SizeInt32, format: DXGI_FORMAT, copy: impl FnOnce(&ID3D11Texture2D) -> Result<T, ProbeError>,
    ) -> Result<Option<T>, ProbeError> {
        let frame = self.frame()?;
        let size = stage(frame.ContentSize(), "Read the WGC frame bounds")?;
        if size != expected {
            return Ok(None);
        }
        let surface = stage(frame.Surface(), "Get the WGC frame surface")?;
        let access: IDirect3DDxgiInterfaceAccess = stage(surface.cast(), "Get DXGI access to the WGC surface")?;
        let texture: ID3D11Texture2D = stage(unsafe { access.GetInterface() }, "Get the WGC texture")?;
        let mut description = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut description) };
        if size.Width <= 0 || size.Height <= 0
            || size.Width as u32 > description.Width || size.Height as u32 > description.Height
        {
            return Err(unavailable("The WGC frame bounds exceed the texture dimensions"));
        }
        if description.Format != format {
            return Err(unavailable("The WGC surface pixel format differs from the configured pool format"));
        }
        copy(&texture).map(Some)
    }

    pub fn close(mut self) -> Result<(), ProbeError> {
        self.frame.take().map_or(Ok(()), |frame| stage(frame.Close(), "Return the borrowed frame to WGC"))
    }
}

impl Drop for BorrowedFrame {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            let _ = frame.Close();
        }
    }
}

pub(super) fn request_borderless() -> bool {
    let session = HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession");
    let property = HSTRING::from("IsBorderRequired");
    if !ApiInformation::IsPropertyPresent(&session, &property).unwrap_or(false) {
        return false;
    }
    let Ok(request) = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless) else {
        return false;
    };
    // The user or OS may never grant access. Bound the startup wait on this worker.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match request.Status() {
            Ok(status) if status.0 == 0 && Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(status) if status.0 == 1 => return request.GetResults().is_ok_and(|status| status == AppCapabilityAccessStatus::Allowed),
            _ => {
                let _ = request.Cancel();
                return false;
            }
        }
    }
}
