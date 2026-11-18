mod launcher;
mod ingress;
mod peer;
mod visit;
mod selector;

use std::time::Duration;

use launcher::{LaunchStatus, Ticket};
pub(super) use ingress::{InputCertificate, InputStagingLoss};
pub(super) use selector::{ContextDelivery, Selector};
pub(super) use visit::add_time;

/// The bound on the collector's cleanup after a revoke, an environment change
/// or Stop. The collector bounds its own cleanup by the same 750 ms.
const CLEANUP: Duration = Duration::from_millis(750);
