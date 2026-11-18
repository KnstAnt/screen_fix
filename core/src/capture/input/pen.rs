//! Recorder rules for interpreting pen HID reports. The wire format only
//! validates the reports; these rules decide what they mean for held input.

use crate::protocol::ipc::{HidField, HidReport, HidReportResult, HidUsage, HidValue};

/// Whether `report` leaves the pen tip state unknown: a pen (or an
/// unidentified collection) whose report did not decode, or whose tip is not
/// a switch.
pub(crate) fn continuity_uncertain(report: &HidReport) -> bool {
    if report.usage_page.is_some() && !report.is_pen() {
        return false;
    }
    match &report.result {
        HidReportResult::InvalidExtent | HidReportResult::PreparsedUnavailable
        | HidReportResult::UnsupportedReport => true,
        HidReportResult::Report(decoded) => decoded.fields.as_slice().iter().any(|field| {
            field.usage == HidUsage::Tip && !matches!(field.value, HidValue::Switch(_))
        }),
        _ => false,
    }
}

/// The link collection and state of the single attested pen tip switch.
/// Distinguishes it from missing or multi-transducer evidence without
/// guessing which transducer moved.
pub(crate) fn tip(report: &HidReport) -> Option<(u16, bool)> {
    if !report.is_pen() {
        return None;
    }
    let HidReportResult::Report(decoded) = &report.result else { return None };
    let mut tips = decoded.fields.as_slice().iter().filter(|field| field.usage == HidUsage::Tip);
    match (tips.next(), tips.next()) {
        (Some(HidField { link, value: HidValue::Switch(down), .. }), None) => Some((*link, *down)),
        _ => None,
    }
}
