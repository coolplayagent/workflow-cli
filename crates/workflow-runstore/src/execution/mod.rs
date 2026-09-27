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
