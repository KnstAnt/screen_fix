//! The timing budgets of the collector runtime.

use std::time::Duration;

/// The heartbeat period, and the largest age of a healthy witness sample.
pub const HEARTBEAT: Duration = Duration::from_millis(250);
/// The validity of one lease renewal. It also bounds a grant's expiry, the
/// span of one read request and the silence of the control lane.
pub(super) const LEASE: Duration = Duration::from_secs(1);

/// The idle wait of the owner loop, the egress lanes, the read workers and
/// the witness startup.
pub(super) const POLL: Duration = Duration::from_millis(4);
/// A read deadline that is not meant to expire.
pub(super) const LONG_WAIT: Duration = Duration::from_secs(86_400);

/// Input received within this span after the read floor is not forwarded;
/// an accepted resync reports it as uncovered.
pub(super) const INPUT_HOLDBACK: Duration = Duration::from_millis(32);
/// The longest an essential producer's coverage, or one egress write, may
/// stall.
pub(super) const ESSENTIAL_PROGRESS: Duration = Duration::from_millis(500);
/// Budget of the provider-backed egress gate of one context record.
pub(super) const GATE: Duration = Duration::from_millis(100);

/// Budget of one UI Automation read.
pub(super) const UIA_BUDGET: Duration = Duration::from_millis(500);
/// Budget of one IME read.
pub(super) const IME_BUDGET: Duration = Duration::from_millis(50);
/// Budget of one native read, and of one commit sender check.
pub(super) const NATIVE_BUDGET: Duration = Duration::from_millis(250);

/// The drain time of already dispatched notifications before receipt
/// coverage advances.
pub(super) const NOTICE_HOLDBACK: Duration = Duration::from_millis(64);
/// The longest notice coverage may lag behind the witness. It has the value
/// of `ESSENTIAL_PROGRESS`, but is a separate budget.
pub(super) const NOTICE_STALL: Duration = Duration::from_millis(500);
/// The idle wait of the notice worker.
pub(super) const NOTICE_POLL: Duration = Duration::from_millis(10);

/// The longest interval between two witness samples.
pub(super) const WITNESS_SAMPLE: Duration = Duration::from_millis(10);
/// A coarse message tick older than this is not bounded from its anchor.
pub(super) const MAX_TICK_AGE_MS: u64 = 1_000;
