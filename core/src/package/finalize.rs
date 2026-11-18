//! The final rows that the input and context journal owners write when an
//! attempt closes (§6.1, INP-13).

use crate::protocol::{MonitorId, SessionTimeUs};

use super::{
    diagnostics::Failure,
    journal::{FinalRow, HealthRecord},
    metadata::{InputBlind, InputBlindReason},
    outcome::HealthKind,
};

/// An input activity loss at close: its health row joins the final context
/// rows and its blind interval the final input rows (INP-13).
pub(crate) struct InputLoss {
    pub(crate) failure: Failure,
    /// The detection receipt.
    pub(crate) at: SessionTimeUs,
    /// The earliest missing receipt.
    pub(crate) first_missing: SessionTimeUs,
    /// The end of the blind interval: the input cutoff, else the session time
    /// at close, else the detection receipt.
    pub(crate) end_t_us: SessionTimeUs,
    pub(crate) reason: InputBlindReason,
}

/// A video activity loss, with its receipt once the session clock started.
pub(crate) struct VideoLoss {
    pub(crate) failure: Failure,
    pub(crate) monitor: MonitorId,
    pub(crate) at: Option<SessionTimeUs>,
}

/// The attempt facts that decide the final context rows.
pub(crate) struct ContextReport<'a> {
    pub(crate) video: Vec<VideoLoss>,
    /// Monitors whose video failure row the input loop already wrote.
    pub(crate) written_video: Option<&'a [MonitorId]>,
    /// Capture health reported after the input loop's last hand-off.
    pub(crate) late_health: Vec<HealthRecord>,
    /// The primary attempt failure and the session time at close.
    pub(crate) attempt_failure: Option<(Failure, SessionTimeUs)>,
}

/// The final rows of the input activity losses.
pub(crate) struct FinalRows {
    blinds: Vec<InputBlind>,
    input_health: Vec<HealthRecord>,
}

impl FinalRows {
    pub(crate) fn collect(losses: impl IntoIterator<Item = InputLoss>) -> Self {
        let mut rows = Self { blinds: Vec::new(), input_health: Vec::new() };
        for loss in losses {
            let blind = InputBlind { start_t_us: loss.first_missing.min(loss.end_t_us), end_t_us: loss.end_t_us, reason: loss.reason };
            rows.input_health.push(HealthRecord {
                kind: HealthKind::InputFailure, failure: loss.failure, detected: loss.at,
                first_affected: Some(blind.start_t_us), ongoing: true, monitor: None, count: None,
            });
            rows.blinds.push(blind);
        }
        rows
    }

    /// The blind interval of the input capture evidence: the first one.
    pub(crate) fn first_blind(&self) -> Option<InputBlind> {
        self.blinds.first().copied()
    }

    /// The final input rows, once.
    pub(crate) fn take_input(&mut self) -> Vec<FinalRow> {
        std::mem::take(&mut self.blinds).into_iter().map(FinalRow::Blind).collect()
    }

    /// The final context rows, in order: the video failures not yet written,
    /// the late capture health, the attempt failure and the input failures.
    /// A failure detected before the session clock started has no receipt on
    /// the common timeline; it is reported only through diagnostics.
    pub(crate) fn context(self, report: ContextReport<'_>) -> Vec<FinalRow> {
        let mut health: Vec<_> = report.video.into_iter()
            .filter(|entry| report.written_video.is_none_or(|written| !written.contains(&entry.monitor)))
            .filter_map(|entry| entry.at.map(|at| FinalRow::Health(video_failure_health(entry.failure, entry.monitor, at))))
            .collect();
        health.extend(report.late_health.into_iter().map(FinalRow::Health));
        if let Some((failure, at)) = report.attempt_failure {
            health.push(FinalRow::Health(HealthRecord {
                kind: HealthKind::AttemptFailure, failure, detected: at, first_affected: None,
                ongoing: false, monitor: None, count: None,
            }));
        }
        health.extend(self.input_health.into_iter().map(FinalRow::Health));
        health
    }
}

/// The health reports omitted from the context journal: those the
/// controller dropped, plus the late health when no context journal is open.
pub(crate) fn omitted_health(omitted: u64, late_health: usize, context_open: bool) -> u64 {
    if context_open { omitted } else { omitted.saturating_add(late_health as u64) }
}

/// One monitor's video can no longer be produced for the rest of the attempt.
pub(crate) fn video_failure_health(failure: Failure, monitor: MonitorId, at: SessionTimeUs) -> HealthRecord {
    HealthRecord {
        kind: HealthKind::VideoFailure, failure, detected: at, first_affected: Some(at), ongoing: true,
        monitor: Some(monitor), count: None,
    }
}
