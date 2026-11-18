//! The source, pause and admission vocabulary that the manifest serializes.
//! The session controller produces these values and re-exports them.

use crate::protocol::{AdmissionEpoch, QpcTick, SessionTimeUs, SourceFailureReason, TokenIdentityFailure, ipc::HelperFailure};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    User,
    LockScreen,
    Sleep,
    SecureDesktop,
    LowDisk,
    ElevationTransition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdmissionBoundary {
    pub epoch: AdmissionEpoch,
    pub qpc: QpcTick,
    pub time: SessionTimeUs,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct DiskPreflight {
    pub available_bytes: u64,
    pub minimum_bytes: u64,
    /// M1 has no task duration from which to calculate this reserve.
    pub duration_reserve_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStage {
    Foreground,
    Consent,
    Readiness,
    Activation,
    Capture,
    Cleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceFailure {
    pub stage: SourceStage,
    pub reason: SourceFailureReason,
    pub admitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<HelperFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_identity: Option<TokenIdentityFailure>,
}

impl SourceFailure {
    pub(crate) fn without_report_detail(mut self) -> Self {
        self.token_identity = None;
        if let Some(native) = &mut self.native {
            native.raw_input_registration = None;
        }
        self
    }
}
