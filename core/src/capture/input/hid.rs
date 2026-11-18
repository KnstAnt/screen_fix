//! Approved extended-input publication and device-clock fitting.

mod touchpad;

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Value, json};

use crate::protocol::{
    DeviceId, SessionTimeUs,
    ipc::{
        DecodedHidReport, ForegroundAtReceipt, HidAngleReading, HidReport, HidReportResult, HidUsage, HidValue,
        HidValueSpec, MAX_INPUT_DEVICES, NdofData, OptionalCollection,
    },
};

const FIT_SAMPLES: usize = 32;
const MAX_NDOF_BUTTON_CANDIDATES: usize = 32;
const NDOF_KEY_WINDOW_US: i64 = 50_000;
const MAX_INITIAL_INTERVAL_US: i64 = 100_000;

#[derive(Default)]
pub(crate) struct HidPublication {
    clocks: BTreeMap<DeviceId, ScanClock>,
    frames: BTreeMap<DeviceId, touchpad::Frame>,
    latest_contacts: BTreeMap<DeviceId, TouchpadContact>,
    ndof_masks: BTreeMap<DeviceId, u64>,
    ndof_buttons: VecDeque<NdofButton>,
    ndof_overflow_at: Option<SessionTimeUs>,
}

impl HidPublication {
    /// Forget the touchpad and multi-axis state, including the scan clocks of
    /// those devices, without touching pen state.
    pub fn reset_optional(&mut self) {
        let devices: Vec<DeviceId> = self.frames.keys().chain(self.latest_contacts.keys()).chain(self.ndof_masks.keys())
            .chain(self.ndof_buttons.iter().map(|button| &button.device)).copied().collect();
        for device in devices {
            self.reset(Some(device));
        }
        self.ndof_overflow_at = None;
    }

    pub fn reset(&mut self, device: Option<DeviceId>) {
        if let Some(device) = device {
            self.clocks.remove(&device);
            self.frames.remove(&device);
            self.latest_contacts.remove(&device);
            self.ndof_masks.remove(&device);
            self.ndof_buttons.retain(|button| button.device != device);
        } else {
            self.clocks.clear();
            self.frames.clear();
            self.latest_contacts.clear();
            self.ndof_masks.clear();
            self.ndof_buttons.clear();
            self.ndof_overflow_at = None;
        }
    }

    pub fn observe_touchpad(
        &mut self, device: DeviceId, at: SessionTimeUs, proof: ForegroundAtReceipt,
        reference: Option<&str>, active: bool,
    ) {
        self.latest_contacts.remove(&device);
        if active && self.latest_contacts.len() < MAX_INPUT_DEVICES {
            self.latest_contacts.insert(device, TouchpadContact {
                at, proof, reference: reference.map(str::to_owned),
            });
        }
    }

    pub fn overlapping_contact(
        &self, device: DeviceId, at: SessionTimeUs, proof: ForegroundAtReceipt,
    ) -> Option<&TouchpadContact> {
        let contact = self.latest_contacts.get(&device)?;
        let gap = at.0.checked_sub(contact.at.0)?;
        // A short backward overlap, with no intervening loss or contact
        // report, avoids extrapolating beyond the approved contact prefix.
        (contact.proof == proof && (0..=12_000).contains(&gap)).then_some(contact)
    }

    pub fn ndof_key_candidates(&self, at: SessionTimeUs, proof: ForegroundAtReceipt) -> Option<Value> {
        let candidates: Vec<_> = self.ndof_buttons.iter().filter_map(|button| {
            let gap = at.0.checked_sub(button.at.0)?;
            ((0..=NDOF_KEY_WINDOW_US).contains(&gap) && button.proof == proof).then(|| {
                json!({ "device": button.device, "report_reference": button.reference, "gap_us": gap })
            })
        }).collect();
        let truncated = self.ndof_overflow_at.is_some_and(|lost| at.0.saturating_sub(lost.0) <= NDOF_KEY_WINDOW_US);
        (!candidates.is_empty() || truncated).then(|| json!({
            "status": "candidate_only", "rivals_present": candidates.len() > 1,
            "candidates": candidates, "truncated": truncated,
        }))
    }

