//! The fixed vocabulary of journal rows that the writer and the reader share:
//! row kinds, the row counts a stream summary records, and the typed row parts
//! written by several owners. Rows still travel as `serde_json::Value`; key
//! order on disk is the sorted order of its map, so a typed part written into
//! a row keeps the row's bytes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, de::IntoDeserializer};
use serde_json::Value;

use crate::protocol::{
    AdmissionEpoch, CoarseTime, MessageTickMs, RecordTime, SessionTimeUs, StreamId, UncertaintyUs, ipc::Producer,
};

use super::{
    journal::StreamSummary,
    outcome::{GapRecord, LossReason, UiaOutcome, UiaRead},
};

/// The `kind` of a journal row that the package interprets. Frames rows have
/// no kind; every other kind a producer writes parses as `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RowKind {
    Gap,
    /// A context or stills health condition.
    Health,
    /// An input health condition, or the per-second input counts (INP-1, INP-12).
    InputHealth,
    Pause,
    Resume,
    Discontinuity,
    RecorderControl,
    RecorderControlGap,
    InputSourceLoss,
    Foreground,
    Blind,
    SuppressedInterval,
    Still,
    StillDropped,
    CursorAsset,
    Focus,
    Element,
    KeyDown,
    KeyUp,
    PointerDown,
    PointerUp,
    PointerMove,
    Scroll,
    Pen,
    /// A kind the package does not interpret. It is never written.
    #[serde(other, skip_serializing)]
    Other,
}

impl RowKind {
    /// The kind named by a row's `kind` string.
    pub(super) fn parse(kind: &str) -> Self {
        Self::deserialize(IntoDeserializer::<serde::de::value::Error>::into_deserializer(kind)).unwrap_or(Self::Other)
    }

    /// The kind of `stream`'s health condition rows: input health conditions
    /// are `input_health` rows, like the per-second input counts (INP-1, INP-12).
    pub(super) fn health(stream: StreamId) -> Self {
        if stream == StreamId::Input { Self::InputHealth } else { Self::Health }
    }

    /// An input row of a user action that a still can reference (gate 2).
    pub(super) fn is_action(self) -> bool {
        matches!(self, Self::KeyDown | Self::KeyUp | Self::PointerDown | Self::PointerUp | Self::PointerMove
            | Self::Scroll | Self::Pen)
    }

    /// The boundary, loss, health and suppression kinds a `stream` journal may
    /// hold inside a pause interval (gate 4). The gate itself judges a context
    /// `pause`, `resume` and unapproved `foreground` row.
    pub(super) fn allowed_in_pause(self, stream: StreamId) -> bool {
        match stream {
            StreamId::Context => matches!(self, Self::Discontinuity | Self::Gap | Self::Health | Self::RecorderControl
                | Self::RecorderControlGap | Self::InputSourceLoss),
            StreamId::Input => matches!(self, Self::Gap | Self::Blind | Self::InputHealth | Self::SuppressedInterval),
            StreamId::Frames | StreamId::Stills => false,
        }
    }

    /// How the stream summary counts a row of this kind.
    pub(super) fn tally(self, value: &Value) -> RowTally {
        match self {
            Self::Pause => RowTally::Pause,
            Self::Blind => RowTally::Blind,
            Self::SuppressedInterval => RowTally::Suppressed(suppressed_us(value)),
            _ => Unavailable::of(self, value).map_or(RowTally::Plain, RowTally::Unavailable),
        }
    }
}

/// The duration of a `suppressed_interval` row, as the summary counts it.
fn suppressed_us(value: &Value) -> u64 {
    value.get("end_t_us").and_then(Value::as_i64).zip(value.get("start_t_us").and_then(Value::as_i64))
        .map_or(0, |(end, start)| end.saturating_sub(start).max(0) as u64)
}

/// How the stream summary counts one data row.
#[derive(Debug)]
pub(super) enum RowTally {
    Plain,
    Pause,
    Blind,
    /// A suppression interval with its duration in microseconds.
    Suppressed(u64),
    Unavailable(Unavailable),
}

