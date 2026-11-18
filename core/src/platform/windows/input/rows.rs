//! The lifecycle rows of the input pump: pause, discontinuity and resume on
//! the context stream, the input blind rows of a pause, and the
//! `input_source_loss` rows of an elevated visit.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    package::{metadata::UncertifiedInputTail, outcome::HealthKind, row::{BlindRow, RowKind}},
    platform::windows::controller::PausedVideo,
    protocol::{AdmissionEpoch, MonitorId, QpcTick, SessionTimeUs, ipc::{ProducerGap, elevated::{VisitBinding, runtime::CoverageReason}}},
    session::{AdmissionBoundary, PauseReason, labels::pause_blind_labels},
};

use super::super::elevated::InputStagingLoss;

pub(super) fn paused_slot_ranges(ranges: &[PausedVideo], closed: SessionTimeUs, through: SessionTimeUs) -> Vec<PausedSlots> {
    ranges.iter().map(|range| PausedSlots {
        monitor: range.monitor, fps: range.fps, first_paused_slot: range.first_paused_slot,
        end_slot_exclusive: range.end_exclusive(closed, through),
    }).collect()
}

/// The video slots of one monitor that a pause interval covered.
#[derive(serde::Serialize)]
pub(super) struct PausedSlots {
    monitor: MonitorId,
    fps: u32,
    first_paused_slot: u64,
    end_slot_exclusive: Option<u64>,
}

/// The input blind rows of a pause interval from `start` to `end` (INP-13).
pub(super) fn blind_pause_rows(start: SessionTimeUs, end: SessionTimeUs, reasons: &[PauseReason])
    -> serde_json::Result<Vec<Value>>
{
    pause_blind_labels(reasons).into_iter().map(|reason| BlindRow::new(reason, start, end, &[]).value()).collect()
}

/// The pause reasons of a `pause` or `resume` row: `reason` is the primary
/// (first ordered) reason (CTX-6); `reasons` keeps the full ordered set as an extension.
#[derive(serde::Serialize)]
struct PauseReasons<'a> {
    reason: Option<&'a PauseReason>,
    reasons: &'a [PauseReason],
}

impl<'a> PauseReasons<'a> {
    fn new(reasons: &'a [PauseReason]) -> Self {
        Self { reason: reasons.first(), reasons }
    }
}

/// The context `pause` row at the input cutoff.
#[derive(serde::Serialize)]
struct PauseRow<'a> {
    kind: RowKind,
    #[serde(flatten)]
    reasons: PauseReasons<'a>,
    cutoff_t_us: SessionTimeUs,
    uncertified_input_tail: Option<UncertifiedInputTail>,
    paused_slots: &'a [PausedVideo],
    paused_slots_all_monitors: bool,
    /// The first QPC a required hook did not cover, when one stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    required_hook_first_uncovered_qpc: Option<QpcTick>,
}

pub(super) fn pause_row(
    reasons: &[PauseReason], cutoff: SessionTimeUs, tail: Option<UncertifiedInputTail>, ranges: &[PausedVideo],
    all_monitors: bool, hook_loss: Option<QpcTick>,
) -> serde_json::Result<Value> {
    serde_json::to_value(PauseRow {
        kind: RowKind::Pause, reasons: PauseReasons::new(reasons), cutoff_t_us: cutoff, uncertified_input_tail: tail,
        paused_slots: ranges, paused_slots_all_monitors: all_monitors, required_hook_first_uncovered_qpc: hook_loss,
    })
}

/// The context `discontinuity` row from the pause cutoff to the reopened boundary.
#[derive(serde::Serialize)]
struct DiscontinuityRow<'a> {
    kind: RowKind,
    start_t_us: SessionTimeUs,
    end_t_us: SessionTimeUs,
    gap_us: i64,
    previous_admission_epoch: AdmissionEpoch,
    reasons: &'a [PauseReason],
}

/// The `discontinuity` row from the pause cutoff `closed` to the reopened boundary at `opened`.
pub(super) fn discontinuity_row(closed: AdmissionBoundary, opened: SessionTimeUs, reasons: &[PauseReason])
    -> serde_json::Result<Value>
{
    serde_json::to_value(DiscontinuityRow {
        kind: RowKind::Discontinuity, start_t_us: closed.time, end_t_us: opened,
        gap_us: opened.0.saturating_sub(closed.time.0), previous_admission_epoch: closed.epoch, reasons,
    })
}

