//! Durable run storage port. Adapters commit state, events and command intents atomically.
mod effects;
pub use effects::*;
mod execution;
mod inbox;
mod migration;
mod model;
mod restoration;
pub use execution::*;
pub use inbox::*;
pub use migration::*;
pub use model::*;
pub use restoration::*;
use serde::{Deserialize, Serialize};
pub use workflow_kernel::{
    BundleSpec, Checkpoint, Command, Event, Limits, RunStatus, Snapshot, Transition, Values,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    InvalidRequest,
    ArtifactUnavailable,
    ArtifactRejected,
    LeaseBusy,
    LeaseConflict,
    AttemptInProgress,
    AttemptBudget,
    ManualReconciliation,
    MigrationBlocked,
    RecoveryRequired,
    UnsupportedEffect,
    NotFound,
    StartConflict,
    BindingConflict,
    ReceiptConflict,
    SignalConflict,
    DeliveryOrder,
    TransitionRejected,
    Busy,
    Storage,
    UnsupportedStorage,
    CorruptStorage,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub kernel_code: Option<workflow_kernel::ErrorCode>,
}
impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            kernel_code: None,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<workflow_kernel::Error> for Error {
    fn from(e: workflow_kernel::Error) -> Self {
        Self {
            code: ErrorCode::TransitionRejected,
            message: e.message,
            kernel_code: Some(e.code),
        }
    }
}
impl From<workflow_worker::Error> for Error {
    fn from(e: workflow_worker::Error) -> Self {
        Self::new(ErrorCode::InvalidRequest, e.message)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// A successful mutation must follow the adapter's durable commit. Read methods
/// observe one consistent transaction. Events/receipts are trusted host inputs.
/// Reading pending commands is not a lease or permission to execute them.
pub trait RunStore {
    fn start(&mut self, request: &StartRun) -> Result<Committed>;
    fn apply(&mut self, event: &Event) -> Result<Committed>;
    fn get(&mut self, run_id: &str) -> Result<Snapshot>;
    /// Frozen executable contracts for composing a worker; never replaces the stored binding.
    fn bundle(&mut self, run_id: &str) -> Result<BundleSpec>;
    fn list(&mut self, after: Option<&str>, limit: u32) -> Result<Page<RunSummary, String>>;
    fn history(
        &mut self,
        run_id: &str,
        after_revision: u64,
        limit: u32,
    ) -> Result<Page<RecordedEvent, u64>>;
    fn outbox(
        &mut self,
        run_id: &str,
        after_sequence: u64,
        limit: u32,
        pending_only: bool,
    ) -> Result<Page<OutboxEntry, u64>>;
    fn acknowledge(&mut self, receipt: &DeliveryReceipt) -> Result<OutboxEntry>;
    fn verify(&mut self, run_id: &str) -> Result<Verification>;
}
pub fn validate_id(id: &str) -> Result<()> {
    if !workflow_validator::identifier(id) {
        return Err(Error::new(ErrorCode::InvalidRequest, "stable ID required"));
    }
    Ok(())
}
pub fn validate_limit(limit: u32) -> Result<()> {
    if !(1..=100).contains(&limit) {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "page limit must be 1..100",
        ));
    }
    Ok(())
}
pub fn command_entry(
    run_digest: &str,
    run_id: &str,
    sequence: u64,
    revision: u64,
    index: u32,
    command: Command,
) -> Result<OutboxEntry> {
    Ok(OutboxEntry {
        run_id: run_id.into(),
        sequence,
        revision,
        command_index: index,
        command_id: workflow_worker::digest(&(run_digest, revision, index))?,
        command_digest: workflow_worker::digest(&command)?,
        command,
        receipt: None,
    })
}
pub fn schema(kind: &str) -> Result<String> {
    let schema = match kind {
        "lease" => schemars::schema_for!(LeaseRequest),
        "execution-record" => schemars::schema_for!(ExecutionRecord),
        "start" => schemars::schema_for!(StartRun),
        "receipt" => schemars::schema_for!(DeliveryReceipt),
        "signal" => schemars::schema_for!(SignalSubmission),
        "effect-attempt" => schemars::schema_for!(workflow_effects::EffectAttempt),
        "effect-reply" => schemars::schema_for!(workflow_effects::EffectReply),
        "effect-observation" => schemars::schema_for!(workflow_effects::Observation),
        "recovery-acknowledgement" => schemars::schema_for!(RecoveryAcknowledgement),
        "restored-effect" => schemars::schema_for!(RestoredEffect),
        "migration-request" => schemars::schema_for!(workflow_kernel::MigrationRequest),
        "migration-plan" => schemars::schema_for!(workflow_kernel::MigrationPlan),
        "storage-upgrade" => schemars::schema_for!(StorageUpgrade),
        "effect-resolution" => schemars::schema_for!(workflow_effects::ManualResolution),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "run schema must be start, receipt, lease, execution-record or signal",
            ));
        }
    };
    serde_json::to_string_pretty(&schema)
        .map_err(|e| Error::new(ErrorCode::InvalidRequest, e.to_string()))
}

impl From<workflow_artifacts::Error> for Error {
    fn from(e: workflow_artifacts::Error) -> Self {
        Self::new(
            if e.code == workflow_artifacts::ErrorCode::NotFound {
                ErrorCode::ArtifactUnavailable
            } else {
                ErrorCode::ArtifactRejected
            },
            e.message,
        )
    }
}