impl RowTally {
    /// The tally of an encoded row, by its `kind` string.
    pub(super) fn of(value: &Value) -> Self {
        value.get("kind").and_then(Value::as_str).map_or(Self::Plain, |kind| RowKind::parse(kind).tally(value))
    }
}

/// A row that records an unavailable observation, for the rows whose stream
/// defines an explicit outcome: a dropped still by its reason, and a focus or
/// element read whose `uia` value is an outcome instead of an element.
#[derive(Debug)]
pub(super) enum Unavailable {
    Still(Box<str>),
    Uia(UiaRead, UiaOutcome),
}

impl Unavailable {
    fn of(kind: RowKind, value: &Value) -> Option<Self> {
        let read = match kind {
            RowKind::StillDropped => return Some(Self::Still(
                value.get("reason").and_then(Value::as_str).unwrap_or("unspecified").into())),
            RowKind::Focus => UiaRead::Focus,
            RowKind::Element => UiaRead::Element,
            _ => return None,
        };
        UiaOutcome::deserialize(value.get("uia")?).ok().map(|outcome| Self::Uia(read, outcome))
    }
}

/// The row counts of one stream that its summary records and a journal scan
/// reproduces (E3). The writer and the reader count rows only through this type.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RowCounts {
    /// Per reason: the sum of known gap counts and the number of unknown-count gap rows.
    dropped: BTreeMap<LossReason, (u64, u64)>,
    unavailable: BTreeMap<String, u64>,
    uia: BTreeMap<UiaRead, BTreeMap<UiaOutcome, u64>>,
    health: u64,
    pauses: u64,
    blind: u64,
    suppressed_rows: u64,
    suppressed_us: u64,
}

impl RowCounts {
    /// The counts `summary` records. A reason without gap rows is not counted:
    /// its entry holds only privacy-sensitive totals.
    pub(super) fn of(summary: &StreamSummary) -> Self {
        Self {
            dropped: summary.dropped_by_reason.iter()
                .filter(|(_, total)| total.gap_items != 0 || total.unknown_gaps != 0)
                .map(|(reason, total)| (*reason, (total.gap_items, total.unknown_gaps)))
                .collect(),
            unavailable: summary.unavailable_by_reason.clone(),
            uia: summary.uia_unavailable.clone(),
            health: summary.health_rows,
            pauses: summary.pause_rows,
            blind: summary.blind_rows,
            suppressed_rows: summary.suppressed_interval_rows,
            suppressed_us: summary.suppressed_interval_us,
        }
    }

    /// Record these counts in `summary`, whose other totals stay as they are.
    pub(super) fn record_in(&self, summary: &mut StreamSummary) {
        for (reason, (items, unknown)) in &self.dropped {
            let total = summary.dropped_by_reason.entry(*reason).or_default();
            total.gap_items = *items;
            total.unknown_gaps = *unknown;
        }
        summary.unavailable_by_reason.clone_from(&self.unavailable);
        summary.uia_unavailable.clone_from(&self.uia);
        summary.health_rows = self.health;
        summary.pause_rows = self.pauses;
        summary.blind_rows = self.blind;
        summary.suppressed_interval_rows = self.suppressed_rows;
        summary.suppressed_interval_us = self.suppressed_us;
    }

    /// The scanned counts match `summary`, and its terminal outcomes reconcile
    /// with `offered` unless a loss count is unknown.
    pub(super) fn reconciles(&self, summary: &StreamSummary) -> bool {
        *self == Self::of(summary) && summary.outcome_total().is_none_or(|total| total == summary.offered)
    }

