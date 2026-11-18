//! Checked native report extents and preparsed allocations stay on the Raw Input owner.

mod ndof;

use std::collections::BTreeMap;

use screen_fix_protocol::{
    BoundedList, DeviceId,
    ipc::{DecodedHidReport, DeviceDescriptor, EXTERNAL_PEN_USAGE, HidAngleReading, HidCollectionEvidence, HidField, HidFieldDeclaration,
        HidProfile, HidReport, HidReportResult, HidUsage, HidValue, HidValueSpec, INTEGRATED_PEN_USAGE, OptionalCollection,
        TouchpadCapabilities, is_pen_usage,
        MAX_HID_FIELDS, MAX_HID_REPORT_BYTES, MAX_INPUT_DEVICES},
};
use windows_sys::Win32::{
    Devices::HumanInterfaceDevice::{
        HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_STATUS_NULL,
        HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, HidP_GetButtonCaps, HidP_GetCaps,
        HidP_GetScaledUsageValue, HidP_GetUsageValue, HidP_GetUsages, HidP_GetValueCaps,
        HidP_Input,
    },
    Foundation::HANDLE,
    UI::Input::{GetRawInputDeviceInfoW, RID_DEVICE_INFO, RIDI_DEVICEINFO, RIM_TYPEHID},
};

use super::device;

const HEADER_BYTES: usize = 8;
const MAX_REPORTS_PER_PACKET: usize = 256;
const MAX_NATIVE_CAPS: usize = 128;
const MAX_ACTIVE_USAGES: usize = 128;

#[derive(Default)]
pub(super) struct HidCollections {
    devices: BTreeMap<DeviceId, Option<Collection>>,
}

impl HidCollections {
    pub(super) fn observe(&mut self, handle: HANDLE, descriptor: &mut DeviceDescriptor, pen_enabled: bool) {
        let Some(usage) = descriptor.kind.hid_usage() else { return };
        let profile = match (usage, OptionalCollection::from_usage(usage)) {
            (EXTERNAL_PEN_USAGE, _) if pen_enabled => HidProfile::ExternalPen,
            (INTEGRATED_PEN_USAGE, _) if pen_enabled => HidProfile::IntegratedPen,
            (_, Some(OptionalCollection::Touchpad)) => HidProfile::PrecisionTouchpad,
            (_, Some(OptionalCollection::MultiAxis)) => HidProfile::UnsupportedMultiAxisController,
            _ => return,
        };
        let collection = if self.devices.contains_key(&descriptor.id) || self.devices.len() < MAX_INPUT_DEVICES {
            self.devices.entry(descriptor.id).or_insert_with(|| Collection::load(handle)).as_ref()
        } else {
            None
        };
        let Some(collection) = collection else { return };
        descriptor.preparsed_data_available = true;
        let profile = if collection.ndof.is_some() { HidProfile::SpaceMouse } else { profile };
        let fields = collection.fields.iter().take(MAX_HID_FIELDS).map(|field| HidFieldDeclaration {
            report_id: field.id, link_collection: field.link, usage: field.usage,
            numerical_range: match field.kind { FieldKind::Number(spec) => Some(spec), _ => None },
        }).collect();
        let Ok(fields) = BoundedList::new(fields) else { return };
        let pen = matches!(profile, HidProfile::ExternalPen | HidProfile::IntegratedPen);
        let missing = if pen {
            USAGES.iter().filter_map(|&(_, _, usage)| {
                ((numeric_usage(usage, false) || switch_usage(usage, false))
                    && !collection.fields.iter().any(|field| field.usage == usage)).then_some(usage)
            }).collect()
        } else {
            Vec::new()
        };
        let Ok(missing_fields) = BoundedList::new(missing) else { return };
        descriptor.hid = Some(HidCollectionEvidence {
            profile, fields, fields_truncated: collection.fields.len() > MAX_HID_FIELDS, missing_fields,
            // HidD_GetFeature has no deadline on this Raw Input owner.
            touchpad_capabilities: (profile == HidProfile::PrecisionTouchpad)
                .then_some(TouchpadCapabilities::UnavailableUnboundedDriverIo),
        });
    }

    pub(super) fn remove(&mut self, handle: HANDLE) {
        if let Some(device) = device::id(handle) {
            self.devices.remove(&device);
        }
    }