    pub fn observe_published_ndof(
        &mut self, sample: &HidReport, at: SessionTimeUs, reference: Option<String>, proof: ForegroundAtReceipt,
    ) {
        let HidReportResult::Ndof(report) = &sample.result else { return };
        let button = match &report.data {
            NdofData::ButtonMask(mask) => {
                if self.ndof_masks.len() == MAX_INPUT_DEVICES && !self.ndof_masks.contains_key(&sample.device) {
                    return;
                }
                self.ndof_masks.insert(sample.device, *mask).is_some_and(|prior| *mask & !prior != 0)
            }
            NdofData::ShortPress(codes) | NdofData::LongPress(codes) => codes.as_slice().iter().any(|code| *code != 0),
            NdofData::Motion { .. } => false,
        };
        if !button {
            return;
        }
        self.ndof_buttons.retain(|event| at.0.saturating_sub(event.at.0) <= NDOF_KEY_WINDOW_US);
        if self.ndof_buttons.len() == MAX_NDOF_BUTTON_CANDIDATES {
            self.ndof_overflow_at = self.ndof_buttons.pop_front().map(|old| old.at);
        }
        self.ndof_buttons.push_back(NdofButton { device: sample.device, at, reference, proof });
    }

    pub fn scan_time(&mut self, sample: &HidReport, at: SessionTimeUs) -> Option<SessionTimeUs> {
        let HidReportResult::Report(report) = &sample.result else { return None };
        let mut scan = report.fields.as_slice().iter()
            .filter(|field| field.usage == HidUsage::ScanTime).map(|field| field.value);
        match (scan.next(), scan.next()) {
            (Some(HidValue::Number { raw: Some(raw), spec }), None) => self.map(sample.device, raw, spec, at),
            _ => None,
        }
    }

    pub fn row(
        &mut self, sample: &HidReport, at: SessionTimeUs, reference: Option<&str>,
        proof: ForegroundAtReceipt, source: Option<SessionTimeUs>,
    ) -> (Value, bool) {
        let kind = ReportKind::of(sample);
        match &sample.result {
            HidReportResult::Ndof(report) => (json!({
                "kind": "ndof", "report_id": report.id, "sub_index": sample.sub_index,
                "report_reference": reference, "device_kind": "spacemouse_profile", "data": report.data,
            }), false),
            HidReportResult::Report(report) => {
                let fields: Vec<_> = report.fields.as_slice().iter().map(|field| {
                    let mut published = json!({ "link_collection": field.link, "usage": field.usage });
                    match field.value {
                        HidValue::Switch(active) => published["value"] = json!(active),
                        HidValue::Unavailable => published["value_state"] = json!("unavailable"),
                        HidValue::Number { raw, spec } => {
                            published["value"] = json!(raw);
                            published["declared_range"] = json!(spec);
                            match field.usage {
                                HidUsage::Pressure => published["normalized"] = json!(raw.and_then(|raw| normalize(raw, spec.logical))),
                                _ => {}
                            }
                        }
                        HidValue::Angle { raw, reading, spec } => {
                            published["value"] = json!(raw);
                            published["declared_range"] = json!(spec);
                            published["angle_reading"] = json!(reading);
                            published["degrees"] = json!(reading.and_then(|reading| degrees(reading, spec)));
                        }
                    }
                    published
                }).collect();
                let mut row = json!({ "kind": kind.label(), "report_id": report.id, "sub_index": sample.sub_index,
                    "usage_page": sample.usage_page, "usage": sample.usage, "fields": fields,
                    "report_reference": reference,
                    "source_clock": source.map(|_| "hid_scan_time_sliding_minimum"),
                });
                if kind == ReportKind::Pen {
                    lift_pen(&mut row, report);
                }
                if kind == ReportKind::Touchpad {
                    let mut frame = self.frames.remove(&sample.device);
                    let (frame_row, active) = if self.frames.len() == MAX_INPUT_DEVICES {
                        (json!({ "status": "unavailable_capacity" }), false)
                    } else {
                        touchpad::observe(&mut frame, report, at, reference, proof)
                    };
                    row["contact_frame"] = frame_row;
                    if let Some(frame) = frame {
                        self.frames.insert(sample.device, frame);
                    }
                    (row, active)
                } else {
                    (row, false)
                }
            }
            result => {
                if !matches!(result, HidReportResult::NoRelevantFields) {
                    self.reset(Some(sample.device));
                }
                (json!({ "kind": kind.label(), "sub_index": sample.sub_index, "report": result,
                    "report_id": sample.report_id, "report_reference": reference,
                    "usage_page": sample.usage_page, "usage": sample.usage }), false)
            }
        }
    }