    pub(super) fn count_row(&mut self, tally: &RowTally) {
        match tally {
            RowTally::Plain => {}
            RowTally::Pause => self.pauses += 1,
            RowTally::Blind => self.blind += 1,
            RowTally::Suppressed(duration) => {
                self.suppressed_rows += 1;
                self.suppressed_us = self.suppressed_us.saturating_add(*duration);
            }
            RowTally::Unavailable(Unavailable::Still(reason)) => {
                *self.unavailable.entry(reason.to_string()).or_default() += 1;
            }
            RowTally::Unavailable(Unavailable::Uia(read, outcome)) => {
                *self.uia.entry(*read).or_default().entry(*outcome).or_default() += 1;
            }
        }
    }

    /// Count one health condition row.
    pub(super) fn count_health(&mut self) {
        self.health += 1;
    }

    pub(super) fn count_gap(&mut self, gap: &GapRecord) {
        let total = self.dropped.entry(gap.reason).or_default();
        match gap.count {
            Some(count) => total.0 = total.0.saturating_add(count),
            None => total.1 = total.1.saturating_add(1),
        }
    }
}

/// An input blind interval row (INP-13). `reason` is the interval's reason
/// label; `producers` names the producers whose evidence is missing and is
/// absent when all input is.
#[derive(Debug, Serialize)]
pub(crate) struct BlindRow<'a, R> {
    kind: RowKind,
    reason: R,
    start_us: SessionTimeUs,
    end_us: SessionTimeUs,
    #[serde(skip_serializing_if = "<[Producer]>::is_empty")]
    producers: &'a [Producer],
}

impl<'a, R: Serialize> BlindRow<'a, R> {
    pub(crate) fn new(reason: R, start_us: SessionTimeUs, end_us: SessionTimeUs, producers: &'a [Producer]) -> Self {
        Self { kind: RowKind::Blind, reason, start_us, end_us, producers }
    }

    pub(crate) fn value(&self) -> serde_json::Result<Value> {
        serde_json::to_value(self)
    }
}

/// How typed fields meet the fields a row already has.
#[derive(Clone, Copy)]
enum Merge {
    Replace,
    /// A field the row already has keeps its value.
    Keep,
}

/// Write the serialized struct `fields` into the object `row`.
fn merge(row: &mut Value, fields: impl Serialize, merge: Merge) -> serde_json::Result<()> {
    use serde::ser::Error as _;
    let row = row.as_object_mut().ok_or_else(|| serde_json::Error::custom("A journal row is not an object"))?;
    let Value::Object(fields) = serde_json::to_value(fields)? else {
        return Err(serde_json::Error::custom("Row fields are not an object"));
    };
    for (name, value) in fields {
        match merge {
            Merge::Replace => { row.insert(name, value); }
            Merge::Keep => { row.entry(name).or_insert(value); }
        }
    }
    Ok(())
}

/// The coarse Windows message tick of a row and whether the row arrived late
/// against it. Without a tick every field is null.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct MessageTick {
    tick_ms: Option<MessageTickMs>,
    tick_uncertainty_us: Option<UncertaintyUs>,
    late: Option<bool>,
}

impl MessageTick {
    pub(crate) fn new(coarse: Option<CoarseTime>, late: Option<bool>) -> Self {
        Self { tick_ms: coarse.map(|time| time.tick_ms), tick_uncertainty_us: coarse.map(|time| time.uncertainty_us), late }
    }

    /// Write the tick into `row`, replacing a tick it has.
    pub(crate) fn write(self, row: &mut Value) -> serde_json::Result<()> {
        merge(row, self, Merge::Replace)
    }
}

/// The timing envelope of an observed row (X5): its source time and
/// uncertainty, its message tick and its admission epoch. The journal writer
/// stamps `recv_t_us` from the row's admission receipt (`stamp_receipt`), so
/// the envelope does not carry it.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct RowEnvelope {
    source_t_us: Option<SessionTimeUs>,
    source_uncertainty_us: Option<UncertaintyUs>,
    admission_epoch: AdmissionEpoch,
    #[serde(skip)]
    tick: MessageTick,
}