    pub(super) fn reports(
        &mut self,
        handle: HANDLE,
        payload: &[u8],
        pen_enabled: bool,
        mut emit: impl FnMut(HidReport),
    ) {
        let Some(device) = device::id(handle) else { return };
        if !self.devices.contains_key(&device) && self.devices.len() == MAX_INPUT_DEVICES {
            emit(report(device, None, 0, HidReportResult::PreparsedUnavailable));
            return;
        }
        let collection = self.devices.entry(device).or_insert_with(|| Collection::load(handle));
        let Some(collection) = collection else {
            emit(report(device, None, 0, HidReportResult::PreparsedUnavailable));
            return;
        };
        let usage = (collection.caps.UsagePage, collection.caps.Usage);
        let accepted = if is_pen_usage(usage) { pen_enabled } else { OptionalCollection::from_usage(usage).is_some() };
        if !accepted {
            emit(report(device, Some(usage), 0, HidReportResult::UnsupportedCollection));
            return;
        }
        let Some((length, bytes)) = report_extent(payload) else {
            emit(report(device, Some(usage), 0, HidReportResult::InvalidExtent));
            return;
        };
        if !collection.accepts_length(length) {
            emit(report(device, Some(usage), 0, HidReportResult::InvalidExtent));
            return;
        }
        for (index, bytes) in bytes.chunks_exact(length).enumerate() {
            let result = collection.decode(bytes);
            let mut sample = report(device, Some(usage), index as u32, result);
            sample.report_id = bytes.first().copied();
            emit(sample);
        }
    }
}

struct Collection {
    preparsed: Box<[u64]>,
    caps: HIDP_CAPS,
    fields: Vec<FieldCap>,
    known_report_ids: [bool; 256],
    ndof: Option<ndof::Profile>,
}

impl Collection {
    fn load(handle: HANDLE) -> Option<Self> {
        let preparsed = device::preparsed(handle)?;
        let mut caps = HIDP_CAPS::default();
        // The preparsed bytes came from the system for this Raw Input handle.
        let status = unsafe { HidP_GetCaps(preparsed.as_ptr() as _, &mut caps) };
        if status as u32 != HIDP_STATUS_SUCCESS as u32 {
            return None;
        }
        let multiaxis = (caps.UsagePage, caps.Usage) == OptionalCollection::MultiAxis.usage();
        let mut collection = Self { preparsed, caps, fields: Vec::new(), known_report_ids: [false; 256], ndof: None };
        if multiaxis {
            let mut bytes = std::mem::size_of::<RID_DEVICE_INFO>() as u32;
            let mut info = RID_DEVICE_INFO { cbSize: bytes, ..Default::default() };
            let copied = unsafe {
                GetRawInputDeviceInfoW(handle, RIDI_DEVICEINFO, (&mut info as *mut RID_DEVICE_INFO).cast(), &mut bytes)
            };
            if copied == std::mem::size_of::<RID_DEVICE_INFO>() as u32 && info.dwType == RIM_TYPEHID {
                let native = unsafe { info.Anonymous.hid };
                collection.ndof = ndof::Profile::identify(native.dwVendorId, native.dwProductId);
            }
        } else {
            collection.load_fields()?;
        }
        Some(collection)
    }

    fn load_fields(&mut self) -> Option<()> {
        let mut value_count = self.caps.NumberInputValueCaps;
        let mut button_count = self.caps.NumberInputButtonCaps;
        if usize::from(value_count) > MAX_NATIVE_CAPS || usize::from(button_count) > MAX_NATIVE_CAPS {
            return None;
        }
        let mut values = vec![HIDP_VALUE_CAPS::default(); value_count.into()];
        let mut buttons = vec![HIDP_BUTTON_CAPS::default(); button_count.into()];
        let preparsed = self.preparsed.as_ptr() as _;
        if value_count != 0 && unsafe { HidP_GetValueCaps(
            HidP_Input, values.as_mut_ptr(), &mut value_count, preparsed,
        ) } as u32 != HIDP_STATUS_SUCCESS as u32 {
            return None;
        }
        if button_count != 0 && unsafe { HidP_GetButtonCaps(
            HidP_Input, buttons.as_mut_ptr(), &mut button_count, preparsed,
        ) } as u32 != HIDP_STATUS_SUCCESS as u32 {
            return None;
        }
        let touchpad = (self.caps.UsagePage, self.caps.Usage) == OptionalCollection::Touchpad.usage();
        for cap in values.get(..usize::from(value_count))? {
            self.known_report_ids[usize::from(cap.ReportID)] = true;
            if cap.IsAlias {
                continue;
            }
            // IsRange is the Windows discriminator of the caps union.
            let [first, last] = unsafe {
                if cap.IsRange {
                    [cap.Anonymous.Range.UsageMin, cap.Anonymous.Range.UsageMax]
                } else {
                    [cap.Anonymous.NotRange.Usage; 2]
                }
            };
            for &(page, code, usage) in USAGES {
                if page != cap.UsagePage || !(first..=last).contains(&code) || !numeric_usage(usage, touchpad) {
                    continue;
                }
                let distinct = u16::try_from(u32::from(last).saturating_sub(u32::from(first)) + 1).ok();
                let spec = (distinct == Some(cap.ReportCount) && (1..=32).contains(&cap.BitSize)).then(|| HidValueSpec {
                    logical: limits(cap.LogicalMin, cap.LogicalMax, cap.BitSize),
                    physical: limits(cap.PhysicalMin, cap.PhysicalMax, cap.BitSize),
                    bits: cap.BitSize as u8,
                    unit: cap.Units,
                    exponent: ((cap.UnitsExp as u8 & 15) as i8) << 4 >> 4,
                });
                self.fields.push(FieldCap {
                    id: cap.ReportID, link: cap.LinkCollection, page, code, usage,
                    kind: spec.map_or(FieldKind::Unsupported, FieldKind::Number),
                });
            }
        }
        for cap in buttons.get(..usize::from(button_count))? {
            self.known_report_ids[usize::from(cap.ReportID)] = true;
            if cap.IsAlias {
                continue;
            }
            let [first, last] = unsafe {
                if cap.IsRange {
                    [cap.Anonymous.Range.UsageMin, cap.Anonymous.Range.UsageMax]
                } else {
                    [cap.Anonymous.NotRange.Usage; 2]
                }
            };
            for &(page, code, usage) in USAGES {
                if page != cap.UsagePage || !(first..=last).contains(&code) || !switch_usage(usage, touchpad) {
                    continue;
                }
                self.fields.push(FieldCap {
                    id: cap.ReportID, link: cap.LinkCollection, page, code, usage, kind: FieldKind::Switch,
                });
            }
        }
        self.fields.sort_unstable_by_key(|field| (field.id, field.link, field.usage));
        Some(())
    }

