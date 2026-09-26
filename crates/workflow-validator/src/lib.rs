//! Deployment-independent static validation and decision semantics.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use workflow_ir::*;

pub use workflow_ir::Diagnostic;

const MAX_NODES: usize = 4096;
const MAX_EDGES: usize = 16384;

struct Checker<'a> {
    file: &'a str,
    diagnostics: Vec<Diagnostic>,
}
impl Checker<'_> {
    fn error(
        &mut self,
        code: &str,
        path: impl Into<String>,
        node: Option<&str>,
        edge: Option<&str>,
        message: impl Into<String>,
    ) {
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            file: self.file.into(),
            path: path.into(),
            node: node.map(str::to_owned),
            edge: edge.map(str::to_owned),
            message: message.into(),
        });
    }
    fn reference(&mut self, reference: &VersionRef, path: &str, node: &str) {
        if !identifier(&reference.id) {
            self.error(
                "invalid_reference",
                format!("{path}.id"),
                Some(node),
                None,
                "reference ID must be a stable identifier",
            );
        }
        if !pinned_version(&reference.version) {
            self.error(
                "unpinned_version",
                format!("{path}.version"),
                Some(node),
                None,
                "reference requires an immutable version, not a range, wildcard or latest",
            );
        }
    }
    fn contract(&mut self, contract: &Contract, path: &str, node: Option<&str>) {
        for name in contract.keys() {
            if !identifier(name) {
                self.error(
                    "invalid_field",
                    format!("{path}.{name}"),
                    node,
                    None,
                    "field name must be a stable identifier",
                );
            }
        }
    }
    fn condition(&mut self, condition: &Condition, fields: &Contract, path: &str, node: &str) {
        match condition {
            Condition::Exists { field }
            | Condition::Eq { field, .. }
            | Condition::NotEq { field, .. } => match fields.get(field) {
                None => self.error(
                    "unknown_condition_field",
                    format!("{path}.field"),
                    Some(node),
                    None,
                    format!("unknown field {field}"),
                ),
                Some(schema) => {
                    if let Condition::Eq { value, .. } | Condition::NotEq { value, .. } = condition
                        && !schema.value_type.accepts(value)
                    {
                        self.error(
                            "condition_type",
                            format!("{path}.value"),
                            Some(node),
                            None,
                            format!("literal does not match type of {field}"),
                        );
                    }
                }
            },
            Condition::All { conditions } | Condition::Any { conditions } => {
                if conditions.is_empty() {
                    self.error(
                        "empty_condition",
                        path,
                        Some(node),
                        None,
                        "all/any requires at least one condition",
                    );
                }
                for (i, c) in conditions.iter().enumerate() {
                    self.condition(c, fields, &format!("{path}.conditions[{i}]"), node);
                }
            }
            Condition::Not { condition } => {
                self.condition(condition, fields, &format!("{path}.condition"), node)
            }
        }
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}
fn pinned_version(value: &str) -> bool {
    identifier(value)
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "latest" | "head" | "main" | "master" | "stable" | "x"
        )
}

