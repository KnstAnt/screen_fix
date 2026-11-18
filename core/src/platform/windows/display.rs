pub(super) mod topology;

use std::{collections::BTreeMap, mem::size_of, num::NonZeroU32};

use windows::{
    core::Interface,
    Win32::{
    Devices::Display::*,
    Foundation::{ERROR_INSUFFICIENT_BUFFER, LUID},
    Graphics::{
        Dxgi::{CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
            Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020},
        Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
    },
    UI::{HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI}, WindowsAndMessaging::MONITORINFOF_PRIMARY},
    },
    Graphics::SizeInt32,
};

use crate::{
    capture::frame::MonitorGeometry,
    config::DisplayKey,
    package::metadata::DisplayField,
    protocol::{GeometryEpoch, GeometryId, MonitorId, PhysicalRect},
};

use super::{ProbeError, stage, unavailable};

/// A display adapter, by its LUID.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct AdapterId {
    low: u32,
    high: i32,
}

impl From<LUID> for AdapterId {
    fn from(luid: LUID) -> Self {
        Self { low: luid.LowPart, high: luid.HighPart }
    }
}

impl From<AdapterId> for LUID {
    fn from(id: AdapterId) -> Self {
        Self { LowPart: id.low, HighPart: id.high }
    }
}

pub(super) struct Monitor {
    pub key: DisplayKey,
    pub name: String,
    pub adapter: IDXGIAdapter1,
    pub handle: HMONITOR,
    pub bounds: PhysicalRect,
    pub dpi_x: Option<NonZeroU32>,
    pub dpi_y: Option<NonZeroU32>,
    pub refresh: Option<(u32, NonZeroU32)>,
    pub rotation: i32,
    pub primary: bool,
    pub hdr: bool,
    pub sdr_white: Option<NonZeroU32>,
    /// Properties of this monitor that could not be read, with the native
    /// code of the failed read. They never fail the enumeration (VID-9).
    pub unavailable: BTreeMap<DisplayField, Option<i32>>,
}

impl Monitor {
    pub fn geometry(&self, monitor: MonitorId, epoch: GeometryEpoch) -> MonitorGeometry {
        MonitorGeometry {
            id: GeometryId { monitor, epoch },
            bounds: self.bounds,
            dpi_x: self.dpi_x,
            dpi_y: self.dpi_y,
        }
    }

    /// The physical pixel size, as capture and encoding take it.
    pub fn size(&self) -> Result<SizeInt32, ProbeError> {
        let width = i32::try_from(self.bounds.width()).map_err(|_| unavailable("The physical monitor width overflowed"))?;
        let height = i32::try_from(self.bounds.height()).map_err(|_| unavailable("The physical monitor height overflowed"))?;
        Ok(SizeInt32 { Width: width, Height: height })
    }
}

struct Target {
    source: String,
    name: String,
    key: DisplayKey,
    path: DISPLAYCONFIG_PATH_TARGET_INFO,
}

fn string(units: &[u16]) -> String {
    let end = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

fn header<T>(kind: DISPLAYCONFIG_DEVICE_INFO_TYPE, adapter_id: LUID, id: u32) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: kind, size: size_of::<T>() as u32, adapterId: adapter_id, id }
}

fn status(code: u32, operation: &'static str) -> Result<(), ProbeError> {
    stage(windows::Win32::Foundation::WIN32_ERROR(code).ok(), operation)
}

fn targets() -> Result<Vec<Target>, ProbeError> {
    // Buffer sizing and query can race a topology update.
    for _ in 0..3 {
        let (mut paths, mut modes) = (0, 0);
        let result = unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) };
        status(result.0, "Size the active display configuration")?;
        if paths > 1024 || modes > 4096 {
            return Err(unavailable("The active display configuration exceeds its supported bounds"));
        }
        let mut path_buffer = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
        let mut mode_buffer = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
        let result = unsafe {
            QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut paths, path_buffer.as_mut_ptr(),
                &mut modes, mode_buffer.as_mut_ptr(), None)
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        status(result.0, "Read the active display configuration")?;
        path_buffer.truncate(paths as usize);
        let mut targets = Vec::new();
        for path in path_buffer {
            let source = path.sourceInfo;
            let target = path.targetInfo;
            let mut source_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: header::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, source.adapterId, source.id),
                ..Default::default()
            };
            let mut target_name = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: header::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, target.adapterId, target.id),
                ..Default::default()
            };
            status(unsafe { DisplayConfigGetDeviceInfo(&mut source_name.header) } as u32, "Read the display source name")?;
            status(unsafe { DisplayConfigGetDeviceInfo(&mut target_name.header) } as u32, "Read the physical display device path")?;
            let path = string(&target_name.monitorDevicePath).to_lowercase();
            let Ok(key) = DisplayKey::new(&path) else { continue };
            targets.push(Target {
                source: string(&source_name.viewGdiDeviceName),
                name: string(&target_name.monitorFriendlyDeviceName),
                key,
                path: target,
            });
        }
        return Ok(targets);
    }
    Err(unavailable("The display topology changed during discovery"))
}

/// The selected monitor. Another monitor's unreadable output does not affect
/// it (V4); its own unreadable output fails with that output's error.
pub(super) fn resolve(key: &DisplayKey) -> Result<Monitor, ProbeError> {
    let survey = survey()?;
    if let Some((_, error)) = survey.unreadable.into_iter().find(|(unreadable, _)| unreadable == key) {
        return Err(error);
    }
    survey.monitors.into_iter().find(|monitor| &monitor.key == key)
        .ok_or_else(|| unavailable("The selected physical display is no longer available"))
}

