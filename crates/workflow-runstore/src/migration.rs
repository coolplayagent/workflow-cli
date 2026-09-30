use crate::*;
use schemars::JsonSchema;
pub use workflow_kernel::{MigrationPlan, MigrationRequest};

/// Fenced execution proof for a protected definition migration. The plan and
/// complete target contract are retained by the corresponding kernel event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationAuthority {
    pub epoch: u64,
    pub generation: String,
    pub actor: String,
    pub plan_digest: String,
    pub source_revision: u64,
    pub event_id: String,
    pub event_revision: u64,
    pub previous_delivery_sequence: u64,
    pub retired_commands: Vec<String>,
    pub at_unix_ms: u64,
}

pub fn migration_generation(run_id: &str, event_id: &str, plan_digest: &str) -> Result<String> {
    Ok(workflow_worker::digest(&(
        "definition migration",
        run_id,
        event_id,
        plan_digest,
    ))?)
}
pub fn migration_event_id(migration_id: &str) -> Result<String> {
    validate_id(migration_id)?;
    Ok(format!(
        "migration-{}",
        &workflow_worker::digest(&migration_id)?[7..]
    ))
}
pub fn migration_delivery_id(plan_digest: &str, sequence: u64) -> Result<String> {
    let hash = plan_digest
        .strip_prefix("sha256:")
        .filter(|s| s.len() == 64)
        .ok_or_else(|| Error::new(ErrorCode::InvalidRequest, "invalid migration plan digest"))?;
    Ok(format!("migration-{hash}-{sequence}"))
}
impl MigrationAuthority {
    pub fn validate(&self, run_id: &str) -> Result<()> {
        validate_id(&self.actor)?;
        validate_id(&self.event_id)?;
        let hash = |value: &str| {
            value.strip_prefix("sha256:").is_some_and(|v| {
                v.len() == 64
                    && v.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        };
        if !hash(&self.plan_digest)
            || !hash(&self.generation)
            || self.generation != migration_generation(run_id, &self.event_id, &self.plan_digest)?
            || self.source_revision == 0
            || self.source_revision.checked_add(1) != Some(self.event_revision)
            || self.retired_commands.len() > 10000
            || self.retired_commands.iter().any(|id| !hash(id))
            || self
                .retired_commands
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.retired_commands.len()
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "invalid definition migration authority",
            ));
        }
        Ok(())
    }
}

pub trait MigrationStore: ExecutionStore {
    /// Preview only; this does not change the source or authorize execution.
    fn plan_migration(&mut self, run_id: &str, request: &MigrationRequest)
    -> Result<MigrationPlan>;
    /// Trusted local administrative actor, or authenticated actor stamped by the
    /// shared service. Applies a reviewed plan under the current ownership lease.
    fn migrate_definition(
        &mut self,
        lease: &Lease,
        plan: &MigrationPlan,
        actor: &str,
        clock: &dyn workflow_worker::Clock,
    ) -> Result<Committed>;
    fn historical_snapshot(&mut self, run_id: &str, revision: u64) -> Result<Snapshot>;
}