/// The context `resume` row with the totals suppressed during the pause.
#[derive(serde::Serialize)]
struct ResumeRow<'a> {
    kind: RowKind,
    #[serde(flatten)]
    reasons: PauseReasons<'a>,
    paused_start_t_us: SessionTimeUs,
    paused_slots: &'a [PausedSlots],
    suppressed_context_notifications: u64,
    suppressed_health: &'a BTreeMap<HealthKind, u64>,
}

pub(super) fn resume_row(
    reasons: &[PauseReason], paused_start: SessionTimeUs, paused_slots: &[PausedSlots],
    suppressed_notifications: u64, suppressed_health: &BTreeMap<HealthKind, u64>,
) -> serde_json::Result<Value> {
    serde_json::to_value(ResumeRow {
        kind: RowKind::Resume, reasons: PauseReasons::new(reasons), paused_start_t_us: paused_start, paused_slots,
        suppressed_context_notifications: suppressed_notifications, suppressed_health,
    })
}

/// The input source an `input_source_loss` row reports.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum LossSource {
    Elevated,
}

/// A Recorder staging overflow of elevated input, as an `input_source_loss` cause (E3).
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum StagingCause {
    RecorderStaging,
}

/// What an `input_source_loss` row reports beside its source and visit.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum SourceLoss<'a> {
    /// The collector's coverage loss, with the native producer gap that caused it.
    Coverage { cause: CoverageReason, native_gap: Option<ProducerGap> },
    /// The overflowed producer ranges and the undecodable records.
    Staging { cause: StagingCause, native_gaps: &'a [Option<ProducerGap>], malformed: u64 },
}

/// The context `input_source_loss` row of an elevated visit.
#[derive(serde::Serialize)]
struct InputSourceLossRow<'a> {
    kind: RowKind,
    source: LossSource,
    visit: VisitBinding,
    first_uncovered_qpc: QpcTick,
    #[serde(flatten)]
    loss: SourceLoss<'a>,
}

impl<'a> InputSourceLossRow<'a> {
    fn value(visit: VisitBinding, first_uncovered_qpc: QpcTick, loss: SourceLoss<'a>) -> serde_json::Result<Value> {
        serde_json::to_value(Self { kind: RowKind::InputSourceLoss, source: LossSource::Elevated, visit, first_uncovered_qpc, loss })
    }
}

/// The context row of an elevated visit's input coverage loss.
pub(super) fn source_loss_row(visit: VisitBinding, before: QpcTick, cause: CoverageReason, gap: Option<ProducerGap>)
    -> serde_json::Result<Value>
{
    InputSourceLossRow::value(visit, before, SourceLoss::Coverage { cause, native_gap: gap })
}

