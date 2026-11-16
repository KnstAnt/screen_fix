//! Values exchanged by the capture, input, context and publication owners.

pub mod context;
pub mod frame;
pub mod input;
pub mod observation;
pub mod save;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Capacity,
    Deadline,
    AdmissionClosed,
    GeometryChanged,
    MissingHistory,
    ProviderUnavailable,
    PersistenceFailed,
}
