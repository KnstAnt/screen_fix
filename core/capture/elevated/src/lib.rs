#[cfg(not(target_os = "windows"))]
compile_error!("Screen Fix supports only Windows.");

mod collector;
mod deadline;
mod pipe;
mod process;
mod recorder;
mod runtime;
mod secret;
mod trust;
mod win;
mod wire;

pub use collector::run_collector;
pub use process::{ForegroundAccess, foreground_access};
pub use recorder::{Authenticated, LaunchRequest, RecorderBootstrap};
pub use runtime::{HEARTBEAT, RecorderLanes, Receiver, Sender, normal_capture_desktop};
pub use deadline::{Cancellation, READINESS};
pub use win::{Failure, Result, restrict_dll_search};
