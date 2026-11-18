use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::protocol::{
    SessionTimeUs,
    ipc::{DecodedHidReport, ForegroundAtReceipt, HidUsage, HidValue},
};

use super::{number, switch, unique};

const MAX_CONTACTS: usize = 32;
/// The longest span of one multi-report touchpad frame from its first report.
const MAX_FRAME_SPAN_US: i64 = 100_000;

pub(super) struct Frame {
    scan_time: i64,
    expected: usize,
    proof: ForegroundAtReceipt,
    first_receipt: SessionTimeUs,
    first_reference: Option<String>,
    ids: BTreeSet<i64>,
    contacts: Vec<Value>,
    buttons: [Option<bool>; 3],
    active_contact: bool,
}

pub(super) fn observe(
    pending: &mut Option<Frame>, report: &DecodedHidReport, at: SessionTimeUs,
    reference: Option<&str>, proof: ForegroundAtReceipt,
) -> (Value, bool) {
    let scan = unique_number(report, HidUsage::ScanTime);
    let count = unique_number(report, HidUsage::ContactCount);
    let (Some(scan), Some(count)) = (scan, count.and_then(|count| usize::try_from(count).ok())) else {
        *pending = None;
        return (json!({ "status": "unavailable_scan_time_or_contact_count" }), false);
    };
    if count > MAX_CONTACTS {
        *pending = None;
        return (json!({ "status": "unsupported_contact_count" }), false);
    }
    let interrupted = pending.as_ref().is_some_and(|old| {
        old.scan_time != scan || old.proof != proof || count != 0
            || at.0.saturating_sub(old.first_receipt.0) > MAX_FRAME_SPAN_US
    });
    if interrupted {
        *pending = None;
    }
    if count != 0 {
        *pending = Some(Frame {
            scan_time: scan, expected: count, proof, first_receipt: at, first_reference: reference.map(str::to_owned),
            ids: BTreeSet::new(), contacts: Vec::with_capacity(count), buttons: [None; 3], active_contact: false,
        });
    }
    let Some(frame) = pending.as_mut() else {
        return (json!({ "status": "unavailable_frame_start", "scan_time": scan }), false);
    };
    for (button, usage) in frame.buttons.iter_mut().zip([HidUsage::Button1, HidUsage::Button2, HidUsage::Button3]) {
        if let Some(active) = unique_switch(report, usage) {
            *button = Some(active);
        }
    }
    // Slots beyond Contact Count are padding, even when the descriptor
    // declares fields for all slots. A Tip Off inside the count is evidence.
    let mut links = BTreeMap::<u16, [Option<HidValue>; 5]>::new();
    for field in report.fields.as_slice() {
        let index = match field.usage {
            HidUsage::ContactId => 0,
            HidUsage::X => 1,
            HidUsage::Y => 2,
            HidUsage::Tip => 3,
            HidUsage::Confidence => 4,
            _ => continue,
        };
        links.entry(field.link).or_default()[index] = Some(field.value);
    }
    let ids = links.into_iter().filter(|(_, values)| values[0].is_some());
    for (link, [id, x, y, tip, confidence]) in ids {
        if frame.contacts.len() == frame.expected {
            break;
        }
        let Some(id) = id.and_then(number) else {
            *pending = None;
            return (json!({ "status": "unavailable_contact_id", "scan_time": scan }), false);
        };
        if !frame.ids.insert(id) {
            *pending = None;
            return (json!({ "status": "ambiguous_contact_id", "scan_time": scan }), false);
        }
        let tip = tip.and_then(switch);
        let confidence = confidence.and_then(switch);
        frame.active_contact |= tip == Some(true) && confidence == Some(true);
        frame.contacts.push(json!({
            "contact_id": id, "link_collection": link,
            "x": x.and_then(number), "y": y.and_then(number),
            "tip": tip, "confidence": confidence,
            "report_reference": reference,
        }));
    }
    if frame.contacts.len() != frame.expected {
        return (json!({
            "status": "awaiting_contacts", "scan_time": scan,
            "first_report_reference": frame.first_reference, "contact_count": frame.expected,
            "previous_frame_interrupted": interrupted,
        }), false);
    }
    let frame = pending.take().expect("complete pending touchpad frame");
    (json!({
        "status": "complete", "scan_time": scan,
        "first_recv_t_us": frame.first_receipt, "first_report_reference": frame.first_reference,
        "last_report_reference": reference, "contact_count": frame.expected,
        "contacts": frame.contacts, "buttons": frame.buttons,
        "previous_frame_interrupted": interrupted,
    }), frame.active_contact && !interrupted)
}

fn unique_number(report: &DecodedHidReport, usage: HidUsage) -> Option<i64> {
    unique(report, usage).and_then(number)
}

fn unique_switch(report: &DecodedHidReport, usage: HidUsage) -> Option<bool> {
    unique(report, usage).and_then(switch)
}