    fn map(&mut self, device: DeviceId, raw: i64, spec: HidValueSpec, at: SessionTimeUs) -> Option<SessionTimeUs> {
        if !self.clocks.contains_key(&device) && self.clocks.len() == MAX_INPUT_DEVICES {
            return None;
        }
        if let Some(clock) = self.clocks.get_mut(&device) && clock.spec == spec {
            return clock.map(raw, at);
        }
        // The first report of a new Scan Time base only starts the fit; its
        // receipt is never copied into source time (CLK-2).
        self.clocks.insert(device, ScanClock::start(raw, spec, at)?);
        None
    }
}

/// The collection of a HID report, which selects its row kind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportKind {
    Pen,
    Touchpad,
    MultiAxis,
    Unknown,
}

impl ReportKind {
    fn of(sample: &HidReport) -> Self {
        match sample.collection() {
            _ if sample.is_pen() => Self::Pen,
            Some(OptionalCollection::Touchpad) => Self::Touchpad,
            Some(OptionalCollection::MultiAxis) => Self::MultiAxis,
            None => Self::Unknown,
        }
    }

    /// The `kind` of a row that is not a decoded multi-axis report.
    fn label(self) -> &'static str {
        match self {
            Self::Pen => "pen",
            Self::Touchpad => "touchpad_report",
            Self::MultiAxis => "ndof_unavailable",
            Self::Unknown => "hid_unavailable",
        }
    }
}

pub(crate) struct TouchpadContact {
    pub at: SessionTimeUs,
    proof: ForegroundAtReceipt,
    pub reference: Option<String>,
}

struct NdofButton {
    device: DeviceId,
    at: SessionTimeUs,
    reference: Option<String>,
    proof: ForegroundAtReceipt,
}

/// INP-11 top-level pen keys lifted from the decoded fields. A usage the
/// report does not declare stays absent; one declared but unreadable or in
/// its null state is null. A usage declared in several link collections
/// (several transducers) is ambiguous and also stays absent. `fields` keeps
/// the per-link evidence as an extension.
fn lift_pen(row: &mut Value, report: &DecodedHidReport) {
    let field = |usage: HidUsage| unique(report, usage);
    for (key, usage) in [
        ("tip", HidUsage::Tip), ("in_range", HidUsage::InRange), ("barrel", HidUsage::Barrel),
        ("secondary_barrel", HidUsage::SecondaryBarrel), ("invert", HidUsage::Invert), ("eraser", HidUsage::Eraser),
    ] {
        if let Some(value) = field(usage) {
            row[key] = json!(switch(value));
        }
    }
    let (x, y) = (field(HidUsage::X), field(HidUsage::Y));
    if x.is_some() || y.is_some() {
        row["xy"] = json!([x.and_then(number), y.and_then(number)]);
    }
    if let Some(value) = field(HidUsage::Pressure) {
        let (raw, normalized) = match value {
            HidValue::Number { raw, spec } => (raw, raw.and_then(|raw| normalize(raw, spec.logical))),
            _ => (None, None),
        };
        row["pressure"] = json!(normalized);
        row["pressure_raw"] = json!(raw);
    }
    let (tilt_x, tilt_y) = (field(HidUsage::TiltX).map(angle), field(HidUsage::TiltY).map(angle));
    if tilt_x.is_some() || tilt_y.is_some() {
        let ((x, x_raw), (y, y_raw)) = (tilt_x.unwrap_or_default(), tilt_y.unwrap_or_default());
        row["tilt"] = json!([x, y]);
        row["tilt_raw"] = json!([x_raw, y_raw]);
    }
    if let Some((twist, raw)) = field(HidUsage::Twist).map(angle) {
        row["twist"] = json!(twist);
        row["twist_raw"] = json!(raw);
    }
    if let Some(value) = field(HidUsage::ScanTime) {
        row["scan_time"] = json!(number(value));
    }
}

/// The value of `usage` when the report declares it in exactly one field;
/// a usage declared in several link collections is ambiguous.
fn unique(report: &DecodedHidReport, usage: HidUsage) -> Option<HidValue> {
    let mut declared = report.fields.as_slice().iter().filter(|field| field.usage == usage);
    match (declared.next(), declared.next()) {
        (Some(field), None) => Some(field.value),
        _ => None,
    }
}

/// The logical value read with HidP_GetUsageValue; None in the null state.
fn number(value: HidValue) -> Option<i64> {
    match value {
        HidValue::Number { raw, .. } => raw,
        _ => None,
    }
}

/// The state of a button usage.
fn switch(value: HidValue) -> Option<bool> {
    match value {
        HidValue::Switch(active) => Some(active),
        _ => None,
    }
}

