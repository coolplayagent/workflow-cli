use super::*;
use serde_json::json;

fn terminal(id: &str) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Terminal {
            outcome: TerminalOutcome::Succeeded,
        },
        inputs: Contract::new(),
        outputs: Contract::new(),
        bindings: BTreeMap::new(),
        preconditions: vec![],
    }
}
fn base() -> Workflow {
    let mut start = terminal("start");
    start.kind = NodeKind::Task {
        capability: VersionRef {
            id: "test".into(),
            version: "1".into(),
        },
        policy: None,
    };
    WorkflowBuilder::new("build", "1", "start")
        .node(start)
        .node(terminal("done"))
        .edge(Edge {
            id: "finish".into(),
            from: "start".into(),
            to: "done".into(),
            route: Route::Next,
        })
        .build()
}
fn codes(w: &Workflow) -> Vec<String> {
    validate(w, "case.json")
        .into_iter()
        .map(|d| d.code)
        .collect()
}
fn expect(w: &Workflow, code: &str) {
    assert!(
        codes(w).iter().any(|c| c == code),
        "expected {code}, got {:?}",
        validate(w, "case.json")
    );
}

#[test]
fn rejects_duplicate_ids_dangling_edges_cycles_and_unreachable_nodes() {
    assert!(codes(&base()).is_empty());
    let mut w = base();
    w.nodes.push(w.nodes[0].clone());
    expect(&w, "duplicate_node");
    let mut w = base();
    w.edges.push(w.edges[0].clone());
    expect(&w, "duplicate_edge");
    let mut w = base();
    w.edges[0].to = "missing".into();
    expect(&w, "dangling_edge");
    expect(&w, "no_terminal_path");
    let mut w = base();
    w.edges[0].to = "start".into();
    expect(&w, "implicit_cycle");
    expect(&w, "unreachable_node");
    let mut w = base();
    w.nodes.push(terminal("orphan"));
    expect(&w, "unreachable_node");
}

#[test]
fn rejects_missing_wrong_typed_optional_and_future_bindings() {
    let mut w = base();
    w.nodes[0].inputs.insert(
        "count".into(),
        Field {
            value_type: ValueType::Integer,
            required: true,
        },
    );
    expect(&w, "missing_binding");
    w.nodes[0].bindings.insert(
        "count".into(),
        Binding::Literal {
            value: json!("three"),
        },
    );
    expect(&w, "input_type");
    w.nodes[0]
        .bindings
        .insert("count".into(), Binding::Literal { value: json!(3) });
    assert!(codes(&w).is_empty());
    w.inputs.insert(
        "count".into(),
        Field {
            value_type: ValueType::Integer,
            required: false,
        },
    );
    w.nodes[0].bindings.insert(
        "count".into(),
        Binding::WorkflowInput {
            field: "count".into(),
        },
    );
    expect(&w, "optional_source");
    w.nodes[0].bindings.insert(
        "count".into(),
        Binding::NodeOutput {
            node: "done".into(),
            field: "count".into(),
        },
    );
    expect(&w, "input_order");
    expect(&w, "unknown_output");
}

#[test]
fn bounded_loops_require_both_limits_and_an_exhaustion_exit() {
    let mut w = base();
    w.nodes[0].kind = NodeKind::Loop {
        feedback: Default::default(),
        body: VersionRef {
            id: "repair".into(),
            version: "1".into(),
        },
        max_iterations: 0,
        deadline_ms: 0,
    };
    expect(&w, "unbounded_loop");
    expect(&w, "node_routes");
    w.nodes[0].kind = NodeKind::Loop {
        feedback: Default::default(),
        body: VersionRef {
            id: "repair".into(),
            version: "1".into(),
        },
        max_iterations: 2,
        deadline_ms: 60000,
    };
    w.edges[0].route = Route::Completed;
    let mut exhausted = w.edges[0].clone();
    exhausted.id = "exhausted".into();
    exhausted.route = Route::Exhausted;
    w.edges.push(exhausted);
    assert!(codes(&w).is_empty());
}

