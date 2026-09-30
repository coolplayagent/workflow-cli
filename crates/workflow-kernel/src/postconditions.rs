use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use workflow_gates::Policy;
pub use workflow_gates::{GateContext, GateEvaluation};
use workflow_ir::{Binding, NodeKind, TerminalOutcome, ValueType, VersionRef, Workflow};

/// Mandatory success postcondition, frozen with its graph and policy in the run bundle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Postcondition {
    pub workflow: VersionRef,
    pub node_id: String,
    pub policy: Policy,
    pub action: VersionRef,
    pub repository: Binding,
    pub revision: Binding,
    /// The current frame's task whose resolved inputs identify the checked subject.
    pub input_node: String,
    pub artifacts: Binding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception: Option<workflow_gates::ApprovalRequirement>,
}
fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidBundle, message)
}
fn precedes(w: &Workflow, from: &str, to: &str) -> bool {
    let mut pending = vec![from];
    let mut seen = BTreeSet::new();
    while let Some(n) = pending.pop() {
        if !seen.insert(n) {
            continue;
        }
        for e in w.edges.iter().filter(|e| e.from == n) {
            if e.to == to {
                return true;
            }
            pending.push(&e.to);
        }
    }
    false
}
pub(crate) fn links_type() -> ValueType {
    ValueType::Array {
        items: Box::new(ValueType::Object {
            fields: BTreeMap::from([
                ("artifact_id".into(), ValueType::String),
                ("digest".into(), ValueType::String),
            ]),
        }),
    }
}
fn binding(w: &Workflow, node: &str, b: &Binding, ty: &ValueType) -> Result<()> {
    let field = match b {
        Binding::Literal { value } => {
            return if ty.accepts(value) {
                Ok(())
            } else {
                Err(invalid("postcondition literal has the wrong type"))
            };
        }
        Binding::WorkflowInput { field } => w.inputs.get(field),
        Binding::NodeOutput {
            node: source,
            field,
        } => {
            if source != node && !precedes(w, source, node) {
                return Err(invalid(
                    "postcondition output must belong to itself or an ancestor",
                ));
            }
            w.nodes
                .iter()
                .find(|n| &n.id == source)
                .and_then(|n| n.outputs.get(field))
        }
    }
    .ok_or_else(|| invalid("postcondition binding references a missing field"))?;
    if !field.required || &field.value_type != ty {
        return Err(invalid(
            "postcondition binding needs an exact required field type",
        ));
    }
    Ok(())
}
pub(crate) fn validate(
    spec: &BundleSpec,
    workflows: &BTreeMap<String, Workflow>,
    capabilities: &BTreeMap<String, workflow_worker::Capability>,
) -> Result<()> {
    if spec.postconditions.len() > 256 {
        return Err(invalid("at most 256 postconditions"));
    }
    let mut nodes = BTreeSet::new();
    let mut policies = BTreeMap::new();
    for g in &spec.postconditions {
        workflow_gates::validate_policy(&g.policy).map_err(|e| invalid(e.message))?;
        if !workflow_validator::identifier(&g.action.id)
            || !workflow_validator::pinned_version(&g.action.version)
        {
            return Err(invalid("gate action requires an exact identity"));
        }
        let wk = crate::bundle::key(&g.workflow);
        let w = workflows
            .get(&wk)
            .ok_or_else(|| invalid("postcondition workflow missing"))?;
        let node = w
            .nodes
            .iter()
            .find(|n| n.id == g.node_id)
            .ok_or_else(|| invalid("postcondition node missing"))?;
        if !matches!(
            node.kind,
            NodeKind::Task { .. }
                | NodeKind::Terminal {
                    outcome: TerminalOutcome::Succeeded
                }
        ) || !nodes.insert((wk, g.node_id.clone()))
        {
            return Err(invalid(
                "postconditions require a unique task or successful terminal",
            ));
        }
        let hash = workflow_gates::digest(&g.policy)?;
        let pk = crate::bundle::key(&g.policy.identity);
        if policies
            .insert(pk, hash.clone())
            .is_some_and(|old| old != hash)
        {
            return Err(invalid("policy version names conflicting content"));
        }
        let subject = w
            .nodes
            .iter()
            .find(|n| n.id == g.input_node)
            .ok_or_else(|| invalid("gate input task missing"))?;
        if !matches!(subject.kind, NodeKind::Task { .. })
            || (subject.id != node.id && !precedes(w, &subject.id, &node.id))
        {
            return Err(invalid("gate input task must be itself or an ancestor"));
        }
        binding(w, &node.id, &g.repository, &ValueType::String)?;
        binding(w, &node.id, &g.revision, &ValueType::String)?;
        binding(w, &node.id, &g.artifacts, &links_type())?;
        if let Some(exception) = &g.exception {
            let wait = spec
                .wait_policies
                .iter()
                .find(|p| p.workflow == g.workflow && p.node_id == exception.node_id)
                .ok_or_else(|| invalid("exception needs a declared human approval wait"))?;
            if wait.policy.kind != crate::WaitKind::HumanApproval
                || wait.policy.exception.is_none()
                || wait.policy.subjects.get(&exception.subject_field)
                    != Some(&crate::SubjectKind::Digest)
                || !precedes(w, &exception.node_id, &g.node_id)
                || precedes(w, &g.node_id, &exception.node_id)
            {
                return Err(invalid(
                    "exception requires a prior human wait with a separate exception policy and digest subject",
                ));
            }
        }
        for q in &g.policy.requirements {
            let checker = w
                .nodes
                .iter()
                .find(|n| n.id == q.node_id)
                .ok_or_else(|| invalid("required checker task missing"))?;
            let NodeKind::Task { capability, policy } = &checker.kind else {
                return Err(invalid("required checker must be a task"));
            };
            if checker.id != node.id && precedes(w, &node.id, &checker.id) {
                return Err(invalid("checker cannot depend on its own pending gate"));
            }
            let c = &capabilities[&crate::bundle::key(capability)];
            if policy.is_some()
                || c.descriptor().effects != workflow_worker::EffectContract::ReadOnly
            {
                return Err(invalid(
                    "required checker must be an independent read-only task without a model policy",
                ));
            }
            if capability != &q.capability
                || c.digest() != q.contract_digest
                || !checker
                    .outputs
                    .get(&q.pass_field)
                    .is_some_and(|f| f.value_type == ValueType::Boolean)
            {
                return Err(invalid(
                    "checker capability, contract or Boolean output differs from the gate policy",
                ));
            }
        }
    }
    Ok(())
}
