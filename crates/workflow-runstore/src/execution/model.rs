use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow_worker::{ExecutionGrant, WorkRequest, WorkResult};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub run_id: String,
    pub owner: String,
    pub acquisition_id: String,
    pub ttl_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub run_id: String,
    pub owner: String,
    pub acquisition_id: String,
    pub epoch: u64,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreparedTask {
    pub attempt_id: String,
    pub command_id: String,
    pub command_sequence: u64,
    pub epoch: u64,
    pub number: u32,
    pub prepared_revision: u64,
    pub request: WorkRequest,
    pub grant: ExecutionGrant,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Claimed {
    Task { attempt: Box<PreparedTask> },
    Handled { command_id: String },
    Idle,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionAction {
    Effect {
        record: Box<workflow_effects::EffectRecord>,
        transition: Option<EffectTransition>,
    },
    Acquired {
        lease: Lease,
    },
    Renewed {
        lease: Lease,
        at_unix_ms: u64,
    },
    Released {
        epoch: u64,
        at_unix_ms: u64,
    },
    Prepared {
        attempt: Box<PreparedTask>,
    },
    Finished {
        attempt_id: String,
        result: WorkResult,
        event_id: String,
        event_revision: u64,
        at_unix_ms: u64,
    },
    Failed {
        attempt_id: String,
        error: workflow_worker::Error,
        at_unix_ms: u64,
    },
    Handled {
        epoch: u64,
        command_id: String,
        event_id: Option<String>,
        event_revision: Option<u64>,
        at_unix_ms: u64,
    },
    GateChecked {
        epoch: u64,
        command_id: String,
        event_id: String,
        event_revision: u64,
        at_unix_ms: u64,
    },
    TimerAdvanced {
        epoch: u64,
        event_id: String,
        event_revision: u64,
        at_unix_ms: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    pub sequence: u64,
    pub action: ExecutionAction,
}
#[derive(Clone, Debug, PartialEq)]
pub enum AttemptOutcome {
    Finished {
        result: WorkResult,
        event_id: String,
        revision: u64,
    },
    Failed(workflow_worker::Error),
}
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptState {
    pub prepared: PreparedTask,
    pub outcome: Option<AttemptOutcome>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectTransition {
    pub event_id: String,
    pub event_revision: u64,
    pub command_id: String,
}
