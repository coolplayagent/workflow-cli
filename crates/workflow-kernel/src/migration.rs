//! Reviewed definition migration contracts, independent of storage schema upgrades.
//! A migration restarts the target root with fresh instances and keeps the full
//! versioned event history. It never reuses an old result, gate or approval.
use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow_ir::VersionRef;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationNode {
    pub workflow: VersionRef,
    pub node_id: String,
}
impl Ord for MigrationNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.workflow.id, &self.workflow.version, &self.node_id).cmp(&(
            &other.workflow.id,
            &other.workflow.version,
            &other.node_id,
        ))
    }
}
impl PartialOrd for MigrationNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeMapping {
    pub source: MigrationNode,
    pub target: Option<MigrationNode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrationExecutionPolicy {
    /// Recompute all results, gates and approvals from explicit target inputs.
    /// Any existing effect ledger blocks this policy. Finish the old run or
    /// explicitly reconcile a separate new execution instead.
    RestartWithFreshEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrationTimerPolicy {
    /// Cancel old timers. Target nodes start their declared durations when the
    /// paused migrated run is explicitly resumed and reaches those nodes.
    CancelAndRearmOnResume,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationRequest {
    pub migration_id: String,
    pub target_bundle: BundleSpec,
    pub target_inputs: Values,
    pub execution_policy: MigrationExecutionPolicy,
    pub timer_policy: MigrationTimerPolicy,
    /// Explicit overrides of the suggested identity mapping. Unspecified nodes
    /// map to the same node ID in the selected target workflow, or to removal.
    #[serde(default)]
    pub node_mapping: Vec<NodeMapping>,
    pub decision_summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationNodeImpact {
    pub source: MigrationNode,
    pub target: Option<MigrationNode>,
    pub definition_changed: bool,
    pub gate_changed: bool,
    pub approval_policy_changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InvalidatedInstance {
    pub frame_id: u64,
    pub node: MigrationNode,
    pub instance_id: u64,
    pub state: String,
    pub input_digest: String,
    pub output_digest: String,
    pub gate_decision_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationTimer {
    pub instance_id: u64,
    pub source: MigrationNode,
    pub old_deadline_unix_ms: u64,
    pub target: Option<MigrationNode>,
    pub target_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationPlan {
    pub schema_version: u32,
    pub run_id: String,
    pub run_digest: String,
    pub source_revision: u64,
    pub source_state_digest: String,
    pub source_bundle_digest: String,
    pub target_bundle_digest: String,
    pub inputs_changed: bool,
    pub request: MigrationRequest,
    pub nodes: Vec<MigrationNodeImpact>,
    pub added_nodes: Vec<MigrationNode>,
    pub invalidated_instances: Vec<InvalidatedInstance>,
    /// Old callback IDs remain historical records. None authorize target nodes.
    pub invalidated_messages: Vec<String>,
    pub timers: Vec<MigrationTimer>,
}
