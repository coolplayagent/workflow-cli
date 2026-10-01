//! Reviewed workflow assets and pure planning. No host adapter or provider is
//! called here. Catalogs authenticate reviewers and preserve immutable versions.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use workflow_ir::{Field, NodeKind, VersionRef};
use workflow_kernel::{BundleSpec, CompiledBundle, Values};

mod planning;
mod review;
pub use planning::*;
pub use review::*;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub path: String,
    pub message: String,
}
pub type Result<T> = std::result::Result<T, Error>;
fn invalid(path: impl Into<String>, message: impl Into<String>) -> Error {
    Error {
        path: path.into(),
        message: message.into(),
    }
}
pub fn digest<T: Serialize>(value: &T) -> Result<String> {
    workflow_worker::digest(value).map_err(|_| invalid("$", "template encoding failed"))
}
fn name(value: &str, path: &str) -> Result<()> {
    if !workflow_validator::identifier(value) {
        return Err(invalid(path, "stable identifier required"));
    }
    Ok(())
}
fn reference(value: &VersionRef, path: &str) -> Result<()> {
    name(&value.id, path)?;
    if !workflow_validator::pinned_version(&value.version) {
        return Err(invalid(path, "exact pinned version required"));
    }
    Ok(())
}
pub fn identity_key(value: &VersionRef) -> String {
    format!("{}@{}", value.id, value.version)
}
fn text(value: &str, path: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 4096 || value.contains('\0') {
        return Err(invalid(path, "bounded nonempty description required"));
    }
    Ok(())
}
fn hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|v| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    pub field: Field,
    pub description: String,
    pub project_overridable: bool,
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub allowed_values: Vec<serde_json::Value>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    /// Operator-attested business facts required before choosing this template.
    pub requires: BTreeMap<String, String>,
    pub excludes: BTreeMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub max_task_timeout_ms: u64,
    pub max_loop_iterations: u32,
    pub max_loop_duration_ms: u64,
    pub max_wait_ms: u64,
    pub max_model_calls_per_task: u32,
    pub max_effect_calls_per_attempt: u32,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub schema_version: u32,
    pub identity: VersionRef,
    pub owner_role: String,
    pub goal: String,
    pub applicability: Applicability,
    pub roles: BTreeMap<String, String>,
    pub parameters: BTreeMap<String, Parameter>,
    pub deliverables: BTreeMap<String, String>,
    /// Human-readable handling instructions, required for every regression case.
    pub exception_paths: BTreeMap<Scenario, String>,
    pub budget: Budget,
    pub bundle: BundleSpec,
}
impl Template {
    pub fn validate(&self) -> Result<CompiledBundle> {
        if serde_json::to_vec(self)
            .map_err(|_| invalid("$", "template encoding failed"))?
            .len()
            > 1_048_576
        {
            return Err(invalid("$", "template exceeds 1 MiB"));
        }
        if self.schema_version != 1 {
            return Err(invalid("schema_version", "template schema 1 required"));
        }
        reference(&self.identity, "identity")?;
        name(&self.owner_role, "owner_role")?;
        text(&self.goal, "goal")?;
        if self.roles.is_empty()
            || self.roles.len() > 32
            || !self.roles.contains_key(&self.owner_role)
        {
            return Err(invalid(
                "roles",
                "declare the process owner role and at most 32 roles",
            ));
        }
        for (k, v) in self
            .roles
            .iter()
            .chain(self.deliverables.iter())
            .chain(self.applicability.requires.iter())
            .chain(self.applicability.excludes.iter())
        {
            name(k, k)?;
            text(v, k)?;
        }
        if self.deliverables.is_empty()
            || self.deliverables.len() > 64
            || self.applicability.requires.is_empty()
            || self.applicability.requires.len() > 32
            || self.applicability.excludes.len() > 32
        {
            return Err(invalid(
                "applicability",
                "bounded business prerequisites and deliverables required",
            ));
        }
        if self
            .applicability
            .requires
            .keys()
            .any(|k| self.applicability.excludes.contains_key(k))
        {
            return Err(invalid(
                "applicability",
                "a fact cannot be both required and excluded",
            ));
        }
        if self
            .exception_paths
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
            != Scenario::ALL.into_iter().collect()
        {
            return Err(invalid(
                "exception_paths",
                "document all five execution scenarios",
            ));
        }
        for value in self.exception_paths.values() {
            text(value, "exception_paths")?;
        }
        let bundle = CompiledBundle::compile(self.bundle.clone())
            .map_err(|e| invalid("bundle", e.message))?;
        let root = self
            .bundle
            .workflows
            .iter()
            .find(|w| w.id == self.bundle.root.id && w.version == self.bundle.root.version)
            .ok_or_else(|| invalid("bundle.root", "root missing"))?;
        if self.parameters.len() > 64 || self.parameters.keys().ne(root.inputs.keys()) {
            return Err(invalid(
                "parameters",
                "parameters must exactly name the root input contract",
            ));
        }
        for (key, p) in &self.parameters {
            let path = format!("parameters.{key}");
            if p.field != root.inputs[key] || p.allowed_values.len() > 64 {
                return Err(invalid(
                    &path,
                    "parameter contract differs from root inputs",
                ));
            }
            text(&p.description, &path)?;
            for v in p.default.iter().chain(p.allowed_values.iter()) {
                if !p.field.value_type.accepts(v) {
                    return Err(invalid(&path, "parameter value has the wrong type"));
                }
            }
            if let Some(v) = &p.default
                && !p.allowed_values.is_empty()
                && !p.allowed_values.contains(v)
            {
                return Err(invalid(&path, "default is outside allowed values"));
            }
            if !p.project_overridable && p.default.is_none() {
                return Err(invalid(&path, "fixed parameter requires a default"));
            }
        }
        validate_budget(self)?;
        let successes = root
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    n.kind,
                    NodeKind::Terminal {
                        outcome: workflow_ir::TerminalOutcome::Succeeded
                    }
                )
            })
            .collect::<Vec<_>>();
        if successes.is_empty()
            || successes.iter().any(|node| {
                !self
                    .bundle
                    .postconditions
                    .iter()
                    .any(|g| g.workflow == self.bundle.root && g.node_id == node.id)
            })
        {
            return Err(invalid(
                "bundle.postconditions",
                "every successful root terminal requires an independent acceptance gate",
            ));
        }
        if self.bundle.effect_bindings.iter().any(|e| {
            e.compensates.is_none() && e.release.as_ref().is_none_or(|r| r.approvals.is_empty())
        }) {
            return Err(invalid(
                "bundle.effect_bindings",
                "delivery effects require frozen quality and approval authorization",
            ));
        }
        for w in &self.bundle.workflows {
            for n in &w.nodes {
                if let NodeKind::Task { capability, .. } = &n.kind {
                    let descriptor = self
                        .bundle
                        .capabilities
                        .iter()
                        .find(|c| &c.capability == capability)
                        .ok_or_else(|| invalid("bundle.capabilities", "task capability missing"))?;
                    if descriptor.effects != workflow_worker::EffectContract::ReadOnly
                        && !self.bundle.effect_bindings.iter().any(|e| {
                            e.workflow.id == w.id
                                && e.workflow.version == w.version
                                && e.node_id == n.id
                        })
                    {
                        return Err(invalid(
                            "bundle.effect_bindings",
                            "every write task requires a frozen managed effect binding",
                        ));
                    }
                }
                if matches!(n.kind, NodeKind::Wait { .. })
                    && !self.bundle.wait_policies.iter().any(|p| {
                        p.workflow.id == w.id
                            && p.workflow.version == w.version
                            && p.node_id == n.id
                    })
                {
                    return Err(invalid(
                        format!("bundle.workflows.{}.{}", w.id, n.id),
                        "template waits need frozen responder and subject policies",
                    ));
                }
            }
        }
        Ok(bundle)
    }
    pub fn content_digest(&self) -> Result<String> {
        self.validate()?;
        digest(self)
    }
}
fn validate_budget(t: &Template) -> Result<()> {
    let b = &t.budget;
    if b.max_task_timeout_ms == 0
        || b.max_loop_iterations == 0
        || b.max_loop_duration_ms == 0
        || b.max_wait_ms == 0
        || b.max_model_calls_per_task == 0
        || b.max_effect_calls_per_attempt == 0
    {
        return Err(invalid("budget", "all budget ceilings must be positive"));
    }
    for c in &t.bundle.capabilities {
        if c.timeout_ms > b.max_task_timeout_ms {
            return Err(invalid(
                "budget.max_task_timeout_ms",
                "capability timeout exceeds template budget",
            ));
        }
    }
    for w in &t.bundle.workflows {
        for n in &w.nodes {
            match n.kind {
                NodeKind::Loop {
                    max_iterations,
                    deadline_ms,
                    ..
                } if max_iterations > b.max_loop_iterations
                    || deadline_ms > b.max_loop_duration_ms =>
                {
                    return Err(invalid("budget", "loop exceeds template budget"));
                }
                NodeKind::Wait { timeout_ms, .. } if timeout_ms > b.max_wait_ms => {
                    return Err(invalid(
                        "budget.max_wait_ms",
                        "wait exceeds template budget",
                    ));
                }
                _ => {}
            }
        }
    }
    for p in &t.bundle.model_policies {
        if p.budget.model_calls > b.max_model_calls_per_task {
            return Err(invalid(
                "budget.max_model_calls_per_task",
                "model policy exceeds template budget",
            ));
        }
    }
    for p in &t.bundle.effect_bindings {
        if p.policy.retry.max_calls > b.max_effect_calls_per_attempt {
            return Err(invalid(
                "budget.max_effect_calls_per_attempt",
                "effect policy exceeds template budget",
            ));
        }
    }
    Ok(())
}
