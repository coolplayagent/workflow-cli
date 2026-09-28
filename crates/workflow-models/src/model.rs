use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow_ir::VersionRef;
use workflow_worker::{AdapterOutcome, CapabilityDescriptor, Values, WorkRequest, WorkResult};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    pub schema_version: u32,
    pub policy: VersionRef,
    pub task: CapabilityDescriptor,
    pub goal: String,
    pub tools: Vec<CapabilityDescriptor>,
    pub budget: Budget,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub model_calls: u32,
    pub tool_calls: u32,
    pub context_bytes: u32,
    pub response_bytes: u32,
    pub output_tokens_per_call: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub adapter: VersionRef,
    pub model: String,
    /// Host binding fingerprint excludes credential values.
    pub binding_digest: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub protocol_version: u32,
    pub action: Action,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Call {
        capability: VersionRef,
        inputs: Values,
        summary: String,
    },
    Complete {
        outputs: Values,
        summary: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelReply {
    pub proposal: Proposal,
    pub resolved_model: String,
    pub response_id: String,
    pub usage: Usage,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelFailure {
    Unavailable,
    InvalidResponse,
    Refused,
    Budget,
    Deadline,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Received { reply: ModelReply },
    Failed { failure: ModelFailure },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolReply {
    Received { result: WorkResult },
    Rejected { error: workflow_worker::ErrorCode },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelEvent {
    Replied {
        call_digest: String,
        at_unix_ms: u64,
        response: Reply,
    },
    ToolFinished {
        request: Box<WorkRequest>,
        at_unix_ms: u64,
        response: ToolReply,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRecord {
    pub schema_version: u32,
    pub request_digest: String,
    pub policy_digest: String,
    pub identity: ModelIdentity,
    pub deadline_unix_ms: u64,
    pub events: Vec<ModelEvent>,
    pub outcome: AdapterOutcome,
}
#[derive(Clone, Debug, Serialize)]
pub struct ModelCall {
    pub protocol_version: u32,
    pub request_digest: String,
    pub policy: PolicySpec,
    pub inputs: Values,
    pub events: Vec<ModelEvent>,
    pub remaining_model_calls: u32,
    pub remaining_tool_calls: u32,
    pub deadline_unix_ms: u64,
}
pub trait ModelAdapter: Send + Sync {
    fn identity(&self) -> ModelIdentity;
    /// Return an explicit JSON proposal, usage and visible identifiers; no hidden reasoning state.
    fn complete(&self, call: &ModelCall) -> Reply;
}
