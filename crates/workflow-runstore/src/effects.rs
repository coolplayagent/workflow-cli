use crate::*;
use serde::{Deserialize, Serialize};
use workflow_effects::{EffectAttempt, EffectState, ManualResolution, Observation};
use workflow_worker::Clock;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectClaim {
    Call {
        attempt: Box<EffectAttempt>,
    },
    Waiting {
        not_before_unix_ms: u64,
    },
    Manual {
        operation_key: String,
        reason: String,
    },
    Handled,
    /// The next command belongs to the ordinary executor, or the run is paused.
    Idle,
}
pub trait EffectStore: ExecutionStore {
    fn claim_effect(&mut self, lease: &Lease, clock: &dyn Clock) -> Result<EffectClaim>;
    fn observe_effect(
        &mut self,
        lease: &Lease,
        attempt_id: &str,
        observation: &Observation,
        clock: &dyn Clock,
    ) -> Result<Committed>;
    fn resolve_effect(
        &mut self,
        lease: &Lease,
        operation_key: &str,
        resolution: &ManualResolution,
        clock: &dyn Clock,
    ) -> Result<Committed>;
    fn effects(
        &mut self,
        run_id: &str,
        after_instance: u64,
        limit: u32,
    ) -> Result<Page<EffectState, u64>>;
}