/// The selected monitor, if it still has the properties of `expected`. A
/// failed read proves nothing about the monitor and yields none.
pub(super) fn resolve_matching(key: &DisplayKey, expected: &topology::Output) -> Option<Monitor> {
    resolve(key).ok()
        .filter(|actual| topology::Output::from_native(actual).is_ok_and(|actual| actual == *expected))
}

/// Every active monitor. Any unreadable output fails the whole list, which
/// then proves nothing about any monitor.
pub(super) fn enumerate() -> Result<Vec<Monitor>, ProbeError> {
    let survey = survey()?;
    match survey.unreadable.into_iter().next() {
        Some((_, error)) => Err(error),
        None => Ok(survey.monitors),
    }
}

/// One enumeration of the active outputs.
struct Survey {
    monitors: Vec<Monitor>,
    /// Identified outputs whose color mode or rectangle could not be read,
    /// with the failure. They prove neither presence nor absence.
    unreadable: Vec<(DisplayKey, ProbeError)>,
}

/// The active monitors. A failure to enumerate the outputs or to identify
/// one fails the whole survey, which then proves nothing about any monitor
/// and is retried on the next topology tick. A failure to read an identified
/// output's color mode or rectangle is recorded for that output alone. DPI,
/// refresh and SDR white failures only leave that monitor's field null.
fn survey() -> Result<Survey, ProbeError> {
    let targets = targets()?;
    // The enumeration owns a fresh factory and never caches an HMONITOR across re-resolution.
    let factory: IDXGIFactory1 = stage(unsafe { CreateDXGIFactory1() }, "Open DXGI")?;
    let mut monitors = Vec::new();
    let mut unreadable: Vec<(DisplayKey, ProbeError)> = Vec::new();
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
            let desc = stage(unsafe { output.GetDesc() }, "Read the DXGI display description")?;
            if !desc.AttachedToDesktop.as_bool() || desc.Monitor.0.is_null() {
                continue;
            }
            let name = string(&desc.DeviceName);
            // Mirrored targets share a single desktop source and capture surface.
            // Select one deterministic representative instead of opening duplicate sessions.
            let Some(target) = targets.iter().filter(|target| target.source == name)
                .min_by(|a, b| a.key.as_str().cmp(b.key.as_str())) else { continue };
            if monitors.iter().any(|monitor: &Monitor| monitor.key == target.key)
                || unreadable.iter().any(|(key, _)| *key == target.key)
            {
                continue;
            }
            match read_monitor(&adapter, &output, &desc, target, name) {
                Ok(monitor) => monitors.push(monitor),
                Err(error) => unreadable.push((target.key.clone(), error)),
            }
        }
    }
    monitors.sort_by(|a, b| b.primary.cmp(&a.primary).then_with(|| a.key.as_str().cmp(b.key.as_str())));
    Ok(Survey { monitors, unreadable })
}

/// The properties of one identified output.
fn read_monitor(
    adapter: &IDXGIAdapter1, output: &IDXGIOutput, desc: &DXGI_OUTPUT_DESC, target: &Target, name: String,
) -> Result<Monitor, ProbeError> {
    let output6: IDXGIOutput6 = stage(output.cast(), "Get the advanced display output")?;
    let color = stage(unsafe { output6.GetDesc1() }, "Read the display color mode")?;
    let hdr = color.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    let mut missing = BTreeMap::new();
    let sdr_white = if hdr {
        let mut level = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: header::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                target.path.adapterId, target.path.id),
            ..Default::default()
        };
        let code = unsafe { DisplayConfigGetDeviceInfo(&mut level.header) };
        let white = if code == 0 { NonZeroU32::new(level.SDRWhiteLevel) } else { None };
        if white.is_none() {
            missing.insert(DisplayField::SdrWhiteLevel, (code != 0).then_some(code));
        }
        white
    } else {
        None
    };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    stage(unsafe { GetMonitorInfoW(desc.Monitor, &mut info).ok() }, "Read the physical monitor rectangle")?;
    let rect = info.rcMonitor;
    let bounds = PhysicalRect::new(rect.left, rect.top, rect.right, rect.bottom)
        .ok_or_else(|| unavailable("The monitor rectangle has invalid physical bounds"))?;
    let (mut dpi_x, mut dpi_y) = (0, 0);
    let dpi = unsafe { GetDpiForMonitor(desc.Monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let (dpi_x, dpi_y) = match dpi {
        Ok(()) => (NonZeroU32::new(dpi_x), NonZeroU32::new(dpi_y)),
        Err(_) => (None, None),
    };
    if dpi_x.is_none() || dpi_y.is_none() {
        missing.insert(DisplayField::Dpi, dpi.err().map(|error| error.code().0));
    }
    let rate = target.path.refreshRate;
    let refresh = NonZeroU32::new(rate.Denominator).filter(|_| rate.Numerator != 0)
        .map(|denominator| (rate.Numerator, denominator));
    if refresh.is_none() {
        missing.insert(DisplayField::Refresh, None);
    }
    Ok(Monitor {
        key: target.key.clone(),
        name: if target.name.is_empty() { name } else { target.name.clone() },
        adapter: adapter.clone(), handle: desc.Monitor, bounds,
        dpi_x: dpi_x.filter(|_| dpi_y.is_some()), dpi_y: dpi_y.filter(|_| dpi_x.is_some()),
        refresh, rotation: target.path.rotation.0,
        primary: info.dwFlags & MONITORINFOF_PRIMARY != 0, hdr, sdr_white, unavailable: missing,
    })
}
