//! Platform-independent values shared by the recorder and input helper.

mod bounded;
mod evidence;
mod identity;
pub mod ipc;
mod time;

pub use bounded::{BoundedList, BoundedText, PayloadLimit};
pub use evidence::*;
pub use identity::*;
pub use time::*;