impl RowEnvelope {
    pub(crate) fn new(time: RecordTime, late: Option<bool>, admission_epoch: AdmissionEpoch) -> Self {
        Self {
            source_t_us: time.source_t_us, source_uncertainty_us: time.source_uncertainty_us, admission_epoch,
            tick: MessageTick::new(time.coarse, late),
        }
    }

    /// Write the envelope into `row`, replacing the fields it has.
    pub(crate) fn write(self, row: &mut Value) -> serde_json::Result<()> {
        merge(row, self, Merge::Replace)?;
        self.tick.write(row)
    }

    /// Write the envelope into `row`, keeping a message tick the row already
    /// carries: a context row stamped with its WinEvent tick.
    pub(crate) fn write_keeping_tick(self, row: &mut Value) -> serde_json::Result<()> {
        merge(row, self, Merge::Replace)?;
        merge(row, self.tick, Merge::Keep)
    }
}

/// The receipt the journal writer stamps on a row.
#[derive(Serialize)]
struct Received {
    recv_t_us: SessionTimeUs,
}

/// The source time of a row without one.
#[derive(Serialize)]
struct Unsourced {
    source_t_us: Option<SessionTimeUs>,
}

/// Stamp the admission `receipt` on an encoded row, with a null `source_t_us`
/// unless the row has a source time.
pub(super) fn stamp_receipt(row: &mut Value, receipt: SessionTimeUs) -> serde_json::Result<()> {
    merge(row, Received { recv_t_us: receipt }, Merge::Replace)?;
    merge(row, Unsourced { source_t_us: None }, Merge::Keep)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::package::metadata::InputBlindReason;

    fn coarse(tick: u32) -> CoarseTime {
        CoarseTime { tick_ms: MessageTickMs(tick), uncertainty_us: UncertaintyUs(16_000) }
    }

    fn written(mut row: Value, write: impl FnOnce(&mut Value) -> serde_json::Result<()>) -> serde_json::Result<String> {
        write(&mut row)?;
        serde_json::to_string(&row)
    }

    /// An input row's envelope (gate.rs): every field is set, null when absent.
    #[test]
    fn envelope_bytes() -> serde_json::Result<()> {
        let time = RecordTime {
            recv_t_us: SessionTimeUs(50), source_t_us: Some(SessionTimeUs(30)),
            source_uncertainty_us: Some(UncertaintyUs(2)), coarse: Some(coarse(9)),
        };
        let envelope = RowEnvelope::new(time, Some(true), AdmissionEpoch(4));
        assert_eq!(written(json!({ "kind": "key_down", "late": false, "tick_ms": 1 }), |row| envelope.write(row))?,
            r#"{"admission_epoch":4,"kind":"key_down","late":true,"source_t_us":30,"source_uncertainty_us":2,"tick_ms":9,"tick_uncertainty_us":16000}"#);
        let bare = RecordTime { recv_t_us: SessionTimeUs(50), source_t_us: None, source_uncertainty_us: None, coarse: None };
        let envelope = RowEnvelope::new(bare, None, AdmissionEpoch(1));
        assert_eq!(written(json!({ "kind": "scroll" }), |row| envelope.write(row))?,
            r#"{"admission_epoch":1,"kind":"scroll","late":null,"source_t_us":null,"source_uncertainty_us":null,"tick_ms":null,"tick_uncertainty_us":null}"#);
        Ok(())
    }

    /// A context row's envelope (input.rs): its own WinEvent tick is kept,
    /// and a row without one gets the receipt tick and a null `late`.
    #[test]
    fn context_envelope_bytes() -> serde_json::Result<()> {
        let time = RecordTime { recv_t_us: SessionTimeUs(70), source_t_us: None, source_uncertainty_us: None, coarse: Some(coarse(9)) };
        let envelope = RowEnvelope::new(time, None, AdmissionEpoch(2));
        let stamped = json!({ "kind": "window_shown", "late": false, "tick_ms": 5, "tick_uncertainty_us": 16000 });
        assert_eq!(written(stamped, |row| envelope.write_keeping_tick(row))?,
            r#"{"admission_epoch":2,"kind":"window_shown","late":false,"source_t_us":null,"source_uncertainty_us":null,"tick_ms":5,"tick_uncertainty_us":16000}"#);
        assert_eq!(written(json!({ "kind": "display", "source_t_us": 3 }), |row| envelope.write_keeping_tick(row))?,
            r#"{"admission_epoch":2,"kind":"display","late":null,"source_t_us":null,"source_uncertainty_us":null,"tick_ms":9,"tick_uncertainty_us":16000}"#);
        let empty = RowEnvelope::new(RecordTime { coarse: None, ..time }, None, AdmissionEpoch(2));
        assert_eq!(written(json!({ "kind": "focus" }), |row| empty.write_keeping_tick(row))?,
            r#"{"admission_epoch":2,"kind":"focus","late":null,"source_t_us":null,"source_uncertainty_us":null,"tick_ms":null,"tick_uncertainty_us":null}"#);
        Ok(())
    }

    /// A WinEvent tick (context.rs) replaces only the tick fields.
    #[test]
    fn message_tick_bytes() -> serde_json::Result<()> {
        let tick = MessageTick::new(Some(coarse(12)), None);
        assert_eq!(written(json!({ "kind": "window_title", "win_event_recv_t_us": 8 }), |row| tick.write(row))?,
            r#"{"kind":"window_title","late":null,"tick_ms":12,"tick_uncertainty_us":16000,"win_event_recv_t_us":8}"#);
        Ok(())
    }

    /// The journal stamp: `recv_t_us` always, a null `source_t_us` only when absent.
    #[test]
    fn receipt_stamp_bytes() -> serde_json::Result<()> {
        assert_eq!(written(json!({ "kind": "gap", "recv_t_us": 1 }), |row| stamp_receipt(row, SessionTimeUs(90)))?,
            r#"{"kind":"gap","recv_t_us":90,"source_t_us":null}"#);
        assert_eq!(written(json!({ "kind": "still", "source_t_us": 40 }), |row| stamp_receipt(row, SessionTimeUs(90)))?,
            r#"{"kind":"still","recv_t_us":90,"source_t_us":40}"#);
        Ok(())
    }

    /// The blind row of the journal, the input Receiver and a pause interval.
    #[test]
    fn blind_bytes() -> serde_json::Result<()> {
        let producers = [Producer::RawInput, Producer::KeyboardHook];
        let row = BlindRow::new(InputBlindReason::ProducerStall, SessionTimeUs(10), SessionTimeUs(20), &producers);
        assert_eq!(serde_json::to_string(&row.value()?)?,
            r#"{"end_us":20,"kind":"blind","producers":["raw_input","keyboard_hook"],"reason":"producer_stall","start_us":10}"#);
        let row = BlindRow::new(InputBlindReason::HelperExited, SessionTimeUs(10), SessionTimeUs(10), &[]);
        assert_eq!(serde_json::to_string(&row.value()?)?,
            r#"{"end_us":10,"kind":"blind","reason":"helper_exited","start_us":10}"#);
        Ok(())
    }

    #[test]
    fn row_kinds() {
        assert_eq!(RowKind::parse("input_source_loss"), RowKind::InputSourceLoss);
        assert_eq!(RowKind::parse("recorder_control_gap"), RowKind::RecorderControlGap);
        assert_eq!(RowKind::parse("window_shown"), RowKind::Other);
        assert!(serde_json::to_value(RowKind::Other).is_err());
        assert_eq!(serde_json::to_value(RowKind::health(StreamId::Input)).ok(), Some(json!("input_health")));
        assert_eq!(serde_json::to_value(RowKind::health(StreamId::Context)).ok(), Some(json!("health")));
        assert!(RowKind::Pen.is_action() && !RowKind::Blind.is_action());
        assert!(RowKind::Blind.allowed_in_pause(StreamId::Input) && !RowKind::Blind.allowed_in_pause(StreamId::Context));
    }
}
