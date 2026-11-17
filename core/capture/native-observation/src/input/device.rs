use std::{collections::BTreeMap, mem::{size_of, size_of_val}, ptr::{null, null_mut}};

use screen_fix_protocol::{
    BoundedText, DeviceId,
    ipc::{DEVICE_PATH_HASH_BYTES, DeviceDescriptor, DeviceKind, HelperFailureStage, InputSample, MAX_INPUT_DEVICES},
};
use windows_sys::Win32::{
    Devices::{
        DeviceAndDriverInstallation::{
            HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA, SPDRP_HARDWAREID,
            SetupDiCreateDeviceInfoList, SetupDiDestroyDeviceInfoList, SetupDiGetDeviceInterfaceDetailW,
            SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDeviceInterfaceW,
        },
        HumanInterfaceDevice::{HIDD_ATTRIBUTES, HidD_GetAttributes},
        Properties::{DEVPKEY_Device_DriverVersion, DEVPROP_TYPE_STRING},
    },
    Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RID_DEVICE_INFO,
        RIDI_DEVICEINFO, RIDI_DEVICENAME, RIDI_PREPARSEDDATA, RIM_TYPEHID, RIM_TYPEKEYBOARD,
        RIM_TYPEMOUSE,
    },
};

use super::{NativeError, last_error};

const PREPARSED_LIMIT: usize = 64 * 1024;
const DEVICE_NAME_LIMIT: usize = 1024;

pub(super) fn id(handle: HANDLE) -> Option<DeviceId> {
    (!handle.is_null()).then_some(DeviceId(handle as usize as u64))
}

pub(super) fn inventory() -> Result<Vec<RAWINPUTDEVICELIST>, NativeError> {
    let mut count = 0;
    let element = size_of::<RAWINPUTDEVICELIST>() as u32;
    if unsafe { GetRawInputDeviceList(null_mut(), &mut count, element) } == u32::MAX {
        return Err(last_error(HelperFailureStage::DeviceDiscovery));
    }
    if count as usize > MAX_INPUT_DEVICES {
        return Err(NativeError::without_code(HelperFailureStage::DeviceDiscovery));
    }
    // Allocate the bounded allowance once for startup so hot-plug between the
    // size query and listing cannot overrun the buffer.
    let mut devices = vec![RAWINPUTDEVICELIST::default(); MAX_INPUT_DEVICES];
    count = MAX_INPUT_DEVICES as u32;
    let copied = unsafe { GetRawInputDeviceList(devices.as_mut_ptr(), &mut count, element) };
    if copied == u32::MAX {
        return Err(last_error(HelperFailureStage::DeviceDiscovery));
    }
    if copied as usize > devices.len() {
        return Err(NativeError::without_code(HelperFailureStage::DeviceDiscovery));
    }
    devices.truncate(copied as usize);
    Ok(devices)
}

/// The Raw Input thread's part of a descriptor, from Raw Input queries only:
/// kind and hashed path. The interface path is returned for the device worker,
/// which alone opens it (`identify`, I6).
pub(super) fn descriptor(handle: HANDLE, known_kind: Option<u32>) -> Option<(DeviceDescriptor, Option<Vec<u16>>)> {
    let id = id(handle)?;
    let mut info = RID_DEVICE_INFO::default();
    let mut size = size_of::<RID_DEVICE_INFO>() as u32;
    info.cbSize = size;
    let result = unsafe {
        GetRawInputDeviceInfoW(handle, RIDI_DEVICEINFO, (&mut info as *mut RID_DEVICE_INFO).cast(), &mut size)
    };
    let valid_info = result != u32::MAX && result as usize >= size_of::<RID_DEVICE_INFO>();
    let kind = if valid_info { Some(info.dwType) } else { known_kind };
    let kind = match kind {
        Some(RIM_TYPEKEYBOARD) => DeviceKind::Keyboard,
        Some(RIM_TYPEMOUSE) => DeviceKind::Pointer,
        Some(RIM_TYPEHID) if valid_info => {
            let hid = unsafe { info.Anonymous.hid };
            DeviceKind::Hid { usage_page: hid.usUsagePage, usage: hid.usUsage }
        }
        _ => DeviceKind::Unknown,
    };
    let path = name(handle);
    let hash = path.as_deref().and_then(path_hash);
    Some((DeviceDescriptor {
        id, kind, path_hash: hash, vendor_id: None, product_id: None, version: None,
        preparsed_data_available: false, driver_version: None, hid: None,
    }, path))
}

