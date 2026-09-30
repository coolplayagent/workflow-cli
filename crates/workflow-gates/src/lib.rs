//! Deterministic evidence checks, independent of storage, transport and the workflow engine.
//! A PASS is an observation of the supplied target, not an execution grant or approval.
mod evaluation;
mod observation;
pub use observation::validate_evaluation;
mod model;
mod validation;
pub use evaluation::{evaluate, revalidate};
pub use model::*;
pub use validation::{validate, validate_policy};
use workflow_artifacts::{ArtifactReader, Producer};
pub use workflow_artifacts::{Error, ErrorCode, Result, digest, parse_message, to_message};

/// Human review binds the policy and complete delivery subject. Run/instance
/// binding is supplied separately by the authenticated wait correlation, avoiding
/// a self-referential start digest when this value is an initial workflow input.
pub fn review_digest(policy: &Policy, target: &Target) -> Result<String> {
    validate(&Request {
        policy: policy.clone(),
        target: target.clone(),
        evidence: vec![],
    })?;
    let mut artifacts = target.artifacts.clone();
    artifacts.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
    digest(&(
        "workflow-review-v1",
        policy,
        &target.action,
        &target.source_revision,
        &target.input_digest,
        artifacts,
    ))
}
impl GateEvaluation {
    pub fn admission_deadline(&self) -> Option<u64> {
        if self.decision.verdict == Verdict::Pass {
            self.decision.expires_at_unix_ms
        } else {
            self.exception.as_ref().map(|a| a.expires_at_unix_ms)
        }
    }
}

/// The host must authenticate/validate the execution ledger before returning a record.
/// Implementations verify artifact bytes and ancestors through ArtifactReader.
/// Unavailability is UNKNOWN. A matching manifest alone is not execution evidence.
pub trait EvidenceSource: ArtifactReader {
    fn executed_check(&self, producer: &Producer) -> Result<Option<ExecutedCheck>>;
}

/// Stateless policy evaluation port. Decisions are observations of exact
/// evidence, never permissions to mutate a run or execute an external effect.
/// Authoritative consumers must still check their frozen policy and target.
pub trait PolicyEvaluator {
    fn evaluate(
        &self,
        request: &Request,
        source: &dyn EvidenceSource,
        now: u64,
    ) -> Result<Decision>;
    fn revalidate(
        &self,
        request: &Request,
        prior: &Decision,
        source: &dyn EvidenceSource,
        now: u64,
    ) -> Result<Decision>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicPolicyEvaluator;
impl PolicyEvaluator for DeterministicPolicyEvaluator {
    fn evaluate(
        &self,
        request: &Request,
        source: &dyn EvidenceSource,
        now: u64,
    ) -> Result<Decision> {
        evaluate(request, source, now)
    }
    fn revalidate(
        &self,
        request: &Request,
        prior: &Decision,
        source: &dyn EvidenceSource,
        now: u64,
    ) -> Result<Decision> {
        revalidate(request, prior, source, now)
    }
}
pub fn schema(kind: &str) -> Result<String> {
    let value = match kind {
        "request" => schemars::schema_for!(Request),
        "decision" => schemars::schema_for!(Decision),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidDocument,
                "gate schema must be request or decision",
            ));
        }
    };
    serde_json::to_string_pretty(&value)
        .map_err(|e| Error::new(ErrorCode::InvalidDocument, e.to_string()))
}
#[cfg(test)]
mod tests;
