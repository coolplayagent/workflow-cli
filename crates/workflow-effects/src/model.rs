use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow_ir::VersionRef;
use workflow_worker::{CapabilityDescriptor, Values};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Includes queries and writes; persists across leases and process restarts.
    pub max_calls: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub total_write_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectPolicy {
    pub identity: VersionRef,
    /// Logical provider/resource boundary, resolved by the host without secrets.
    pub target: VersionRef,
    /// Service principal binding, not an authenticated human actor assertion.
    pub call_identity: VersionRef,
    pub retry: RetryPolicy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectBinding {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensates: Option<String>,
    pub workflow: VersionRef,
    pub node_id: String,
    pub policy: EffectPolicy,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectIntent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensates: Option<CompensationRef>,
    pub schema_version: u32,
    pub operation_key: String,
    pub run_id: String,
    pub run_digest: String,
    pub instance_id: u64,
    pub workflow: VersionRef,
    pub node_id: String,
    pub command_id: String,
    pub command_digest: String,
    pub capability: CapabilityDescriptor,
    pub inputs: Values,
    pub input_digest: String,
    pub policy: EffectPolicy,
    pub created_at_unix_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    Write,
    Query,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectAttempt {
    pub intent: EffectIntent,
    pub attempt_id: String,
    pub epoch: u64,
    pub number: u32,
    pub kind: CallKind,
    pub prepared_revision: u64,
    pub issued_at_unix_ms: u64,
    pub deadline_unix_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectReceipt {
    pub operation_key: String,
    pub intent_digest: String,
    pub target: VersionRef,
    pub resource_id: String,
    pub provider_receipt: String,
    pub outputs: Values,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    Applied {
        receipt: EffectReceipt,
    },
    /// Provider guarantees this invocation did not apply an effect. Timeouts,
    /// transport errors and ambiguous HTTP 5xx must use Unknown instead.
    NotApplied {
        code: String,
        class: workflow_worker::FailureClass,
        message: String,
    },
    /// A query found nothing *at that instant*. It does not fence an old writer.
    Absent,
    Unknown {
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectStatus {
    InFlight,
    Retry {
        kind: CallKind,
        not_before_unix_ms: u64,
    },
    Uncertain {
        reason: String,
    },
    /// A compensation invocation is known not to have applied, but its business
    /// obligation remains unresolved and needs an operator to complete it.
    NeedsAttention {
        code: String,
    },
    Applied {
        receipt: EffectReceipt,
    },
    Failed {
        code: String,
    },
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    pub attempt: EffectAttempt,
    pub request_digest: String,
    pub observation: Option<Observation>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensated_by: Option<String>,
    pub intent: EffectIntent,
    pub calls: Vec<AttemptRecord>,
    pub status: EffectStatus,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManualResolution {
    pub resolution_id: String,
    /// Trusted local administrator annotation. Remote roles must be checked by
    /// an authenticated service; this string alone grants no authority.
    pub actor: String,
    pub reason: String,
    pub evidence: String,
    pub outcome: ManualOutcome,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManualOutcome {
    Applied { receipt: EffectReceipt },
    ConfirmedNotApplied,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectChange {
    /// Trusted recovery imports an actual post-backup provider receipt. It does
    /// not dispatch a call or recreate the missing attempt history.
    Imported {
        intent: Box<EffectIntent>,
        resolution: ManualResolution,
    },
    Prepared {
        attempt: Box<EffectAttempt>,
        request_digest: String,
    },
    Observed {
        operation_key: String,
        attempt_id: String,
        observation: Observation,
    },
    Stopped {
        operation_key: String,
        reason: String,
    },
    Resolved {
        operation_key: String,
        resolution: ManualResolution,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectRecord {
    pub epoch: u64,
    pub at_unix_ms: u64,
    pub change: EffectChange,
}
impl EffectChange {
    pub fn key(&self) -> &str {
        match self {
            Self::Imported { intent, .. } => &intent.operation_key,
            Self::Prepared { attempt, .. } => &attempt.intent.operation_key,
            Self::Observed { operation_key, .. }
            | Self::Stopped { operation_key, .. }
            | Self::Resolved { operation_key, .. } => operation_key,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Call(CallKind),
    Wait(u64),
    InProgress,
    Manual(String),
    Done,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectReply {
    pub request_digest: String,
    pub observation: Observation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompensationRef {
    pub operation_key: String,
    pub receipt: EffectReceipt,
}