    fn accepts_length(&self, length: usize) -> bool {
        let maximum = usize::from(self.caps.InputReportByteLength);
        if (self.caps.UsagePage, self.caps.Usage) == OptionalCollection::MultiAxis.usage() {
            (2..=maximum).contains(&length)
        } else {
            length == maximum
        }
    }

    fn decode(&self, bytes: &[u8]) -> HidReportResult {
        if (self.caps.UsagePage, self.caps.Usage) == OptionalCollection::MultiAxis.usage() {
            return self.ndof.and_then(|profile| profile.decode(bytes))
                .map_or(HidReportResult::UnsupportedReport, HidReportResult::Ndof);
        }
        let id = bytes[0]; // HID parser buffers always reserve the leading Report ID byte.
        let start = self.fields.partition_point(|cap| cap.id < id);
        let end = self.fields.partition_point(|cap| cap.id <= id);
        let fields = &self.fields[start..end];
        if fields.is_empty() && self.known_report_ids[usize::from(id)] {
            return HidReportResult::NoRelevantFields;
        }
        if fields.is_empty() || fields.len() > MAX_HID_FIELDS {
            return HidReportResult::UnsupportedReport;
        }
        let mut decoded = Vec::with_capacity(fields.len());
        for cap in fields {
            if decoded.last().is_some_and(|previous: &HidField| previous.link == cap.link && previous.usage == cap.usage) {
                if let Some(previous) = decoded.last_mut() {
                    previous.value = HidValue::Unavailable;
                }
                continue;
            }
            // HidP interprets link zero as any collection. An equal usage in
            // another link makes the root lookup ambiguous.
            let ambiguous_root = cap.link == 0 && fields.iter().any(|other|
                other.link != 0 && other.page == cap.page && other.code == cap.code);
            let value = if ambiguous_root {
                HidValue::Unavailable
            } else {
                self.read_field(*cap, bytes)
            };
            decoded.push(HidField { link: cap.link, usage: cap.usage, value });
        }
        match BoundedList::new(decoded) {
            Ok(fields) => HidReportResult::Report(DecodedHidReport { id, fields }),
            Err(_) => HidReportResult::UnsupportedReport,
        }
    }

