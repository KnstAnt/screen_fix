//! Shared Windows native observation and input collection mechanics.
#![cfg(target_os = "windows")]

pub mod input;

#[cfg(feature = "context")]
pub mod events;
#[cfg(feature = "context")]
pub mod held;
#[cfg(feature = "context")]
pub mod hung;
#[cfg(feature = "context")]
pub mod identity;
#[cfg(feature = "context")]
pub mod ime;
#[cfg(feature = "context")]
pub mod uia;
#[cfg(feature = "context")]
pub mod uia_commits;
#[cfg(feature = "context")]
pub mod windows;
