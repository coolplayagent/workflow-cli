//! Deterministic workflow transitions. No clock reads, I/O, model calls or task execution.
mod bundle;
mod effects;
mod engine;
mod inbox;
mod model;
mod postconditions;
pub use bundle::*;
pub use engine::Engine;
pub use inbox::*;
pub use model::*;
pub use postconditions::*;
use serde::{Deserialize, Serialize};
pub use workflow_worker::Values;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidBundle,
    MissingReference,
    ContractMismatch,
    RecursiveBundle,
    UnsupportedPolicy,
    UnsafeCancellation,
    InvalidRequest,
    RevisionConflict,
    EventConflict,
    TerminalRun,
    RunPaused,
    InvalidTaskResult,
    UnknownInstance,
    InvalidSignal,
    InvalidGateResult,
    ClockReversal,
    BudgetExceeded,
    CorruptCheckpoint,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}
impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
impl From<workflow_worker::Error> for Error {
    fn from(e: workflow_worker::Error) -> Self {
        Self::new(ErrorCode::InvalidRequest, e.message)
    }
}
pub fn schema(kind: &str) -> Result<String> {
    let schema = match kind {
        "bundle" => schemars::schema_for!(BundleSpec),
        "event" => schemars::schema_for!(Event),
        "scenario" => schemars::schema_for!(Scenario),
        "checkpoint" => schemars::schema_for!(Checkpoint),
        "signal" => schemars::schema_for!(SignalMessage),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "kernel schema must be bundle, event, scenario, checkpoint or signal",
            ));
        }
    };
    serde_json::to_string_pretty(&schema)
        .map_err(|e| Error::new(ErrorCode::InvalidRequest, e.to_string()))
}

#[cfg(test)]
mod tests;

impl From<workflow_gates::Error> for Error {
    fn from(e: workflow_gates::Error) -> Self {
        Self::new(ErrorCode::InvalidGateResult, e.message)
    }
}