    fn read_field(&self, cap: FieldCap, bytes: &[u8]) -> HidValue {
        let preparsed = self.preparsed.as_ptr() as _;
        let report = bytes.as_ptr() as _;
        let length = bytes.len() as u32;
        match cap.kind {
            FieldKind::Unsupported => HidValue::Unavailable,
            FieldKind::Switch => {
                let mut active = [0_u16; MAX_ACTIVE_USAGES];
                let mut count = active.len() as u32;
                let status = unsafe { HidP_GetUsages(
                    HidP_Input, cap.page, cap.link, active.as_mut_ptr(), &mut count, preparsed, report, length,
                ) };
                if status as u32 == HIDP_STATUS_SUCCESS as u32 {
                    if let Some(active) = active.get(..count as usize) {
                        return HidValue::Switch(active.contains(&cap.code));
                    }
                }
                HidValue::Unavailable
            }
            FieldKind::Number(spec) => {
                let angular = matches!(cap.usage, HidUsage::TiltX | HidUsage::TiltY | HidUsage::Twist);
                let mut scaled = 0;
                let scaled_status = unsafe { HidP_GetScaledUsageValue(
                    HidP_Input, cap.page, cap.link, cap.code, &mut scaled, preparsed, report, length,
                ) };
                if scaled_status as u32 == HIDP_STATUS_NULL as u32 {
                    return if angular {
                        HidValue::Angle { raw: None, reading: None, spec }
                    } else {
                        HidValue::Number { raw: None, spec }
                    };
                }
                // Any other scaled status still leaves the logical value
                // readable; an unsigned value is never lost to it (I6).
                let mut raw = 0;
                let status = unsafe { HidP_GetUsageValue(
                    HidP_Input, cap.page, cap.link, cap.code, &mut raw, preparsed, report, length,
                ) };
                if status as u32 != HIDP_STATUS_SUCCESS as u32 {
                    return HidValue::Unavailable;
                }
                let logical = if spec.logical[0] < 0 { signed(raw, spec.bits) } else { i64::from(raw) };
                if angular {
                    let reading = if scaled_status as u32 == HIDP_STATUS_SUCCESS as u32 {
                        HidAngleReading::ScaledPhysical(scaled)
                    } else {
                        HidAngleReading::SignedLogicalFallback(signed(raw, spec.bits))
                    };
                    HidValue::Angle { raw: Some(logical), reading: Some(reading), spec }
                } else {
                    HidValue::Number { raw: Some(logical), spec }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct FieldCap {
    id: u8,
    link: u16,
    page: u16,
    code: u16,
    usage: HidUsage,
    kind: FieldKind,
}

#[derive(Clone, Copy)]
enum FieldKind {
    Switch,
    Number(HidValueSpec),
    Unsupported,
}

const USAGES: &[(u16, u16, HidUsage)] = &[
    (1, 0x30, HidUsage::X), (1, 0x31, HidUsage::Y),
    (0x0D, 0x30, HidUsage::Pressure), (0x0D, 0x32, HidUsage::InRange),
    (0x0D, 0x3D, HidUsage::TiltX), (0x0D, 0x3E, HidUsage::TiltY), (0x0D, 0x41, HidUsage::Twist),
    (0x0D, 0x42, HidUsage::Tip), (0x0D, 0x44, HidUsage::Barrel),
    (0x0D, 0x45, HidUsage::Eraser), (0x0D, 0x3C, HidUsage::Invert),
    (0x0D, 0x5A, HidUsage::SecondaryBarrel), (0x0D, 0x56, HidUsage::ScanTime),
    (0x0D, 0x51, HidUsage::ContactId), (0x0D, 0x54, HidUsage::ContactCount),
    (0x0D, 0x47, HidUsage::Confidence),
    (9, 1, HidUsage::Button1), (9, 2, HidUsage::Button2), (9, 3, HidUsage::Button3),
];

fn numeric_usage(usage: HidUsage, touchpad: bool) -> bool {
    match usage {
        HidUsage::X | HidUsage::Y | HidUsage::ScanTime => true,
        HidUsage::ContactId | HidUsage::ContactCount => touchpad,
        HidUsage::Pressure | HidUsage::TiltX | HidUsage::TiltY | HidUsage::Twist => !touchpad,
        _ => false,
    }
}

fn switch_usage(usage: HidUsage, touchpad: bool) -> bool {
    match usage {
        HidUsage::Tip => true,
        HidUsage::Confidence | HidUsage::Button1 | HidUsage::Button2 | HidUsage::Button3 => touchpad,
        HidUsage::InRange | HidUsage::Barrel | HidUsage::SecondaryBarrel | HidUsage::Invert | HidUsage::Eraser => !touchpad,
        _ => false,
    }
}

fn signed(raw: u32, bits: u8) -> i64 {
    let shift = 32 - bits;
    i64::from((raw << shift) as i32 >> shift)
}

fn limits(min: i32, max: i32, bits: u16) -> [i64; 2] {
    let max = if bits == 32 && min >= 0 { i64::from(max as u32) } else { i64::from(max) };
    [i64::from(min), max]
}

fn report_extent(payload: &[u8]) -> Option<(usize, &[u8])> {
    let length = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?) as usize;
    let count = u32::from_le_bytes(payload.get(4..HEADER_BYTES)?.try_into().ok()?) as usize;
    if !(1..=MAX_HID_REPORT_BYTES).contains(&length) || !(1..=MAX_REPORTS_PER_PACKET).contains(&count) {
        return None;
    }
    let end = HEADER_BYTES.checked_add(length.checked_mul(count)?)?;
    Some((length, payload.get(HEADER_BYTES..end)?))
}

fn report(
    device: DeviceId,
    collection: Option<(u16, u16)>,
    sub_index: u32,
    result: HidReportResult,
) -> HidReport {
    let (usage_page, usage) = collection.unzip();
    HidReport { device, usage_page, usage, sub_index, report_id: None, result }
}