/// Static checking never resolves providers, touches a database, or trusts client claims.
pub fn validate(workflow: &Workflow, file: &str) -> Vec<Diagnostic> {
    let mut c = Checker {
        file,
        diagnostics: vec![],
    };
    if workflow.schema_version != SCHEMA_VERSION {
        c.error(
            "unsupported_schema",
            "schema_version",
            None,
            None,
            "only schema_version 1 is supported",
        );
    }
    if !identifier(&workflow.id) {
        c.error(
            "invalid_id",
            "id",
            None,
            None,
            "workflow ID must be a stable identifier",
        );
    }
    if !pinned_version(&workflow.version) {
        c.error(
            "unpinned_version",
            "version",
            None,
            None,
            "workflow version must be immutable",
        );
    }
    if workflow.nodes.len() > MAX_NODES || workflow.edges.len() > MAX_EDGES {
        c.error(
            "graph_limit",
            "$",
            None,
            None,
            "definition exceeds 4096 nodes or 16384 edges",
        );
        return c.diagnostics;
    }
    c.contract(&workflow.inputs, "inputs", None);
    let mut nodes = BTreeMap::new();
    for (i, n) in workflow.nodes.iter().enumerate() {
        if !identifier(&n.id) {
            c.error(
                "invalid_id",
                format!("nodes[{i}].id"),
                Some(&n.id),
                None,
                "node ID must be a stable identifier",
            );
        }
        if nodes.insert(n.id.as_str(), n).is_some() {
            c.error(
                "duplicate_node",
                format!("nodes[{i}].id"),
                Some(&n.id),
                None,
                "duplicate node ID",
            );
        }
    }
    if !nodes.contains_key(workflow.entry.as_str()) {
        c.error(
            "unknown_entry",
            "entry",
            None,
            None,
            "entry must reference a node",
        );
    }
    let mut edge_ids = BTreeSet::new();
    let mut outgoing: BTreeMap<&str, Vec<&Edge>> = nodes.keys().map(|id| (*id, vec![])).collect();
    let mut incoming = outgoing.clone();
    for (i, e) in workflow.edges.iter().enumerate() {
        if !identifier(&e.id) {
            c.error(
                "invalid_id",
                format!("edges[{i}].id"),
                None,
                Some(&e.id),
                "edge ID must be a stable identifier",
            );
        }
        if !edge_ids.insert(&e.id) {
            c.error(
                "duplicate_edge",
                format!("edges[{i}].id"),
                None,
                Some(&e.id),
                "duplicate edge ID",
            );
        }
        for (field, id) in [("from", &e.from), ("to", &e.to)] {
            if !nodes.contains_key(id.as_str()) {
                c.error(
                    "dangling_edge",
                    format!("edges[{i}].{field}"),
                    None,
                    Some(&e.id),
                    format!("unknown node {id}"),
                );
            }
        }
        if let (Some(node), Route::Case { when }) = (nodes.get(e.from.as_str()), &e.route) {
            let first = c.diagnostics.len();
            c.condition(
                when,
                &node.inputs,
                &format!("edges[{i}].route.when"),
                &node.id,
            );
            for diagnostic in &mut c.diagnostics[first..] {
                diagnostic.edge = Some(e.id.clone());
            }
        }
        if nodes.contains_key(e.from.as_str()) && nodes.contains_key(e.to.as_str()) {
            outgoing
                .get_mut(e.from.as_str())
                .expect("known source")
                .push(e);
            incoming
                .get_mut(e.to.as_str())
                .expect("known target")
                .push(e);
        }
    }
    // Kahn's algorithm rejects all implicit cycles, including disconnected cycles.
    let mut degrees: BTreeMap<_, _> = incoming.iter().map(|(id, es)| (*id, es.len())).collect();
    let mut queue: VecDeque<_> = degrees
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0;
    while let Some(id) = queue.pop_front() {
        visited += 1;
        for edge in &outgoing[id] {
            let degree = degrees.get_mut(edge.to.as_str()).expect("known target");
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(edge.to.as_str());
            }
        }
    }
    if visited != nodes.len() {
        c.error(
            "implicit_cycle",
            "edges",
            None,
            None,
            "cycles must use a bounded loop with a versioned body",
        );
    }
    let reachable = walk([workflow.entry.as_str()], &outgoing, false);
    let terminals: Vec<_> = nodes
        .iter()
        .filter(|(_, n)| matches!(n.kind, NodeKind::Terminal { .. }))
        .map(|(id, _)| *id)
        .collect();
    let terminating = walk(terminals, &incoming, true);
    for (i, n) in workflow.nodes.iter().enumerate() {
        let path = format!("nodes[{i}]");
        if !reachable.contains(n.id.as_str()) {
            c.error(
                "unreachable_node",
                &path,
                Some(&n.id),
                None,
                "node cannot be reached from entry",
            );
        }
        if !terminating.contains(n.id.as_str()) {
            c.error(
                "no_terminal_path",
                &path,
                Some(&n.id),
                None,
                "node has no path to a terminal",
            );
        }
        if n.id == workflow.entry && !incoming[n.id.as_str()].is_empty() {
            c.error(
                "entry_incoming",
                &path,
                Some(&n.id),
                None,
                "entry cannot have incoming edges",
            );
        }
        if incoming[n.id.as_str()].len() > 1
            && !matches!(n.kind, NodeKind::Join { .. } | NodeKind::Terminal { .. })
        {
            c.error(
                "implicit_join",
                &path,
                Some(&n.id),
                None,
                "multiple incoming edges require an explicit join or terminal",
            );
        }
        c.contract(&n.inputs, &format!("{path}.inputs"), Some(&n.id));
        c.contract(&n.outputs, &format!("{path}.outputs"), Some(&n.id));
        for (j, condition) in n.preconditions.iter().enumerate() {
            c.condition(
                condition,
                &n.inputs,
                &format!("{path}.preconditions[{j}]"),
                &n.id,
            );
        }
        check_kind(
            &mut c,
            n,
            &path,
            &outgoing[n.id.as_str()],
            incoming[n.id.as_str()].len(),
        );
        let ancestors = walk([n.id.as_str()], &incoming, true);
        for (name, field) in &n.inputs {
            if field.required && !n.bindings.contains_key(name) {
                c.error(
                    "missing_binding",
                    format!("{path}.bindings.{name}"),
                    Some(&n.id),
                    None,
                    "required input has no binding",
                );
            }
        }
        for (name, binding) in &n.bindings {
            let binding_path = format!("{path}.bindings.{name}");
            let Some(target) = n.inputs.get(name) else {
                c.error(
                    "unknown_input",
                    binding_path,
                    Some(&n.id),
                    None,
                    "binding has no declared input",
                );
                continue;
            };
            let source = match binding {
                Binding::Literal { value } => {
                    if !target.value_type.accepts(value) {
                        c.error(
                            "input_type",
                            &binding_path,
                            Some(&n.id),
                            None,
                            "literal does not match input type",
                        );
                    }
                    continue;
                }
                Binding::WorkflowInput { field } => workflow.inputs.get(field),
                Binding::NodeOutput { node, field } => {
                    if node == &n.id || !ancestors.contains(node.as_str()) {
                        c.error(
                            "input_order",
                            &binding_path,
                            Some(&n.id),
                            None,
                            "output source must precede this node in the graph",
                        );
                    }
                    nodes.get(node.as_str()).and_then(|n| n.outputs.get(field))
                }
            };
            match source {
                None => c.error(
                    "unknown_output",
                    &binding_path,
                    Some(&n.id),
                    None,
                    "binding references an unknown input or output field",
                ),
                Some(source) => {
                    if !target.value_type.assignable_from(&source.value_type) {
                        c.error(
                            "input_type",
                            &binding_path,
                            Some(&n.id),
                            None,
                            "source type is incompatible with input type",
                        );
                    }
                    if target.required && !source.required {
                        c.error(
                            "optional_source",
                            &binding_path,
                            Some(&n.id),
                            None,
                            "required input cannot bind an optional source",
                        );
                    }
                }
            }
        }
    }
    c.diagnostics
}

