use crate::{BundleSpec, Values};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use workflow_ir::VersionRef;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_frames: u32,
    pub max_instances: u32,
    pub max_transitions: u64,
    pub max_events: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frames: 256,
            max_instances: 16384,
            max_transitions: 100000,
            max_events: 4096,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameStatus {
    Active,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}
impl FrameStatus {
    pub fn terminal(&self) -> bool {
        !matches!(self, Self::Active | Self::Cancelling)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenStatus {
    Pending,
    Selected,
    Skipped,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeToken {
    pub status: TokenStatus,
    pub sequence: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeState {
    Pending,
    TaskReady,
    CancelRequested,
    Reconciling,
    Waiting {
        deadline_unix_ms: u64,
    },
    Child {
        frame_id: u64,
        iteration: u32,
        deadline_unix_ms: Option<u64>,
        exhausting: bool,
    },
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}
impl NodeState {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Skipped
        )
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInstance {
    pub instance_id: u64,
    pub state: NodeState,
    pub inputs: Values,
    pub outputs: Values,
    pub cancel_requested: bool,
    pub winner_edge: Option<String>,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub workflow: VersionRef,
    pub definition_digest: String,
    pub inputs: Values,
    pub status: FrameStatus,
    pub nodes: BTreeMap<String, NodeInstance>,
    pub edges: BTreeMap<String, EdgeToken>,
    pub outputs: Values,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u32,
    pub run_id: String,
    pub run_digest: String,
    pub bundle_digest: String,
    pub revision: u64,
    pub now_unix_ms: u64,
    pub status: RunStatus,
    pub frames: BTreeMap<u64, Frame>,
    pub transition_count: u64,
    pub next_frame_id: u64,
    pub next_instance_id: u64,
    pub next_token_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskResult {
    Succeeded { outputs: Values },
    Failed { code: String },
    Cancelled,
    Uncertain { reason: String },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventKind {
    TaskCompleted {
        instance_id: u64,
        result: TaskResult,
    },
    TaskReconciled {
        instance_id: u64,
        result: TaskResult,
    },
    Signal {
        instance_id: u64,
        event: String,
        accepted: bool,
        outputs: Values,
    },
    AdvanceTime,
    Cancel,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub event_id: String,
    pub run_id: String,
    pub run_digest: String,
    pub expected_revision: u64,
    pub at_unix_ms: u64,
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    ExecuteTask {
        instance_id: u64,
        frame_id: u64,
        node_id: String,
        definition_digest: String,
        capability: VersionRef,
        contract_digest: String,
        inputs: Values,
    },
    AwaitSignal {
        instance_id: u64,
        event: String,
        deadline_unix_ms: u64,
    },
    ScheduleLoopDeadline {
        instance_id: u64,
        deadline_unix_ms: u64,
    },
    CancelTimer {
        instance_id: u64,
    },
    CancelTask {
        instance_id: u64,
    },
    ReconcileTask {
        instance_id: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    pub revision: u64,
    pub duplicate: bool,
    pub commands: Vec<Command>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub bundle_digest: String,
    pub run_id: String,
    pub inputs: Values,
    pub started_at_unix_ms: u64,
    pub limits: Limits,
    pub events: Vec<Event>,
    pub state_digest: String,
    pub checksum: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub bundle: BundleSpec,
    pub run_id: String,
    pub inputs: Values,
    pub started_at_unix_ms: u64,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub events: Vec<Event>,
}
