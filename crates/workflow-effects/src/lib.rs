//! Durable effect protocol and deterministic ledger. No provider or database I/O.
mod compensation;
mod ledger;
pub use compensation::compensation_key;
mod model;
mod release;
pub use ledger::*;
pub use model::*;
pub use release::*;
use workflow_ir::VersionRef;
use workflow_worker::{Capability, EffectContract, Idempotency};
pub use workflow_worker::{Error, ErrorCode, Result, Values, digest};

fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}
fn reference(r: &VersionRef) -> Result<()> {
    if !workflow_validator::identifier(&r.id) || !workflow_validator::pinned_version(&r.version) {
        return Err(invalid(
            "effect references require a stable ID and pinned version",
        ));
    }
    Ok(())
}
pub fn validate_policy(p: &EffectPolicy) -> Result<()> {
    reference(&p.identity)?;
    reference(&p.target)?;
    reference(&p.call_identity)?;
    let r = &p.retry;
    if !(1..=32).contains(&r.max_calls)
        || r.initial_backoff_ms == 0
        || r.initial_backoff_ms > r.max_backoff_ms
        || r.max_backoff_ms > 300_000
        || !(1..=86_400_000).contains(&r.total_write_ms)
    {
        return Err(invalid(
            "effect retry requires 1..32 calls, bounded positive backoff and 1..86400000 ms write window",
        ));
    }
    Ok(())
}
pub fn operation_key(run_digest: &str, instance_id: u64) -> Result<String> {
    digest(&("workflow-effect-v1", run_digest, instance_id, "primary"))
}
impl EffectIntent {
    pub fn validate(&self) -> Result<()> {
        validate_policy(&self.policy)?;
        if let Some(release) = &self.release {
            release.validate(self)?;
        }
        let c = Capability::new(self.capability.clone())?;
        if c.descriptor().effects == EffectContract::ReadOnly
            || self.schema_version != 1
            || self.instance_id == 0
            || self.created_at_unix_ms == 0
            || self.operation_key != self.expected_key()?
            || self.input_digest != digest(&self.inputs)?
            || !workflow_validator::identifier(&self.run_id)
            || !workflow_validator::identifier(&self.node_id)
            || self.command_id.is_empty()
            || self.command_digest.is_empty()
        {
            return Err(invalid(
                "effect intent identity, schema or write contract mismatch",
            ));
        }
        reference(&self.workflow)?;
        if self.dependencies.len() > 128
            || self
                .dependencies
                .iter()
                .any(|k| k == &self.operation_key || k.is_empty())
            || self
                .dependencies
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.dependencies.len()
            || self.compensates.as_ref().is_some_and(|c| {
                c.operation_key.is_empty() || c.operation_key != c.receipt.operation_key
            })
        {
            return Err(invalid("invalid effect dependency/compensation references"));
        }
        workflow_validator::validate_values(&self.capability.inputs, &self.inputs)
            .map_err(|e| invalid(&e.message))?;
        self.created_at_unix_ms
            .checked_add(self.policy.retry.total_write_ms)
            .ok_or_else(|| invalid("effect deadline overflow"))?;
        workflow_worker::to_message(self)?;
        Ok(())
    }
    pub fn expected_key(&self) -> Result<String> {
        match &self.compensates {
            Some(original) => {
                compensation_key(&original.operation_key, &self.capability.capability)
            }
            None => operation_key(&self.run_digest, self.instance_id),
        }
    }
    pub fn write_deadline(&self) -> u64 {
        let total = self
            .created_at_unix_ms
            .saturating_add(self.policy.retry.total_write_ms);
        match &self.capability.effects {
            EffectContract::Write {
                idempotency: Idempotency::Key { retention_ms, .. },
                ..
            } => total.min(self.created_at_unix_ms.saturating_add(*retention_ms)),
            _ => total,
        }
    }
    pub fn has_query(&self) -> bool {
        matches!(
            self.capability.effects,
            EffectContract::Write { query: Some(_), .. }
        )
    }
    pub fn deduplicates(&self) -> bool {
        matches!(
            self.capability.effects,
            EffectContract::Write {
                idempotency: Idempotency::Key { .. },
                ..
            }
        )
    }
}
impl EffectReceipt {
    pub fn validate(&self, intent: &EffectIntent) -> Result<()> {
        match (&intent.release, &self.release) {
            (Some(release), Some(receipt)) => {
                receipt.validate(release, intent.created_at_unix_ms)?
            }
            (None, None) => {}
            _ => return Err(invalid("release receipt and protected intent must match")),
        }
        if self.operation_key != intent.operation_key
            || self.intent_digest != digest(intent)?
            || self.target != intent.policy.target
            || self.resource_id.trim().is_empty()
            || self.resource_id.len() > 1024
            || self.provider_receipt.trim().is_empty()
            || self.provider_receipt.len() > 8192
        {
            return Err(invalid(
                "external receipt does not bind this intent, target and resource",
            ));
        }
        workflow_validator::validate_values(&intent.capability.outputs, &self.outputs)
            .map_err(|e| invalid(&e.message))
    }
}
/// Host adapter must check its configured target/principal against the frozen
/// intent, and enforce request deadlines before initiating I/O. Digests are not
/// authentication. A query must observe the provider; it cannot infer success.
pub trait EffectAdapter {
    fn execute(
        &self,
        attempt: &EffectAttempt,
        clock: &dyn workflow_worker::Clock,
    ) -> Result<Observation>;
}

impl EffectAttempt {
    /// Structural protocol validation. Only the host's durable claim confers
    /// dispatch authority; a caller-computed digest is not a permission token.
    pub fn validate(&self) -> Result<()> {
        self.intent.validate()?;
        match (&self.intent.release, self.kind, &self.release) {
            (Some(intent), CallKind::Write, Some(grant)) => {
                grant.validate(intent, self.issued_at_unix_ms)?;
                if self.deadline_unix_ms > grant.expires_at_unix_ms {
                    return Err(invalid("write outlives its release authorization"));
                }
            }
            (_, CallKind::Query, None) | (None, CallKind::Write, None) => {}
            _ => {
                return Err(invalid(
                    "write requires its current release authorization; queries do not grant writes",
                ));
            }
        }
        if self.epoch == 0
            || !(1..=self.intent.policy.retry.max_calls).contains(&self.number)
            || self.prepared_revision == 0
            || !workflow_validator::identifier(&self.attempt_id)
            || self.issued_at_unix_ms < self.intent.created_at_unix_ms
            || self.deadline_unix_ms <= self.issued_at_unix_ms
            || self.deadline_unix_ms
                > self
                    .issued_at_unix_ms
                    .saturating_add(self.intent.capability.timeout_ms)
            || (self.kind == CallKind::Write
                && self.deadline_unix_ms > self.intent.write_deadline())
            || (self.kind == CallKind::Query && !self.intent.has_query())
        {
            return Err(invalid(
                "invalid effect call scope, kind, budget or deadline",
            ));
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests;
