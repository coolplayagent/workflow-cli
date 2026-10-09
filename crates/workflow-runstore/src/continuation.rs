use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Values copied from acknowledged task outputs, not assertions invented by a model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifiedFact {
    pub instance_id: u64,
    pub field: String,
    pub value: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    pub schema_version: u32,
    pub source_run_id: String,
    pub source_run_digest: String,
    pub source_revision: u64,
    pub objective: String,
    pub plan: Vec<String>,
    pub verified_facts: Vec<VerifiedFact>,
    pub artifacts: Vec<workflow_artifacts::ArtifactLink>,
    pub failures: Vec<String>,
    pub remaining_work: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContinuationPlan {
    pub schema_version: u32,
    pub handoff: Handoff,
    pub handoff_input: String,
    pub successor: StartRun,
}
impl ContinuationPlan {
    pub fn validate(&self) -> Result<()> {
        let h = &self.handoff;
        if self.schema_version != 1
            || h.schema_version != 1
            || self.successor.schema_version != 1
            || h.source_revision == 0
            || h.objective.trim().is_empty()
            || h.objective.len() > 8192
            || self.successor.run_id == h.source_run_id
            || !(h.source_run_digest.len() == 71
                && h.source_run_digest.starts_with("sha256:")
                && h.source_run_digest[7..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()))
            || [&h.plan, &h.failures, &h.remaining_work]
                .iter()
                .any(|v| v.len() > 128 || v.iter().any(|s| s.trim().is_empty() || s.len() > 8192))
            || h.verified_facts.len() > 128
            || h.artifacts.len() > 128
            || workflow_worker::to_message(h)?.len() > 131072
            || self.successor.inputs.get(&self.handoff_input)
                != Some(
                    &serde_json::to_value(h)
                        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "handoff encoding"))?,
                )
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "invalid handoff or successor input differs from exact handoff",
            ));
        }
        validate_id(&h.source_run_id)?;
        validate_id(&self.handoff_input)?;
        workflow_kernel::Engine::start(
            workflow_kernel::CompiledBundle::compile(self.successor.bundle.clone())?,
            &self.successor.run_id,
            self.successor.inputs.clone(),
            self.successor.started_at_unix_ms,
            self.successor.limits.clone(),
        )?;
        Ok(())
    }
}
/// Preparing durably reserves exactly one successor at a successful segment
/// boundary. Starting the recorded successor is an idempotent second commit;
/// after a crash callers repeat both operations with the same plan.
pub trait ContinuationStore: ExecutionStore {
    fn prepare_continuation(
        &mut self,
        lease: &Lease,
        plan: &ContinuationPlan,
        clock: &dyn workflow_worker::Clock,
    ) -> Result<ContinuationPlan>;
    fn continuation(&mut self, run_id: &str) -> Result<Option<ContinuationPlan>>;
}
