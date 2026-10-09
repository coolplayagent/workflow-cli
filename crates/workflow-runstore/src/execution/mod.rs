//! Host-owned execution contract. Lease fencing protects commits; it is not remote authentication.
mod authority;
mod model;
use crate::*;
pub use authority::Authority;
pub use model::*;
use workflow_worker::{Clock, WorkResult};

pub const MAX_LEASE_MS: u64 = 300_000;
pub const MAX_ATTEMPTS: u32 = 3;
pub const MAX_EXECUTION_RECORDS: u64 = 10_000;

pub trait ExecutionStore: RunStore {
    fn acquire(&mut self, request: &LeaseRequest, clock: &dyn Clock) -> Result<Lease>;
    fn renew(&mut self, lease: &Lease, ttl_ms: u64, clock: &dyn Clock) -> Result<Lease>;
    fn release(&mut self, lease: &Lease, clock: &dyn Clock) -> Result<()>;
    fn tick_due(&mut self, lease: &Lease, clock: &dyn Clock) -> Result<Option<Committed>>;
    fn claim_next(&mut self, lease: &Lease, clock: &dyn Clock) -> Result<Claimed>;
    fn progress(&mut self, lease: &Lease, attempt_id: &str, clock: &dyn Clock) -> Result<()>;
    fn finish_task(
        &mut self,
        lease: &Lease,
        attempt_id: &str,
        result: &WorkResult,
        clock: &dyn Clock,
    ) -> Result<Committed>;
    fn fail_task(
        &mut self,
        lease: &Lease,
        attempt_id: &str,
        error: &workflow_worker::Error,
        clock: &dyn Clock,
    ) -> Result<()>;
    fn model_checkpoint(
        &mut self,
        request: &workflow_worker::WorkRequest,
        clock: &dyn Clock,
    ) -> Result<Option<workflow_models::ModelCheckpoint>>;
    fn save_model_checkpoint(
        &mut self,
        request: &workflow_worker::WorkRequest,
        previous_digest: Option<&str>,
        checkpoint: &workflow_models::ModelCheckpoint,
        clock: &dyn Clock,
    ) -> Result<()>;
    fn execution_history(
        &mut self,
        run_id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<ExecutionRecord, u64>>;
}
pub fn lease_deadline(now: u64, ttl: u64) -> Result<u64> {
    if now == 0 || !(1..=MAX_LEASE_MS).contains(&ttl) {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "positive time and lease TTL 1..300000 ms required",
        ));
    }
    now.checked_add(ttl)
        .ok_or_else(|| Error::new(ErrorCode::InvalidRequest, "lease deadline overflow"))
}

/// Bind production provenance to the actual persisted worker request. The host
/// separately supplies and attests the source checkout revision.
pub fn artifact_producer(
    request: &workflow_worker::WorkRequest,
) -> Result<workflow_artifacts::Producer> {
    request.validate_shape()?;
    let workflow_worker::InvocationScope::Workflow {
        run_id,
        node_instance_id,
        attempt_id,
        ..
    } = &request.scope
    else {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "run artifact production requires a workflow request",
        ));
    };
    Ok(workflow_artifacts::Producer {
        run_id: run_id.clone(),
        node_instance_id: node_instance_id.clone(),
        attempt_id: attempt_id.clone(),
        request_digest: workflow_worker::digest(request)?,
        input_digest: request.input_digest.clone(),
    })
}

/// Project a recovered, accepted worker observation for the evidence checker.
/// The caller must verify the journal, request/grant/result and committed event first.
pub fn executed_check(
    run_digest: &str,
    prepared: &PreparedTask,
    result: &workflow_worker::WorkResult,
    settled_at_unix_ms: u64,
) -> Result<workflow_gates::ExecutedCheck> {
    use workflow_gates::CheckOutcome;
    use workflow_worker::{AdapterOutcome, FailureClass, InvocationScope};
    let producer = artifact_producer(&prepared.request)?;
    let InvocationScope::Workflow { node_id, .. } = &prepared.request.scope else {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "workflow check required",
        ));
    };
    if producer.request_digest != result.request_digest {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "check request/result mismatch",
        ));
    }
    let (outcome, evidence) = match &result.outcome {
        AdapterOutcome::Succeeded { outputs, evidence } => (
            CheckOutcome::Succeeded {
                outputs: outputs.clone(),
            },
            evidence,
        ),
        AdapterOutcome::Failed {
            code,
            class,
            evidence,
            ..
        } => (
            if *class == FailureClass::Permanent {
                CheckOutcome::Failed { code: code.clone() }
            } else {
                CheckOutcome::Inconclusive { code: code.clone() }
            },
            evidence,
        ),
    };
    Ok(workflow_gates::ExecutedCheck {
        producer,
        run_digest: run_digest.into(),
        node_id: node_id.clone(),
        capability: prepared.request.capability.clone(),
        contract_digest: prepared.request.contract_digest.clone(),
        completed_at_unix_ms: result.completed_at_unix_ms,
        settled_at_unix_ms,
        outcome,
        evidence: evidence
            .iter()
            .map(|e| workflow_artifacts::ArtifactLink {
                artifact_id: e.artifact_id.clone(),
                digest: e.digest.clone(),
            })
            .collect(),
    })
}