/// Device worker only: vendor, product and version (INP-8), and the pen
/// driver version (CTX-12). These calls can block on device IO.
pub(super) fn identify(path: &[u16], descriptor: &mut DeviceDescriptor) {
    let identity = hid_attributes(path).map(|(vendor, product, version)| (vendor, product, Some(version)))
        .or_else(|| hardware_ids(path).map(|(vendor, product)| (vendor, product, None)));
    if let Some((vendor, product, version)) = identity {
        descriptor.vendor_id = Some(vendor);
        descriptor.product_id = Some(product);
        descriptor.version = version;
    }
    if matches!(descriptor.kind, DeviceKind::Hid { usage_page: 0x0D, usage: 1 | 2 }) {
        descriptor.driver_version = driver_version(path);
    }
}

fn name(handle: HANDLE) -> Option<Vec<u16>> {
    let mut length = 0;
    if unsafe { GetRawInputDeviceInfoW(handle, RIDI_DEVICENAME, null_mut(), &mut length) } == u32::MAX
        || length == 0 || length as usize > DEVICE_NAME_LIMIT
    {
        return None;
    }
    // RIDI_DEVICENAME counts UTF-16 elements rather than bytes.
    let mut text = vec![0_u16; length as usize];
    let copied = unsafe { GetRawInputDeviceInfoW(handle, RIDI_DEVICENAME, text.as_mut_ptr().cast(), &mut length) };
    if copied == u32::MAX || copied as usize > text.len() {
        return None;
    }
    let end = text.iter().position(|ch| *ch == 0).unwrap_or(text.len());
    if end == 0 { return None; }
    text.truncate(end);
    text.push(0);
    Some(text)
}

fn path_hash(path: &[u16]) -> Option<BoundedText<DEVICE_PATH_HASH_BYTES>> {
    let mut hasher = blake3::Hasher::new();
    for unit in &path[..path.len() - 1] {
        hasher.update(&unit.to_le_bytes());
    }
    BoundedText::new(hasher.finalize().to_hex().as_str()).ok()
}

fn hid_attributes(path: &[u16]) -> Option<(u16, u16, u16)> {
    // The HID interface is queried without opening it for reading or writing.
    let handle = open_metadata_handle(path);
    if handle == INVALID_HANDLE_VALUE { return None; }
    let mut attributes = HIDD_ATTRIBUTES { Size: size_of::<HIDD_ATTRIBUTES>() as u32, ..Default::default() };
    let queried = unsafe { HidD_GetAttributes(handle, &mut attributes) as u8 } != 0;
    unsafe { CloseHandle(handle) };
    queried.then_some((attributes.VendorID, attributes.ProductID, attributes.VersionNumber))
}

fn open_metadata_handle(path: &[u16]) -> HANDLE {
    unsafe {
        CreateFileW(path.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(), OPEN_EXISTING, 0, null_mut())
    }
}

struct Devices(HDEVINFO);

