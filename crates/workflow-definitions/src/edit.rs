use crate::{Error, ErrorCode, Result, check_draft, validate_revision};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use workflow_ir::{Contract, Edge, Node, Workflow};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub expected_revision: u64,
    pub operations: Vec<Edit>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Edit {
    SetVersion { version: String },
    SetEntry { entry: String },
    SetInputs { inputs: Contract },
    AddNode { node: Node },
    ReplaceNode { node: Node },
    RemoveNode { id: String },
    AddEdge { edge: Edge, before: Option<String> },
    ReplaceEdge { edge: Edge },
    RemoveEdge { id: String },
    OrderEdges { ids: Vec<String> },
}

/// Apply to a private copy; no partial edit can escape if any operation fails.
/// Storage adapters must compare expected_revision inside their write transaction.
pub fn apply_patch(workflow: &Workflow, patch: &Patch) -> Result<Workflow> {
    validate_revision(patch.expected_revision)?;
    if patch.operations.is_empty() || patch.operations.len() > 256 {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "patch requires 1..256 operations",
        ));
    }
    check_draft(workflow)?;
    let mut edited = workflow.clone();
    for (index, operation) in patch.operations.iter().enumerate() {
        apply(&mut edited, operation).map_err(|mut e| {
            e.message = format!("operations[{index}]: {}", e.message);
            e
        })?;
    }
    check_draft(&edited)?;
    Ok(edited)
}
fn unknown(kind: &str, id: &str) -> Error {
    Error::new(ErrorCode::NotFound, format!("{kind} {id} does not exist"))
}
fn exists(kind: &str, id: &str) -> Error {
    Error::new(
        ErrorCode::AlreadyExists,
        format!("{kind} {id} already exists"),
    )
}
fn apply(w: &mut Workflow, edit: &Edit) -> Result<()> {
    match edit {
        Edit::SetVersion { version } => w.version = version.clone(),
        Edit::SetEntry { entry } => w.entry = entry.clone(),
        Edit::SetInputs { inputs } => w.inputs = inputs.clone(),
        Edit::AddNode { node } => {
            if w.nodes.iter().any(|n| n.id == node.id) {
                return Err(exists("node", &node.id));
            }
            w.nodes.push(node.clone());
        }
        Edit::ReplaceNode { node } => {
            let current = w
                .nodes
                .iter_mut()
                .find(|n| n.id == node.id)
                .ok_or_else(|| unknown("node", &node.id))?;
            *current = node.clone();
        }
        Edit::RemoveNode { id } => {
            let i = w
                .nodes
                .iter()
                .position(|n| &n.id == id)
                .ok_or_else(|| unknown("node", id))?;
            w.nodes.remove(i);
        }
        Edit::AddEdge { edge, before } => {
            if w.edges.iter().any(|e| e.id == edge.id) {
                return Err(exists("edge", &edge.id));
            }
            let i = match before {
                Some(id) => w
                    .edges
                    .iter()
                    .position(|e| &e.id == id)
                    .ok_or_else(|| unknown("edge", id))?,
                None => w.edges.len(),
            };
            w.edges.insert(i, edge.clone());
        }
        Edit::ReplaceEdge { edge } => {
            let current = w
                .edges
                .iter_mut()
                .find(|e| e.id == edge.id)
                .ok_or_else(|| unknown("edge", &edge.id))?;
            *current = edge.clone();
        }
        Edit::RemoveEdge { id } => {
            let i = w
                .edges
                .iter()
                .position(|e| &e.id == id)
                .ok_or_else(|| unknown("edge", id))?;
            w.edges.remove(i);
        }
        Edit::OrderEdges { ids } => {
            let actual: BTreeSet<_> = w.edges.iter().map(|e| &e.id).collect();
            let desired: BTreeSet<_> = ids.iter().collect();
            if actual != desired || ids.len() != w.edges.len() {
                return Err(Error::new(
                    ErrorCode::InvalidRequest,
                    "edge order must name each existing edge exactly once",
                ));
            }
            let by_id: std::collections::BTreeMap<_, _> =
                w.edges.drain(..).map(|e| (e.id.clone(), e)).collect();
            w.edges = ids.iter().map(|id| by_id[id].clone()).collect();
        }
    }
    Ok(())
}
