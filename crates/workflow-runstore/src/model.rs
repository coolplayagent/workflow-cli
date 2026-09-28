use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartRun {
    pub schema_version: u32,
    pub bundle: BundleSpec,
    pub run_id: String,
    pub inputs: Values,
    pub started_at_unix_ms: u64,
    #[serde(default)]
    pub limits: Limits,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Committed {
    pub snapshot: Snapshot,
    /// Empty commands on an exact duplicate. Pending delivery is queried via outbox.
    pub transition: Transition,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent {
    pub revision: u64,
    pub digest: String,
    pub event: Event,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryReceipt {
    pub run_id: String,
    pub command_id: String,
    pub command_digest: String,
    /// Stable host delivery identity, not a task success receipt or effect proof.
    pub delivery_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboxEntry {
    pub run_id: String,
    pub sequence: u64,
    pub revision: u64,
    pub command_index: u32,
    pub command_id: String,
    pub command_digest: String,
    pub command: Command,
    pub receipt: Option<DeliveryReceipt>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub revision: u64,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<workflow_kernel::Pause>,
    pub bundle_digest: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page<T, C> {
    pub items: Vec<T>,
    pub next_cursor: Option<C>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    pub run_id: String,
    pub revision: u64,
    pub checkpoint_revision: u64,
    pub events_checked: u64,
    pub commands_checked: u64,
    pub inbox_messages_checked: u64,
    pub state_digest: String,
}
