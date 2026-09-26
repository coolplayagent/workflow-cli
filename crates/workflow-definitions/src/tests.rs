use super::*;
use std::collections::BTreeMap;
use workflow_ir::*;

fn sample() -> Workflow {
    WorkflowBuilder::new("review", "1", "done")
        .node(Node {
            id: "done".into(),
            kind: NodeKind::Terminal {
                outcome: TerminalOutcome::Succeeded,
            },
            inputs: Contract::new(),
            outputs: Contract::new(),
            bindings: BTreeMap::new(),
            preconditions: vec![],
        })
        .build()
}
fn patch(operations: Vec<Edit>) -> Patch {
    Patch {
        expected_revision: 1,
        operations,
    }
}

#[test]
fn batch_edits_are_atomic_and_fail_with_the_operation_index() {
    let before = sample();
    let error = apply_patch(
        &before,
        &patch(vec![
            Edit::SetVersion {
                version: "2".into(),
            },
            Edit::RemoveNode {
                id: "missing".into(),
            },
        ]),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    assert!(error.message.starts_with("operations[1]"));
    assert_eq!(before.version, "1");
}
#[test]
fn draft_can_be_incomplete_but_publication_cannot() {
    let draft = apply_patch(
        &sample(),
        &patch(vec![Edit::RemoveNode { id: "done".into() }]),
    )
    .unwrap();
    assert!(!check_draft(&draft).unwrap().is_empty());
    assert_eq!(
        check_publish(&draft).unwrap_err().code,
        ErrorCode::InvalidDefinition
    );
    let mut duplicate = sample();
    duplicate.nodes.push(duplicate.nodes[0].clone());
    assert_eq!(
        check_draft(&duplicate).unwrap_err().code,
        ErrorCode::InvalidDefinition
    );
}
#[test]
fn node_and_edge_crud_preserve_ids_and_explicit_order() {
    let mut start = sample().nodes[0].clone();
    start.id = "start".into();
    start.kind = NodeKind::Fork;
    let mut other = sample().nodes[0].clone();
    other.id = "other".into();
    let a = Edge {
        id: "a".into(),
        from: "start".into(),
        to: "done".into(),
        route: Route::Next,
    };
    let b = Edge {
        id: "b".into(),
        from: "start".into(),
        to: "other".into(),
        route: Route::Next,
    };
    let w = apply_patch(
        &sample(),
        &patch(vec![
            Edit::AddNode {
                node: start.clone(),
            },
            Edit::AddNode { node: other },
            Edit::SetEntry {
                entry: "start".into(),
            },
            Edit::AddEdge {
                edge: a.clone(),
                before: None,
            },
            Edit::AddEdge {
                edge: b,
                before: Some("a".into()),
            },
        ]),
    )
    .unwrap();
    check_publish(&w).unwrap();
    assert_eq!(w.edges[0].id, "b");
    let w = apply_patch(
        &w,
        &patch(vec![
            Edit::OrderEdges {
                ids: vec!["a".into(), "b".into()],
            },
            Edit::ReplaceEdge { edge: a },
            Edit::ReplaceNode { node: start },
        ]),
    )
    .unwrap();
    assert_eq!(w.edges[0].id, "a");
    let w = apply_patch(
        &w,
        &patch(vec![
            Edit::RemoveEdge { id: "a".into() },
            Edit::RemoveNode { id: "done".into() },
        ]),
    )
    .unwrap();
    assert_eq!(w.edges.len(), 1);
    assert_eq!(w.nodes.len(), 2);
    assert_eq!(
        apply_patch(
            &w,
            &patch(vec![Edit::OrderEdges {
                ids: vec!["b".into(), "b".into()]
            }])
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );
}
#[test]
fn diff_ignores_node_order_and_locates_precise_field_changes() {
    let mut a = sample();
    let mut other = a.nodes[0].clone();
    other.id = "other".into();
    a.nodes.push(other);
    let mut b = a.clone();
    b.nodes.reverse();
    assert!(semantic_diff(&a, &b).unwrap().changes.is_empty());
    b.nodes[1].kind = NodeKind::Terminal {
        outcome: TerminalOutcome::Failed,
    };
    let diff = semantic_diff(&a, &b).unwrap();
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "/nodes/done/kind/outcome");
    assert_ne!(diff.before_digest, diff.after_digest);
}
#[test]
fn diff_detects_priority_changes_and_escapes_pointer_tokens() {
    let mut a = sample();
    a.edges = vec![
        Edge {
            id: "a".into(),
            from: "done".into(),
            to: "done".into(),
            route: Route::Next,
        },
        Edge {
            id: "b".into(),
            from: "done".into(),
            to: "done".into(),
            route: Route::Next,
        },
    ];
    let mut b = a.clone();
    b.edges.reverse();
    let diff = semantic_diff(&a, &b).unwrap();
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "/edge_order");
    a.nodes[0].bindings.insert(
        "nested".into(),
        Binding::Literal {
            value: serde_json::json!({"a/b~c":1}),
        },
    );
    b = a.clone();
    b.nodes[0].bindings.insert(
        "nested".into(),
        Binding::Literal {
            value: serde_json::json!({"a/b~c":2}),
        },
    );
    assert_eq!(
        semantic_diff(&a, &b).unwrap().changes[0].path,
        "/nodes/done/bindings/nested/value/a~1b~0c"
    );
}
#[test]
fn patch_wire_format_rejects_unknown_fields_and_empty_or_unbounded_batches() {
    assert!(
        serde_json::from_str::<Patch>(r#"{"expected_revision":1,"operations":[],"force":true}"#)
            .is_err()
    );
    assert!(serde_json::from_str::<Patch>(r#"{"expected_revision":1,"operations":[{"op":"set_version","version":"2","force":true}]}"#).is_err());
    assert_eq!(
        apply_patch(&sample(), &patch(vec![])).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        apply_patch(
            &sample(),
            &patch(vec![
                Edit::SetVersion {
                    version: "2".into()
                };
                257
            ])
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn drafts_must_round_trip_through_the_bounded_storage_decoder() {
    let mut w = sample();
    let mut literal = serde_json::json!(true);
    for _ in 0..150 {
        literal = serde_json::json!([literal]);
    }
    w.nodes[0]
        .bindings
        .insert("deep".into(), Binding::Literal { value: literal });
    let error = check_draft(&w).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidDefinition);
    assert_eq!(error.diagnostics[0].code, "parse_error");
}