/// Signed degrees and the signed reading they come from: the scaled
/// physical value, or the sign-extended logical value when scaling failed.
fn angle(value: HidValue) -> (Option<f64>, Option<i64>) {
    match value {
        HidValue::Angle { reading: Some(reading), spec, .. } => (degrees(reading, spec), Some(match reading {
            HidAngleReading::ScaledPhysical(scaled) => i64::from(scaled),
            HidAngleReading::SignedLogicalFallback(signed) => signed,
        })),
        _ => (None, None),
    }
}

/// `raw` scaled to 0..=1 over a non-empty logical range that contains it.
pub(crate) fn normalize(raw: i64, [min, max]: [i64; 2]) -> Option<f64> {
    (max > min && (min..=max).contains(&raw)).then(|| (raw - min) as f64 / (max - min) as f64)
}

fn degrees(reading: HidAngleReading, spec: HidValueSpec) -> Option<f64> {
    let scaled = match reading {
        HidAngleReading::ScaledPhysical(physical) => f64::from(physical),
        HidAngleReading::SignedLogicalFallback(raw) => {
            let [min, max] = spec.physical;
            if max > min {
                min as f64 + normalize(raw, spec.logical)? * (max - min) as f64
            } else if spec.physical == [0, 0] && (spec.logical[0]..=spec.logical[1]).contains(&raw) {
                raw as f64
            } else {
                return None;
            }
        }
    };
    let scaled = scaled * 10_f64.powi(i32::from(spec.exponent));
    match spec.unit {
        0x14 => Some(scaled), // English Rotation, degrees.
        0x12 => Some(scaled.to_degrees()), // SI Rotation, radians.
        _ => None,
    }
}

struct ScanClock {
    spec: HidValueSpec,
    raw: i64,
    unwrapped: i128,
    at: SessionTimeUs,
    period_us: Option<i64>,
    offsets: VecDeque<i128>,
}

impl ScanClock {
    fn start(raw: i64, spec: HidValueSpec, at: SessionTimeUs) -> Option<Self> {
        scan_extent(raw, spec)?;
        let mut offsets = VecDeque::with_capacity(FIT_SAMPLES);
        offsets.push_back(i128::from(at.0));
        Some(Self { spec, raw, unwrapped: 0, at, period_us: None, offsets })
    }

    fn map(&mut self, raw: i64, at: SessionTimeUs) -> Option<SessionTimeUs> {
        let (modulus, tick_us) = scan_extent(raw, self.spec)?;
        let dt = at.0.checked_sub(self.at.0)?;
        let delta = (i128::from(raw) - i128::from(self.raw)).rem_euclid(modulus);
        let elapsed = delta.checked_mul(tick_us)?;
        let gap = self.period_us.map_or(MAX_INITIAL_INTERVAL_US, |period| period.saturating_mul(3));
        // A large forward modular delta may be a device reset. Once an
        // interval is observed, either clock can force a new base.
        if dt < 0 || dt > gap || elapsed > i128::from(gap) {
            *self = Self::start(raw, self.spec, at)?;
            return None;
        }
        if elapsed != 0 {
            let elapsed = i64::try_from(elapsed).ok()?;
            self.period_us = Some(self.period_us.map_or(elapsed, |period| {
                (period.saturating_mul(7).saturating_add(elapsed) / 8).max(1)
            }));
        }
        self.raw = raw;
        self.at = at;
        self.unwrapped = self.unwrapped.checked_add(elapsed)?;
        let offset = i128::from(at.0).checked_sub(self.unwrapped)?;
        if self.offsets.len() == FIT_SAMPLES {
            self.offsets.pop_front();
        }
        self.offsets.push_back(offset);
        let fit = self.unwrapped.checked_add(*self.offsets.iter().min()?)?;
        Some(SessionTimeUs(i64::try_from(fit).ok()?))
    }
}

fn scan_extent(raw: i64, spec: HidValueSpec) -> Option<(i128, i128)> {
    let [min, max] = spec.logical;
    if min < 0 || max <= min || !(min..=max).contains(&raw) || !(1..=32).contains(&spec.bits)
        || i128::from(max) >= (1_i128 << spec.bits) {
        return None;
    }
    // A declared SI time unit carries the descriptor exponent in seconds.
    // Unitless descriptors have no established conversion.
    let power = u32::try_from(6_i32 + i32::from(spec.exponent)).ok()?;
    let tick_us = (spec.unit == 0x1001).then(|| 10_i128.checked_pow(power)).flatten()?;
    Some((i128::from(max) - i128::from(min) + 1, tick_us))
}