#[test]
fn joins_and_versions_are_explicit() {
    let mut w = base();
    w.version = "latest".into();
    expect(&w, "unpinned_version");
    let mut w = base();
    w.nodes[0].kind = NodeKind::Join {
        mode: JoinMode::All,
        remaining: RemainingPolicy::CancelAndReconcile,
    };
    expect(&w, "join_policy");
    expect(&w, "join_arity");
    let mut w = base();
    w.nodes[0].kind = NodeKind::Task {
        capability: VersionRef {
            id: "test".into(),
            version: "^1.0".into(),
        },
        policy: None,
    };
    expect(&w, "unpinned_version");
}

fn decision() -> (Node, Vec<Edge>) {
    let mut n = terminal("choose");
    n.kind = NodeKind::Decision {
        mode: DecisionMode::Exclusive,
    };
    n.inputs.insert(
        "pass".into(),
        Field {
            value_type: ValueType::Boolean,
            required: true,
        },
    );
    let routes = vec![
        Route::Case {
            when: Condition::Eq {
                field: "pass".into(),
                value: json!(true),
            },
        },
        Route::Otherwise,
    ];
    (
        n,
        routes
            .into_iter()
            .enumerate()
            .map(|(i, route)| Edge {
                id: format!("edge{i}"),
                from: "choose".into(),
                to: format!("dest{i}"),
                route,
            })
            .collect(),
    )
}

#[test]
fn decisions_reject_bad_values_and_define_no_match_multiple_matches_and_first_match() {
    let (mut n, mut edges) = decision();
    let values = BTreeMap::from([("pass".into(), json!(true))]);
    assert_eq!(
        select_branch(&n, &edges.iter().collect::<Vec<_>>(), &values)
            .unwrap()
            .id,
        "edge0"
    );
    assert_eq!(
        select_branch(
            &n,
            &edges.iter().collect::<Vec<_>>(),
            &BTreeMap::from([("pass".into(), json!(false))])
        )
        .unwrap()
        .id,
        "edge1"
    );
    assert_eq!(
        select_branch(&n, &edges.iter().collect::<Vec<_>>(), &BTreeMap::new())
            .unwrap_err()
            .code,
        "missing_value"
    );
    assert_eq!(
        select_branch(
            &n,
            &edges.iter().collect::<Vec<_>>(),
            &BTreeMap::from([("pass".into(), json!("true"))])
        )
        .unwrap_err()
        .code,
        "value_type"
    );
    let mut duplicate = edges[0].clone();
    duplicate.id = "duplicate".into();
    edges.push(duplicate);
    assert_eq!(
        select_branch(&n, &edges.iter().collect::<Vec<_>>(), &values)
            .unwrap_err()
            .code,
        "multiple_matches"
    );
    n.kind = NodeKind::Decision {
        mode: DecisionMode::FirstMatch,
    };
    assert_eq!(
        select_branch(&n, &edges.iter().collect::<Vec<_>>(), &values)
            .unwrap()
            .id,
        "edge0"
    );
}

#[test]
fn optional_conditions_require_guards_and_use_ordered_short_circuit() {
    let compare = Condition::Eq {
        field: "x".into(),
        value: json!(true),
    };
    assert_eq!(
        evaluate(&compare, &BTreeMap::new()).unwrap_err().code,
        "missing_condition_value"
    );
    let guarded = Condition::All {
        conditions: vec![Condition::Exists { field: "x".into() }, compare],
    };
    assert!(!evaluate(&guarded, &BTreeMap::new()).unwrap());
}

#[test]
fn diagnostics_are_identical_across_deployment_callers() {
    let mut w = base();
    w.edges[0].to = "missing".into();
    let local = validate(&w, "request.yaml");
    let wire = serde_json::to_string(&w).unwrap();
    let remote = validate(
        &workflow_ir::parse(&wire, Format::Json, "request.yaml").unwrap(),
        "request.yaml",
    );
    assert_eq!(local, remote);
    let dangling = local.iter().find(|d| d.code == "dangling_edge").unwrap();
    assert_eq!(dangling.edge.as_deref(), Some("finish"));
    assert_eq!(dangling.path, "edges[0].to");
}