/// The context row of a Recorder staging overflow of elevated input (E3).
pub(super) fn staging_loss_row(loss: &InputStagingLoss) -> serde_json::Result<Value> {
    InputSourceLossRow::value(loss.visit, loss.before, SourceLoss::Staging {
        cause: StagingCause::RecorderStaging, native_gaps: &loss.gaps, malformed: loss.malformed,
    })
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;
    use crate::protocol::{
        FocusGeneration, SourceSequence, WindowId,
        ipc::{GapCause, ProcessInstance, Producer, elevated::WindowInstance},
    };

    const VIDEO: PausedVideo = PausedVideo { monitor: MonitorId(1), first_paused_slot: 12, fps: 30 };
    const VISIT: &str = r#"{"epoch":1,"focus_generation":0,"focus_revision":0,"foreground":{"process":{"creation_filetime":0,"pid":1},"thread":1,"window":1},"grant":1,"resync_revision":1,"source_generation":1,"witness_revision":0}"#;
    const GAP: &str = r#"{"cause":"overflow","first_qpc":400,"first_sequence":3,"last_qpc":450,"last_sequence":5,"producer":"raw_input"}"#;

    fn visit() -> VisitBinding {
        VisitBinding {
            epoch: AdmissionEpoch(1), source_generation: NonZeroU64::MIN, grant: NonZeroU64::MIN, witness_revision: 0,
            foreground: WindowInstance {
                window: WindowId(1), process: ProcessInstance { pid: NonZeroU32::MIN, creation_filetime: 0 }, thread: NonZeroU32::MIN,
            },
            focus_generation: FocusGeneration(0), focus_revision: 0, resync_revision: NonZeroU64::MIN,
        }
    }

    fn gap() -> ProducerGap {
        ProducerGap {
            producer: Producer::RawInput, cause: GapCause::Overflow, first_sequence: SourceSequence(3),
            last_sequence: SourceSequence(5), first_qpc: QpcTick(400), last_qpc: QpcTick(450),
        }
    }

    #[test]
    fn pause_bytes() -> serde_json::Result<()> {
        let tail = UncertifiedInputTail { first_t_us: SessionTimeUs(150), cutoff_t_us: SessionTimeUs(200), reason: None };
        let pause = pause_row(&[PauseReason::User, PauseReason::LowDisk], SessionTimeUs(200), Some(tail), &[VIDEO], true,
            Some(QpcTick(777)))?;
        assert_eq!(serde_json::to_string(&pause)?,
            r#"{"cutoff_t_us":200,"kind":"pause","paused_slots":[{"first_paused_slot":12,"fps":30,"monitor":1}],"paused_slots_all_monitors":true,"reason":"user","reasons":["user","low_disk"],"required_hook_first_uncovered_qpc":777,"uncertified_input_tail":{"cutoff_t_us":200,"first_t_us":150}}"#);
        assert_eq!(serde_json::to_string(&pause_row(&[], SessionTimeUs(200), None, &[], false, None)?)?,
            r#"{"cutoff_t_us":200,"kind":"pause","paused_slots":[],"paused_slots_all_monitors":false,"reason":null,"reasons":[],"uncertified_input_tail":null}"#);
        Ok(())
    }

    #[test]
    fn resume_bytes() -> serde_json::Result<()> {
        let closed = AdmissionBoundary { epoch: AdmissionEpoch(3), qpc: QpcTick(2_000), time: SessionTimeUs(200) };
        assert_eq!(serde_json::to_string(&discontinuity_row(closed, SessionTimeUs(900), &[PauseReason::Sleep])?)?,
            r#"{"end_t_us":900,"gap_us":700,"kind":"discontinuity","previous_admission_epoch":3,"reasons":["sleep"],"start_t_us":200}"#);
        let slots = paused_slot_ranges(&[VIDEO], closed.time, SessionTimeUs(900));
        let health = BTreeMap::from([(HealthKind::VideoFailure, 2)]);
        assert_eq!(serde_json::to_string(&resume_row(&[PauseReason::Sleep], closed.time, &slots, 4, &health)?)?,
            r#"{"kind":"resume","paused_slots":[{"end_slot_exclusive":13,"first_paused_slot":12,"fps":30,"monitor":1}],"paused_start_t_us":200,"reason":"sleep","reasons":["sleep"],"suppressed_context_notifications":4,"suppressed_health":{"video_failure":2}}"#);
        Ok(())
    }

    #[test]
    fn input_source_loss_bytes() -> serde_json::Result<()> {
        let row = source_loss_row(visit(), QpcTick(500), CoverageReason::NativeGap, Some(gap()))?;
        assert_eq!(serde_json::to_string(&row)?, format!(
            r#"{{"cause":"native_gap","first_uncovered_qpc":500,"kind":"input_source_loss","native_gap":{GAP},"source":"elevated","visit":{VISIT}}}"#));
        let row = source_loss_row(visit(), QpcTick(500), CoverageReason::Opening, None)?;
        assert_eq!(serde_json::to_string(&row)?, format!(
            r#"{{"cause":"opening","first_uncovered_qpc":500,"kind":"input_source_loss","native_gap":null,"source":"elevated","visit":{VISIT}}}"#));
        let loss = InputStagingLoss { visit: visit(), before: QpcTick(500), gaps: [None, Some(gap()), None, None, None], malformed: 2 };
        assert_eq!(serde_json::to_string(&staging_loss_row(&loss)?)?, format!(
            r#"{{"cause":"recorder_staging","first_uncovered_qpc":500,"kind":"input_source_loss","malformed":2,"native_gaps":[null,{GAP},null,null,null],"source":"elevated","visit":{VISIT}}}"#));
        Ok(())
    }
}
