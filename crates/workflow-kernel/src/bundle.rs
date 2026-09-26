use crate::{Error, ErrorCode, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use workflow_ir::*;
use workflow_worker::{Capability, CapabilityDescriptor};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BundleSpec {
    pub schema_version: u32,
    pub root: VersionRef,
    pub workflows: Vec<Workflow>,
    pub capabilities: Vec<CapabilityDescriptor>,
}
#[derive(Clone, Debug)]
pub struct CompiledBundle {
    pub(crate) workflows: BTreeMap<String, Workflow>,
    pub(crate) capabilities: BTreeMap<String, Capability>,
    pub(crate) cancellations: BTreeMap<(String, String), Vec<BTreeSet<String>>>,
    spec: BundleSpec,
    digest: String,
}
pub(crate) fn key(r: &VersionRef) -> String {
    format!("{}@{}", r.id, r.version)
}
pub(crate) fn workflow_key(w: &Workflow) -> String {
    format!("{}@{}", w.id, w.version)
}
impl CompiledBundle {
    pub fn compile(mut spec: BundleSpec) -> Result<Self> {
        workflow_worker::to_message(&spec)?;
        if spec.schema_version != 1
            || spec.workflows.is_empty()
            || spec.workflows.len() > 128
            || spec.capabilities.len() > 256
        {
            return Err(Error::new(
                ErrorCode::InvalidBundle,
                "bundle requires schema 1, 1..128 workflows and at most 256 capabilities",
            ));
        }
        let mut workflows = BTreeMap::new();
        let mut total_nodes = 0;
        let mut total_edges = 0;
        for w in &mut spec.workflows {
            let json = w
                .canonical_json()
                .map_err(|e| Error::new(ErrorCode::InvalidBundle, e.to_string()))?;
            *w = workflow_ir::parse(&json, Format::Json, "bundle")
                .map_err(|e| Error::new(ErrorCode::InvalidBundle, e.message))?;
            let diagnostics = workflow_validator::validate(w, "bundle");
            if let Some(d) = diagnostics.first() {
                return Err(Error::new(
                    ErrorCode::InvalidBundle,
                    format!("{}: {}: {}", w.id, d.path, d.message),
                ));
            }
            total_nodes += w.nodes.len();
            total_edges += w.edges.len();
            if workflows.insert(workflow_key(w), w.clone()).is_some() {
                return Err(Error::new(
                    ErrorCode::InvalidBundle,
                    "duplicate workflow ID/version",
                ));
            }
        }
        if total_nodes > 4096 || total_edges > 16384 {
            return Err(Error::new(
                ErrorCode::InvalidBundle,
                "bundle exceeds 4096 nodes or 16384 edges",
            ));
        }
        if !workflows.contains_key(&key(&spec.root)) {
            return Err(Error::new(
                ErrorCode::MissingReference,
                "bundle root is missing",
            ));
        }
        let mut capabilities = BTreeMap::new();
        for d in &spec.capabilities {
            let c = Capability::new(d.clone())
                .map_err(|e| Error::new(ErrorCode::InvalidBundle, e.message))?;
            if capabilities.insert(key(&d.capability), c).is_some() {
                return Err(Error::new(
                    ErrorCode::InvalidBundle,
                    "duplicate capability ID/version",
                ));
            }
        }
        let mut dependencies: BTreeMap<String, BTreeSet<String>> = workflows
            .keys()
            .map(|k| (k.clone(), BTreeSet::new()))
            .collect();
        for capability in capabilities.values() {
            if let workflow_worker::EffectContract::Write {
                query,
                compensation,
                ..
            } = &capability.descriptor().effects
            {
                for reference in [query, compensation].into_iter().flatten() {
                    if !capabilities.contains_key(&key(reference)) {
                        return Err(Error::new(
                            ErrorCode::MissingReference,
                            format!("missing effect capability {}", key(reference)),
                        ));
                    }
                }
            }
        }
        let mut cancellations = BTreeMap::new();
        for (wk, w) in &workflows {
            returns(w)?;
            for node in &w.nodes {
                match &node.kind {
                    NodeKind::Task { capability, policy } => {
                        if policy.is_some() {
                            return Err(Error::new(
                                ErrorCode::UnsupportedPolicy,
                                format!("{}.{} requires a model policy adapter", w.id, node.id),
                            ));
                        }
                        let d = capabilities
                            .get(&key(capability))
                            .ok_or_else(|| {
                                Error::new(
                                    ErrorCode::MissingReference,
                                    format!("missing capability {}", key(capability)),
                                )
                            })?
                            .descriptor();
                        if node.inputs != d.inputs || node.outputs != d.outputs {
                            return Err(Error::new(
                                ErrorCode::ContractMismatch,
                                format!("{}.{} capability contracts differ", w.id, node.id),
                            ));
                        }
                    }
                    NodeKind::Subworkflow {
                        workflow: reference,
                    }
                    | NodeKind::Loop {
                        body: reference, ..
                    } => {
                        let child = workflows.get(&key(reference)).ok_or_else(|| {
                            Error::new(
                                ErrorCode::MissingReference,
                                format!("missing workflow {}", key(reference)),
                            )
                        })?;
                        if node.inputs != child.inputs || node.outputs != returns(child)? {
                            return Err(Error::new(
                                ErrorCode::ContractMismatch,
                                format!("{}.{} child contracts differ", w.id, node.id),
                            ));
                        }
                        dependencies.get_mut(wk).unwrap().insert(key(reference));
                    }
                    NodeKind::Wait { .. } => {}
                    _ if !node.outputs.is_empty() => {
                        return Err(Error::new(
                            ErrorCode::ContractMismatch,
                            format!(
                                "{}.{} control nodes do not produce external outputs",
                                w.id, node.id
                            ),
                        ));
                    }
                    _ => {}
                }
                if matches!(
                    node.kind,
                    NodeKind::Join {
                        remaining: RemainingPolicy::CancelAndReconcile,
                        ..
                    }
                ) {
                    cancellations.insert((wk.clone(), node.id.clone()), cancel_groups(w, node)?);
                }
            }
        }
        // Remove leaves. Any remaining dependency is a recursion cycle, including loop bodies.
        while !dependencies.is_empty() {
            let leaves: BTreeSet<_> = dependencies
                .iter()
                .filter(|(_, ds)| ds.is_empty())
                .map(|(id, _)| id.clone())
                .collect();
            if leaves.is_empty() {
                return Err(Error::new(
                    ErrorCode::RecursiveBundle,
                    "subworkflow/loop dependency cycle",
                ));
            }
            dependencies.retain(|id, _| !leaves.contains(id));
            for ds in dependencies.values_mut() {
                ds.retain(|d| !leaves.contains(d));
            }
        }
        spec.workflows.sort_by_key(workflow_key);
        spec.capabilities.sort_by_key(|d| key(&d.capability));
        let digest = workflow_worker::digest(&spec)?;
        Ok(Self {
            workflows,
            capabilities,
            cancellations,
            spec,
            digest,
        })
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn spec(&self) -> &BundleSpec {
        &self.spec
    }
    pub(crate) fn root(&self) -> &Workflow {
        &self.workflows[&key(&self.spec.root)]
    }
}
fn returns(w: &Workflow) -> Result<Contract> {
    let mut contract = None;
    for node in &w.nodes {
        if matches!(
            node.kind,
            NodeKind::Terminal {
                outcome: TerminalOutcome::Succeeded
            }
        ) {
            if contract.as_ref().is_some_and(|c| c != &node.inputs) {
                return Err(Error::new(
                    ErrorCode::ContractMismatch,
                    format!(
                        "{} successful terminals must declare the same return inputs",
                        w.id
                    ),
                ));
            }
            contract = Some(node.inputs.clone());
        }
    }
    Ok(contract.unwrap_or_default())
}
fn cancel_groups(w: &Workflow, join: &Node) -> Result<Vec<BTreeSet<String>>> {
    let incoming: Vec<_> = w.edges.iter().filter(|e| e.to == join.id).collect();
    // A cancellable any-join needs a closed fork region with disjoint branches.
    // Reject ambiguous ownership rather than cancel a shared ancestor or escaping branch.
    for fork in w.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Fork)) {
        let starts: Vec<_> = w
            .edges
            .iter()
            .filter(|e| e.from == fork.id)
            .map(|e| e.to.clone())
            .collect();
        if starts.len() != incoming.len() {
            continue;
        }
        let mut groups = vec![];
        let mut valid = true;
        let mut used = BTreeSet::new();
        for start in starts {
            let mut pending = VecDeque::from([start]);
            let mut seen = BTreeSet::new();
            let mut joins = 0;
            while let Some(id) = pending.pop_front() {
                if id == join.id {
                    joins += 1;
                    continue;
                }
                if !seen.insert(id.clone()) {
                    continue;
                }
                let outgoing: Vec<_> = w.edges.iter().filter(|e| e.from == id).collect();
                if outgoing.is_empty() {
                    valid = false;
                    break;
                }
                pending.extend(outgoing.into_iter().map(|e| e.to.clone()));
            }
            if joins != 1
                || seen.is_empty()
                || seen.iter().any(|id| !used.insert(id.clone()))
                || incoming.iter().filter(|e| seen.contains(&e.from)).count() != 1
            {
                valid = false;
            }
            groups.push(seen);
        }
        // No branch may receive an edge from outside its owning fork/branch.
        for group in &groups {
            if w.edges
                .iter()
                .any(|e| group.contains(&e.to) && e.from != fork.id && !group.contains(&e.from))
            {
                valid = false;
            }
        }
        if valid {
            return Ok(groups);
        }
    }
    Err(Error::new(
        ErrorCode::UnsafeCancellation,
        format!(
            "{} needs a closed fork region with disjoint branches",
            join.id
        ),
    ))
}
