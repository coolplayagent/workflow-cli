use crate::*;
use std::collections::BTreeSet;
use workflow_worker::{Capability, EffectContract, FailureClass, ModelPolicyBinding};
#[derive(Clone, Debug)]
pub struct Policy {
    spec: PolicySpec,
    binding: ModelPolicyBinding,
}
pub fn failure_outcome(failure: &ModelFailure) -> workflow_worker::AdapterOutcome {
    let code = match failure {
        ModelFailure::Unavailable => "model_unavailable",
        ModelFailure::InvalidResponse => "model_invalid_response",
        ModelFailure::Refused => "model_refused",
        ModelFailure::Budget => "model_budget",
        ModelFailure::Deadline => "model_deadline",
    };
    workflow_worker::AdapterOutcome::Failed {
        code: code.into(),
        class: FailureClass::Permanent,
        message: format!(
            "model execution stopped: {code}; inspect the recorded attempt before retrying"
        ),
        evidence: vec![],
    }
}
fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidBinding, message)
}
impl Policy {
    pub fn new(spec: PolicySpec) -> Result<Self> {
        let binding = ModelPolicyBinding {
            policy: spec.policy.clone(),
            digest: digest(&spec)?,
        };
        binding.validate()?;
        let b = &spec.budget;
        if spec.schema_version != 1
            || spec.goal.trim().is_empty()
            || spec.goal.len() > 8192
            || spec.tools.len() > 16
            || !(1..=16).contains(&b.model_calls)
            || b.tool_calls > 16
            || !(1024..=262_144).contains(&b.context_bytes)
            || !(128..=65_536).contains(&b.response_bytes)
            || !(1..=32768).contains(&b.output_tokens_per_call)
            || to_message(&spec)?.len() > 131_072
        {
            return Err(invalid("model policy/budget exceeds supported bounds"));
        }
        let task = Capability::new(spec.task.clone())?;
        if task.descriptor().effects != EffectContract::ReadOnly {
            return Err(invalid("model tasks must be read-only"));
        }
        for f in [
            ModelFailure::Unavailable,
            ModelFailure::InvalidResponse,
            ModelFailure::Refused,
            ModelFailure::Budget,
            ModelFailure::Deadline,
        ] {
            let workflow_worker::AdapterOutcome::Failed { code, class, .. } = failure_outcome(&f)
            else {
                unreachable!()
            };
            if spec.task.error_codes.get(&code) != Some(&class) {
                return Err(invalid("task must declare permanent model failure codes"));
            }
        }
        let mut ids = BTreeSet::new();
        for tool in &spec.tools {
            Capability::new(tool.clone())?;
            if tool.effects != EffectContract::ReadOnly
                || tool.capability == spec.task.capability
                || !ids.insert((tool.capability.id.clone(), tool.capability.version.clone()))
            {
                return Err(invalid(
                    "model tools require unique, nonrecursive read-only contracts",
                ));
            }
        }
        Ok(Self { spec, binding })
    }
    pub fn spec(&self) -> &PolicySpec {
        &self.spec
    }
    pub fn binding(&self) -> &ModelPolicyBinding {
        &self.binding
    }
}
