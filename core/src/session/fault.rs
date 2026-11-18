//! Typed fault reports and the controller-owned fault log (§6.2).
//!
//! Owners report attempt failures (F) and activity losses (L) here; none of
//! them decides the attempt outcome itself. Recorded losses (G) never reach the
//! controller: their owners report them to the stream ingress as ledger
//! entries, unavailable rows or health rows.

use crate::{
    CaptureError,
    error::CaptureCause,
    package::{
        diagnostics::Failure,
        metadata::{ActivityLoss, LostActivity},
    },
    protocol::{MonitorId, SessionTimeUs},
    session::DegradedActivity,
    util::push_bounded,
};

/// Attempt failures beyond this bound are counted, not retained. Activity
/// losses are bounded by their owners (one per monitor video, the input
/// failure limit and the auxiliary activities) and are always retained.
const MAX_ATTEMPT_REPORTS: usize = 32;
/// Distinct omitted attempt causes remembered so that a cause reported again
/// is counted once. Beyond this bound a repeated omitted cause is counted again.
const MAX_OMITTED_CAUSES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultClass {
    /// F: the integrity of the attempt cannot be maintained. The Stop path
    /// runs with the first cause retained.
    Attempt,
    /// L: one activity stops for the rest of the attempt; the attempt continues.
    Activity(LostActivity),
}

#[derive(Debug, Clone)]
pub(crate) struct FaultReport {
    class: FaultClass,
    cause: CaptureCause,
    monitor: Option<MonitorId>,
    first_affected: Option<SessionTimeUs>,
    detected: Option<SessionTimeUs>,
}

impl FaultReport {
    /// An attempt failure keeps the reporting owner's error, including its component.
    pub(crate) fn attempt(error: &CaptureError) -> Self {
        Self {
            class: FaultClass::Attempt, cause: error.cause().clone(), monitor: None, first_affected: None, detected: None,
        }
    }

    pub(crate) fn activity(
        activity: LostActivity, failure: Failure, monitor: Option<MonitorId>, first_affected: Option<SessionTimeUs>,
    ) -> Self {
        let subject = match monitor {
            Some(monitor) => format!("{} on monitor {}", failure.component().label(), monitor.0),
            None => failure.component().label().to_owned(),
        };
        let message = format!("Recording degraded: {subject} stopped. {}.", failure.operation()).into();
        Self {
            class: FaultClass::Activity(activity),
            cause: CaptureCause { failure, message, remedy: None },
            monitor, first_affected, detected: None,
        }
    }

    pub(crate) fn class(&self) -> FaultClass {
        self.class
    }

    pub(crate) fn operation(&self) -> &'static str {
        self.cause.failure.operation()
    }

    fn failure(&self) -> Failure {
        self.cause.failure
    }

    pub(crate) fn error(&self) -> CaptureError {
        CaptureError::from(self.cause.clone())
    }
}

/// One selected cause of the fault log with the other retained causes.
pub(crate) struct FaultOutcome {
    /// The selected cause as an error.
    pub error: Option<CaptureError>,
    /// Every other retained cause, in report order.
    pub additional: Vec<Failure>,
    pub omitted: u64,
    pub activity_losses: Vec<ActivityLoss>,
}

#[derive(Default)]
pub(crate) struct FaultLog {
    reports: Vec<FaultReport>,
    attempt_reports: usize,
    omitted: u64,
    omitted_causes: Vec<Failure>,
}

impl FaultLog {
    /// Record a report in arrival order. A newly lost activity is returned for the status.
    pub(crate) fn record(&mut self, mut report: FaultReport, now: Option<SessionTimeUs>) -> Option<DegradedActivity> {
        report.detected = report.detected.or(now);
        crate::devlog::devlog!("fault", "{:?} component={} operation={:?} native_code={:?} monitor={:?} detected_us={:?}",
            report.class, report.failure().component().label(), report.failure().operation(),
            report.failure().native_code().map(|code| format!("{:#010x}", code as u32)),
            report.monitor.map(|monitor| monitor.0), report.detected.map(|time| time.0));
        match report.class {
            FaultClass::Attempt => {
                // An owner's failure is reported when detected and again when its
                // result is collected at finalization; the first report stands.
                if self.reports.iter().any(|old| old.class == FaultClass::Attempt && old.failure().same_cause(&report.failure())) {
                    return None;
                }
                if self.attempt_reports == MAX_ATTEMPT_REPORTS {
                    self.omit(report.failure());
                    return None;
                }
                self.attempt_reports += 1;
                self.reports.push(report);
                None
            }
            FaultClass::Activity(activity) => {
                let degraded = DegradedActivity {
                    component: report.failure().component(), monitor: report.monitor, required: activity == LostActivity::Required,
                };
                self.reports.push(report);
                Some(degraded)
            }
        }
    }

    /// Count an attempt cause beyond the retained bound once, like a retained one.
    fn omit(&mut self, failure: Failure) {
        if self.omitted_causes.iter().any(|old| old.same_cause(&failure)) {
            return;
        }
        push_bounded(&mut self.omitted_causes, failure, MAX_OMITTED_CAUSES);
        self.omitted = self.omitted.saturating_add(1);
    }

    pub(crate) fn attempt_failure(&self) -> Option<&FaultReport> {
        self.reports.iter().find(|report| report.class == FaultClass::Attempt)
    }

    /// The attempt result at finalization: the first attempt failure, if any.
    pub(crate) fn outcome(&self) -> FaultOutcome {
        self.select(self.reports.iter().position(|report| report.class == FaultClass::Attempt))
    }

    /// The result when the attempt could not finish: the primary cause.
    pub(crate) fn primary_outcome(&self) -> FaultOutcome {
        self.select((!self.reports.is_empty()).then_some(0))
    }

    fn select(&self, selected: Option<usize>) -> FaultOutcome {
        let additional = self.reports.iter().enumerate()
            .filter(|(index, _)| Some(*index) != selected)
            .map(|(_, report)| report.failure()).collect();
        let activity_losses = self.reports.iter().filter_map(|report| match report.class {
            FaultClass::Activity(activity) => Some(ActivityLoss {
                component: report.failure().component(), activity, monitor: report.monitor,
                operation: report.failure().operation().to_owned(), native_code: report.failure().native_code(),
                first_t_us: report.first_affected.or(report.detected),
            }),
            FaultClass::Attempt => None,
        }).collect();
        FaultOutcome {
            error: selected.map(|index| self.reports[index].error()),
            additional, omitted: self.omitted, activity_losses,
        }
    }
}
