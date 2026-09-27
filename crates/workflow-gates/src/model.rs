use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use workflow_artifacts::{ArtifactLink, ArtifactType, Producer, SourceRevision};
use workflow_ir::VersionRef;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub id: String,
    pub node_id: String,
    pub capability: VersionRef,
    pub contract_digest: String,
    pub report_type: ArtifactType,
    /// Required Boolean in the accepted worker output; artifact text is not a verdict.
    pub pass_field: String,
    pub max_age_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    pub identity: VersionRef,
    pub requirements: Vec<Requirement>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub run_id: String,
    pub run_digest: String,
    pub action: VersionRef,
    pub source_revision: SourceRevision,
    /// Digest of the common inputs actually checked by each required task.
    pub input_digest: String,
    pub artifacts: Vec<ArtifactLink>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub requirement_id: String,
    pub report: ArtifactLink,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub policy: Policy,
    pub target: Target,
    pub evidence: Vec<Evidence>,
}

/// Returned only by a trusted execution-history adapter, never deserialized from gate input.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutedCheck {
    pub producer: Producer,
    pub run_digest: String,
    pub node_id: String,
    pub capability: VersionRef,
    pub contract_digest: String,
    pub completed_at_unix_ms: u64,
    pub settled_at_unix_ms: u64,
    pub outcome: CheckOutcome,
    pub evidence: Vec<ArtifactLink>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum CheckOutcome {
    Succeeded {
        outputs: BTreeMap<String, serde_json::Value>,
    },
    Failed {
        code: String,
    },
    Inconclusive {
        code: String,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    Pass,
    Fail,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Passed,
    CheckFailed,
    CheckInconclusive,
    MissingEvidence,
    EvidenceUnavailable,
    TypeMismatch,
    TargetMismatch,
    UnrecordedEvidence,
    ProducerMismatch,
    ToolMismatch,
    InvalidTime,
    Expired,
    MissingBoolean,
    ArtifactUnavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub requirement_id: String,
    pub report: Option<ArtifactLink>,
    pub verdict: Verdict,
    pub reason: Reason,
    pub producer: Option<Producer>,
    pub completed_at_unix_ms: Option<u64>,
    pub expires_at_unix_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFinding {
    pub artifact: ArtifactLink,
    pub verdict: Verdict,
    pub reason: Reason,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub schema_version: u32,
    pub request_digest: String,
    pub policy_digest: String,
    pub target_digest: String,
    pub evaluated_at_unix_ms: u64,
    pub expires_at_unix_ms: Option<u64>,
    pub verdict: Verdict,
    pub checks: Vec<Finding>,
    pub artifacts: Vec<ArtifactFinding>,
}
