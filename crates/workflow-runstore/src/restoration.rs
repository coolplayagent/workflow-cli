//! Explicit disaster-recovery ownership and external-effect reconciliation.
use crate::*;
use schemars::JsonSchema;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBarrier {
    /// Random restore generation. This is a fencing identity, not a credential.
    pub generation: String,
    pub backup_digest: String,
    pub source_revision: u64,
    pub source_state_digest: String,
    pub restored_by: String,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryAcknowledgement {
    pub resolution_id: String,
    /// Operator verified that every post-backup admitted write is accounted for.
    pub no_missing_effect_intents: bool,
    pub generation: String,
    pub backup_digest: String,
    pub actor: String,
    /// Must describe actual source retirement and post-backup provider audit.
    pub evidence: String,
    pub reason: String,
}
fn hash(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    })
}
fn text(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max
}
impl RecoveryBarrier {
    pub fn validate(&self) -> Result<()> {
        if !hash(&self.generation)
            || !hash(&self.backup_digest)
            || self.source_revision == 0
            || !hash(&self.source_state_digest)
            || !text(&self.restored_by, 128)
            || !text(&self.reason, 1024)
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "invalid recovery generation, backup identity or actor annotation",
            ));
        }
        Ok(())
    }
}
impl RecoveryAcknowledgement {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.resolution_id)?;
        if !self.no_missing_effect_intents
            || !hash(&self.generation)
            || !hash(&self.backup_digest)
            || !text(&self.actor, 128)
            || !text(&self.reason, 1024)
            || !text(&self.evidence, 8192)
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "recovery requires exact backup/generation and bounded operator audit evidence",
            ));
        }
        Ok(())
    }
}

pub trait RestorationStore: RunStore {
    fn import_restored_effect(
        &mut self,
        lease: &Lease,
        import: &RestoredEffect,
        clock: &dyn workflow_worker::Clock,
    ) -> Result<Committed>;
    fn recovery_barrier(&mut self, run_id: &str) -> Result<Option<RecoveryBarrier>>;
    /// Trusted local administrative evidence, not an authentication interface.
    fn acknowledge_recovery(
        &mut self,
        run_id: &str,
        resolution: &RecoveryAcknowledgement,
        clock: &dyn workflow_worker::Clock,
    ) -> Result<bool>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoredEffect {
    pub intent: workflow_effects::EffectIntent,
    pub resolution: workflow_effects::ManualResolution,
}
