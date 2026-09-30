//! An authenticated executor still owns the actual target comparison. A digest
//! binds the reviewed subject; it cannot attest an external system's state.
use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use workflow_artifacts::{ArtifactLink, SourceRevision};
use workflow_gates::{ApprovalEvidence, ApprovalRequirement, GateContext, GateEvaluation, Verdict};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetCheck {
    /// Gateway atomically compares the full subject and applies the operation.
    AtomicCompare,
    /// Gateway observes before writing; the declared query is required after
    /// ambiguous outcomes. This mode does not claim to close the external race.
    ObserveThenReconcile { remaining_race: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSubject {
    pub source_revision: SourceRevision,
    pub input_digest: String,
    pub artifacts: Vec<ArtifactLink>,
}
impl ReleaseSubject {
    pub fn from_target(target: &workflow_gates::Target) -> Self {
        Self {
            source_revision: target.source_revision.clone(),
            input_digest: target.input_digest.clone(),
            artifacts: target.artifacts.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseApproval {
    pub gate_node: String,
    pub approval: ApprovalRequirement,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleasePolicy {
    pub gate_nodes: Vec<String>,
    /// Required task input containing the exact ReleaseSubject object.
    pub subject_field: String,
    pub approvals: Vec<ReleaseApproval>,
    pub target_check: TargetCheck,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIntent {
    pub policy: ReleasePolicy,
    pub subject: ReleaseSubject,
    pub gates: Vec<GateContext>,
    pub approvals: Vec<ApprovalEvidence>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAuthorization {
    pub intent_digest: String,
    pub evaluated_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub gates: Vec<GateEvaluation>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReceipt {
    /// Actual subject observed by the gateway, never merely copied from input.
    pub subject: ReleaseSubject,
    pub target_check: TargetCheck,
    pub authorization_digest: String,
    pub observed_at_unix_ms: u64,
}
impl ReleasePolicy {
    pub fn validate(&self, has_query: bool) -> Result<()> {
        if self.gate_nodes.is_empty()
            || self.gate_nodes.len() > 16
            || self.gate_nodes.iter().collect::<BTreeSet<_>>().len() != self.gate_nodes.len()
            || self
                .gate_nodes
                .iter()
                .any(|n| !workflow_validator::identifier(n))
            || !workflow_validator::identifier(&self.subject_field)
            || self.approvals.len() > 16
            || self
                .approvals
                .iter()
                .any(|a| !self.gate_nodes.contains(&a.gate_node))
        {
            return Err(invalid(
                "release policy requires 1..16 unique gates, a subject and at most 16 scoped approvals",
            ));
        }
        if let TargetCheck::ObserveThenReconcile { remaining_race } = &self.target_check
            && (!has_query
                || remaining_race.trim().is_empty()
                || remaining_race.len() > 1024
                || remaining_race.contains('\0'))
        {
            return Err(invalid(
                "non-atomic release requires an explicit remaining race and a provider query",
            ));
        }
        Ok(())
    }
}
impl ReleaseIntent {
    pub fn validate(&self, intent: &EffectIntent) -> Result<()> {
        self.policy.validate(intent.has_query())?;
        if self.gates.len() != self.policy.gate_nodes.len()
            || self.approvals.len() != self.policy.approvals.len()
            || intent.inputs.get(&self.policy.subject_field)
                != Some(
                    &serde_json::to_value(&self.subject)
                        .map_err(|_| invalid("invalid release subject"))?,
                )
        {
            return Err(invalid(
                "release subject/gates differ from the immutable action inputs",
            ));
        }
        for c in &self.gates {
            workflow_gates::validate(&workflow_gates::Request {
                policy: c.policy.clone(),
                target: c.target.clone(),
                evidence: vec![],
            })
            .map_err(|e| invalid(&e.message))?;
            if c.target.action != intent.capability.capability
                || c.target.run_id != intent.run_id
                || c.target.run_digest != intent.run_digest
                || ReleaseSubject::from_target(&c.target) != self.subject
                || c.expected_instances.len() != c.policy.requirements.len()
                || c.policy
                    .requirements
                    .iter()
                    .any(|q| !c.expected_instances.contains_key(&q.id))
            {
                return Err(invalid(
                    "release gate binds another action, run, subject or instance set",
                ));
            }
        }
        for (required, proof) in self.policy.approvals.iter().zip(&self.approvals) {
            let index = self
                .policy
                .gate_nodes
                .iter()
                .position(|n| n == &required.gate_node)
                .unwrap();
            let c = &self.gates[index];
            if proof.node_id != required.approval.node_id
                || proof.exception.is_some()
                || proof.review_digest
                    != workflow_gates::review_digest(&c.policy, &c.target)
                        .map_err(|e| invalid(&e.message))?
            {
                return Err(invalid(
                    "release approval differs from the reviewed action/policy/subject",
                ));
            }
        }
        Ok(())
    }
}
impl ReleaseAuthorization {
    /// Structural check; the store independently recomputes every evaluation
    /// from authenticated settled tasks and verified artifact bytes.
    pub fn validate(&self, intent: &ReleaseIntent, now: u64) -> Result<()> {
        if self.intent_digest != digest(intent)?
            || self.evaluated_at_unix_ms != now
            || now >= self.expires_at_unix_ms
            || self.gates.len() != intent.gates.len()
        {
            return Err(invalid(
                "missing, expired or mismatched release authorization",
            ));
        }
        let mut deadline = u64::MAX;
        for (context, evaluation) in intent.gates.iter().zip(&self.gates) {
            workflow_gates::validate_evaluation(context, evaluation, now)
                .map_err(|e| invalid(&e.message))?;
            let d = &evaluation.decision;
            if evaluation.request.policy != context.policy
                || evaluation.request.target != context.target
                || d.evaluated_at_unix_ms != now
                || d.request_digest
                    != workflow_gates::digest(&evaluation.request)
                        .map_err(|e| invalid(&e.message))?
                || d.policy_digest != digest(&context.policy)?
                || d.target_digest != digest(&context.target)?
            {
                return Err(invalid(
                    "release decision differs from its frozen subject/policy/time",
                ));
            }
            if d.verdict == Verdict::Pass {
                if evaluation.exception.is_some() {
                    return Err(invalid("PASS cannot masquerade as an exception"));
                }
            } else if evaluation.exception.is_none()
                || evaluation.exception != context.exception
                || evaluation
                    .exception
                    .as_ref()
                    .is_some_and(|a| a.exception.is_none() || a.applied_at_unix_ms > now)
            {
                return Err(invalid(
                    "release requires PASS or an independently authorized scoped exception",
                ));
            }
            deadline = deadline.min(
                evaluation
                    .admission_deadline()
                    .ok_or_else(|| invalid("release gate has no current admission window"))?,
            );
        }
        for approval in &intent.approvals {
            if approval.applied_at_unix_ms > now {
                return Err(invalid("approval is from the future"));
            }
            deadline = deadline.min(approval.expires_at_unix_ms);
        }
        if deadline != self.expires_at_unix_ms {
            return Err(invalid(
                "release deadline differs from evidence and approval expiry",
            ));
        }
        Ok(())
    }
}
impl ReleaseReceipt {
    pub fn validate(&self, intent: &ReleaseIntent, created_at: u64) -> Result<()> {
        if self.subject != intent.subject
            || self.target_check != intent.policy.target_check
            || self.observed_at_unix_ms < created_at
            || !self
                .authorization_digest
                .strip_prefix("sha256:")
                .is_some_and(|s| {
                    s.len() == 64
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
        {
            return Err(invalid(
                "provider receipt does not attest this delivery target and comparison contract",
            ));
        }
        Ok(())
    }
}

pub fn release_subject_type() -> workflow_ir::ValueType {
    use std::collections::BTreeMap;
    use workflow_ir::ValueType as T;
    T::Object {
        fields: BTreeMap::from([
            (
                "source_revision".into(),
                T::Object {
                    fields: BTreeMap::from([
                        ("repository".into(), T::String),
                        ("revision".into(), T::String),
                    ]),
                },
            ),
            ("input_digest".into(), T::String),
            (
                "artifacts".into(),
                T::Array {
                    items: Box::new(T::Object {
                        fields: BTreeMap::from([
                            ("artifact_id".into(), T::String),
                            ("digest".into(), T::String),
                        ]),
                    }),
                },
            ),
        ]),
    }
}