fn walk<'a>(
    starts: impl IntoIterator<Item = &'a str>,
    edges: &BTreeMap<&'a str, Vec<&'a Edge>>,
    reverse: bool,
) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<_> = starts.into_iter().collect();
    while let Some(id) = pending.pop() {
        if seen.insert(id)
            && let Some(edges) = edges.get(id)
        {
            for edge in edges {
                pending.push(if reverse { &edge.from } else { &edge.to });
            }
        }
    }
    seen
}

fn check_kind(c: &mut Checker<'_>, node: &Node, path: &str, edges: &[&Edge], incoming: usize) {
    let mut expected = vec![];
    match &node.kind {
        NodeKind::Task { capability, policy } => {
            c.reference(capability, &format!("{path}.kind.capability"), &node.id);
            if let Some(policy) = policy {
                c.reference(policy, &format!("{path}.kind.policy"), &node.id);
            }
            expected.push(Route::Next);
        }
        NodeKind::Subworkflow { workflow } => {
            c.reference(workflow, &format!("{path}.kind.workflow"), &node.id);
            expected.push(Route::Next);
        }
        NodeKind::Loop {
            body,
            max_iterations,
            deadline_ms,
        } => {
            c.reference(body, &format!("{path}.kind.body"), &node.id);
            if *max_iterations == 0 || *deadline_ms == 0 {
                c.error(
                    "unbounded_loop",
                    format!("{path}.kind"),
                    Some(&node.id),
                    None,
                    "loop requires positive max_iterations and deadline_ms",
                );
            }
            expected.extend([Route::Completed, Route::Exhausted]);
        }
        NodeKind::Wait { event, timeout_ms } => {
            if !identifier(event) || *timeout_ms == 0 {
                c.error(
                    "invalid_wait",
                    format!("{path}.kind"),
                    Some(&node.id),
                    None,
                    "wait requires an event identifier and positive timeout",
                );
            }
            expected.extend([Route::Accepted, Route::Rejected, Route::TimedOut]);
        }
        NodeKind::Join { mode, remaining } => {
            if incoming < 2 {
                c.error(
                    "join_arity",
                    path,
                    Some(&node.id),
                    None,
                    "join requires at least two incoming edges",
                );
            }
            if matches!(
                (mode, remaining),
                (JoinMode::All, RemainingPolicy::CancelAndReconcile)
            ) {
                c.error(
                    "join_policy",
                    format!("{path}.kind.remaining"),
                    Some(&node.id),
                    None,
                    "all join must await its branches",
                );
            }
            expected.push(Route::Next);
        }
        NodeKind::Terminal { .. } => {}
        NodeKind::Fork => {
            if edges.len() < 2 || edges.iter().any(|e| e.route != Route::Next) {
                c.error(
                    "fork_routes",
                    path,
                    Some(&node.id),
                    None,
                    "fork requires at least two next edges",
                );
            }
            let distinct: BTreeSet<_> = edges.iter().map(|e| &e.to).collect();
            if distinct.len() != edges.len() {
                c.error(
                    "duplicate_branch",
                    path,
                    Some(&node.id),
                    None,
                    "fork branches must have distinct destinations",
                );
            }
            return;
        }
        NodeKind::Decision { .. } => {
            let cases = edges
                .iter()
                .filter(|e| matches!(e.route, Route::Case { .. }))
                .count();
            let defaults = edges.iter().filter(|e| e.route == Route::Otherwise).count();
            if cases == 0 || defaults != 1 || cases + defaults != edges.len() {
                c.error(
                    "decision_routes",
                    path,
                    Some(&node.id),
                    None,
                    "decision requires case edges and exactly one otherwise edge",
                );
            }
            return;
        }
    }
    if expected.len() != edges.len()
        || expected
            .iter()
            .any(|r| edges.iter().filter(|e| &e.route == r).count() != 1)
    {
        c.error(
            "node_routes",
            path,
            Some(&node.id),
            None,
            format!("node requires exactly these routes: {expected:?}"),
        );
    }
}

mod decisions;
pub use decisions::{DecisionError, evaluate, select_branch, validate_values};
#[cfg(test)]
mod tests;