#[test]
fn condition_diagnostic_has_exact_edge_field_and_stable_identity() {
    let mut w = base();
    w.nodes[0].kind = NodeKind::Decision {
        mode: DecisionMode::Exclusive,
    };
    w.edges[0].route = Route::Case {
        when: Condition::Eq {
            field: "missing".into(),
            value: json!(true),
        },
    };
    let d = validate(&w, "workflow.json")
        .into_iter()
        .find(|d| d.code == "unknown_condition_field")
        .unwrap();
    assert_eq!(d.path, "edges[0].route.when.field");
    assert_eq!(d.edge.as_deref(), Some("finish"));
    assert_eq!(d.node.as_deref(), Some("start"));
}

#[test]
fn disconnected_cycles_and_implicit_merges_are_not_hidden_by_a_valid_entry() {
    let mut w = base();
    let mut a = w.nodes[0].clone();
    a.id = "a".into();
    let mut b = a.clone();
    b.id = "b".into();
    w.nodes.extend([a, b]);
    w.edges.extend([
        Edge {
            id: "ab".into(),
            from: "a".into(),
            to: "b".into(),
            route: Route::Next,
        },
        Edge {
            id: "ba".into(),
            from: "b".into(),
            to: "a".into(),
            route: Route::Next,
        },
    ]);
    expect(&w, "implicit_cycle");
    expect(&w, "unreachable_node");
    let mut w = base();
    let mut e = w.edges[0].clone();
    e.id = "second".into();
    e.to = "start".into();
    w.edges.push(e);
    expect(&w, "entry_incoming");
}

#[test]
fn invalid_conditions_do_not_hide_behind_a_matching_first_case() {
    let (mut n, mut edges) = decision();
    n.kind = NodeKind::Decision {
        mode: DecisionMode::FirstMatch,
    };
    edges.push(Edge {
        id: "bad".into(),
        from: n.id.clone(),
        to: "other".into(),
        route: Route::Case {
            when: Condition::Eq {
                field: "missing".into(),
                value: json!(1),
            },
        },
    });
    assert_eq!(
        select_branch(
            &n,
            &edges.iter().collect::<Vec<_>>(),
            &BTreeMap::from([("pass".into(), json!(true))])
        )
        .unwrap_err()
        .code,
        "missing_condition_value"
    );
}

#[test]
fn compiler_report_bounds_diagnostics_without_accepting_truncated_failures() {
    let mut w = base();
    for i in 0..300 {
        w.nodes.push(terminal(&format!("orphan-{i}")));
    }
    let report = ValidationReport::check(&w, "many-errors.json");
    assert!(!report.valid);
    assert!(report.digest.is_none());
    assert_eq!(report.diagnostics.len(), MAX_DIAGNOSTICS);
    assert_eq!(report.diagnostics.last().unwrap().code, "diagnostic_limit");
    assert!(serde_json::to_vec(&report).unwrap().len() <= MAX_REPORT_BYTES);
    // Escaping must count against the encoded report bound, not just text length.
    let report = ValidationReport::rejected(Diagnostic {
        code: "parse_error".into(),
        file: "escaped.json".into(),
        path: "$".into(),
        node: None,
        edge: None,
        message: "\u{1}".repeat(MAX_REPORT_BYTES / 2),
    });
    assert!(!report.valid);
    assert_eq!(report.diagnostics[0].code, "diagnostic_limit");
    assert!(serde_json::to_vec(&report).unwrap().len() < 1024);
    let report = validate_source(
        "{}",
        Format::Json,
        &"x".repeat(MAX_DIAGNOSTIC_LABEL_BYTES + 1),
    );
    assert_eq!(report.diagnostics[0].code, "diagnostic_label_limit");
    assert!(serde_json::to_vec(&report).unwrap().len() < 1024);
}