impl Drop for Devices {
    fn drop(&mut self) {
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn device_info(path: &[u16]) -> Option<(Devices, SP_DEVINFO_DATA)> {
    let devices = unsafe { SetupDiCreateDeviceInfoList(null(), null_mut()) };
    if devices == INVALID_HANDLE_VALUE as HDEVINFO { return None; }
    let devices = Devices(devices);
    let mut interface = SP_DEVICE_INTERFACE_DATA { cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
    if unsafe { SetupDiOpenDeviceInterfaceW(devices.0, path.as_ptr(), 0, &mut interface) } == 0 {
        return None;
    }
    let mut info = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
    // SetupAPI fills DevInfoData when the interface exists even though the
    // intentionally omitted detail buffer reports ERROR_INSUFFICIENT_BUFFER.
    let mut required = 0;
    if unsafe {
        SetupDiGetDeviceInterfaceDetailW(devices.0, &interface, null_mut(), 0, &mut required, &mut info)
    } == 0 && unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    Some((devices, info))
}

fn driver_version(path: &[u16]) -> Option<BoundedText<64>> {
    let (devices, mut info) = device_info(path)?;
    let mut text = [0_u16; 64];
    let mut size = 0;
    let mut property = 0;
    if unsafe { SetupDiGetDevicePropertyW(devices.0, &mut info, &DEVPKEY_Device_DriverVersion,
        &mut property, text.as_mut_ptr().cast(), size_of_val(&text) as u32, &mut size, 0) } == 0
        || property != DEVPROP_TYPE_STRING || size < 4 || size % 2 != 0
    {
        return None;
    }
    let text = text.get(..size as usize / 2)?.strip_suffix(&[0])?;
    let version = String::from_utf16(text).ok()?;
    (!version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.'))
        .then(|| BoundedText::new(&version).ok()).flatten()
}

fn hardware_ids(path: &[u16]) -> Option<(u16, u16)> {
    let (devices, info) = device_info(path)?;
    let mut ids = [0_u16; 2048];
    let mut size = 0;
    let mut kind = 0;
    if unsafe { SetupDiGetDeviceRegistryPropertyW(devices.0, &info, SPDRP_HARDWAREID,
        &mut kind, ids.as_mut_ptr().cast(), size_of_val(&ids) as u32, &mut size) } == 0
        || kind != 7 // REG_MULTI_SZ
        || size as usize > size_of_val(&ids) || size % 2 != 0
    {
        return None;
    }
    ids[..size as usize / 2].split(|ch| *ch == 0).find_map(|id| {
        let id = String::from_utf16(id).ok()?.to_ascii_uppercase();
        let fields = || id.split(['\\', '&']);
        let value = |tag| fields().find_map(|field| {
            let hex = field.strip_prefix(tag)?;
            (hex.len() == 4).then(|| u16::from_str_radix(hex, 16).ok()).flatten()
        });
        value("VID_").zip(value("PID_")).or_else(|| value("VEN_").zip(value("DEV_")))
    })
}

#[derive(Default)]
pub(super) struct DeviceIds {
    active: BTreeMap<DeviceId, DeviceId>,
    next: u64,
}

impl DeviceIds {
    pub(super) fn clear(&mut self) { self.active.clear(); }

    pub(super) fn assign(&mut self, handle: DeviceId) -> Option<DeviceId> {
        if let Some(id) = self.active.get(&handle) {
            return Some(*id);
        }
        if self.active.len() == MAX_INPUT_DEVICES {
            return None;
        }
        self.next = self.next.checked_add(1)?;
        let id = DeviceId(self.next);
        self.active.insert(handle, id);
        Some(id)
    }

    /// The session ID is still assigned to a present device.
    pub(super) fn active(&self, id: DeviceId) -> bool {
        self.active.values().any(|active| *active == id)
    }

    pub(super) fn remove(&mut self, handle: DeviceId) -> Option<DeviceId> {
        self.active.remove(&handle)
    }

    /// Translate the record's native handle to its session device ID. `None`
    /// when the inventory is full. `Some(Some(handle))` names a handle seen
    /// first in this record: the caller announces it before the record.
    pub(super) fn identify(&mut self, sample: &mut InputSample) -> Option<Option<DeviceId>> {
        let slot = match sample {
            InputSample::RawKey(key) => key.device.as_mut(),
            InputSample::RawPointer(pointer) => pointer.device.as_mut(),
            InputSample::Hid(report) => Some(&mut report.device),
            _ => None,
        };
        let Some(slot) = slot else { return Some(None) };
        let native = *slot;
        let first_seen = !self.active.contains_key(&native);
        *slot = self.assign(native)?;
        Some(first_seen.then_some(native))
    }
}

pub(super) fn preparsed(handle: HANDLE) -> Option<Box<[u64]>> {
    let mut bytes = 0;
    if unsafe { GetRawInputDeviceInfoW(handle, RIDI_PREPARSEDDATA, null_mut(), &mut bytes) } == u32::MAX
        || bytes == 0 || bytes as usize > PREPARSED_LIMIT
    {
        return None;
    }
    let requested = bytes;
    // Raw Input writes the opaque preparsed allocation in-place. Keep its
    // address aligned and preserve the initialized storage on the Raw owner.
    let mut buffer = vec![0_u64; (bytes as usize).div_ceil(size_of::<u64>())];
    let copied = unsafe {
        GetRawInputDeviceInfoW(handle, RIDI_PREPARSEDDATA, buffer.as_mut_ptr().cast(), &mut bytes)
    };
    (copied == requested && bytes == requested).then(|| buffer.into_boxed_slice())
}
