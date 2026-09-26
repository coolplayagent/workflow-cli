use crate::{Result, check_draft, definition_digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use workflow_ir::Workflow;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub kind: ChangeKind,
    /// RFC 6901 pointer into the review representation, where nodes/edges are keyed by ID.
    pub path: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticDiff {
    pub before_digest: String,
    pub after_digest: String,
    pub changes: Vec<Change>,
}
pub fn semantic_diff(before: &Workflow, after: &Workflow) -> Result<SemanticDiff> {
    check_draft(before)?;
    check_draft(after)?;
    let mut changes = vec![];
    compare(
        "",
        Some(&review(before)),
        Some(&review(after)),
        &mut changes,
    );
    Ok(SemanticDiff {
        before_digest: definition_digest(before)?,
        after_digest: definition_digest(after)?,
        changes,
    })
}
fn review(w: &Workflow) -> Value {
    let nodes: serde_json::Map<String, Value> =
        w.nodes.iter().map(|n| (n.id.clone(), json!(n))).collect();
    let edges: serde_json::Map<String, Value> =
        w.edges.iter().map(|e| (e.id.clone(), json!(e))).collect();
    json!({"schema_version":w.schema_version,"id":w.id,"version":w.version,"entry":w.entry,"inputs":w.inputs,"nodes":nodes,"edges":edges,"edge_order":w.edges.iter().map(|e|&e.id).collect::<Vec<_>>()})
}
fn compare(path: &str, before: Option<&Value>, after: Option<&Value>, changes: &mut Vec<Change>) {
    if before == after {
        return;
    }
    if let (Some(Value::Object(a)), Some(Value::Object(b))) = (before, after) {
        let keys: BTreeSet<_> = a.keys().chain(b.keys()).collect();
        for key in keys {
            compare(
                &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                a.get(key),
                b.get(key),
                changes,
            );
        }
    } else {
        let kind = match (before, after) {
            (None, _) => ChangeKind::Added,
            (_, None) => ChangeKind::Removed,
            _ => ChangeKind::Modified,
        };
        changes.push(Change {
            kind,
            path: path.into(),
            before: before.cloned(),
            after: after.cloned(),
        });
    }
}
