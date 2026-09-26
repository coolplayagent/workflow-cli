use super::*;
use serde_json::json;

fn example() -> Workflow {
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

#[test]
fn json_yaml_and_builder_have_the_same_semantic_digest() {
    let workflow = example();
    let json = workflow.canonical_json().unwrap();
    let yaml = workflow.to_yaml().unwrap();
    assert_eq!(parse(&json, Format::Json, "a.json").unwrap(), workflow);
    assert_eq!(parse(&yaml, Format::Yaml, "a.yaml").unwrap(), workflow);
    assert_eq!(
        parse(&yaml, Format::Yaml, "a.yaml")
            .unwrap()
            .digest()
            .unwrap(),
        workflow.digest().unwrap()
    );
}

#[test]
fn imports_fail_on_unknown_fields_duplicate_fields_and_trailing_documents() {
    let json = example().canonical_json().unwrap();
    for invalid in [
        format!("{json} {{}}"),
        json.replacen('{', "{\"surprise\":true,", 1),
        json.replacen('{', "{\"id\":\"duplicate\",", 1),
    ] {
        assert_eq!(
            parse(&invalid, Format::Json, "bad").unwrap_err().code,
            "parse_error"
        );
    }
    assert!(
        parse(
            &(example().to_yaml().unwrap() + "\n---\n{}"),
            Format::Yaml,
            "bad.yaml"
        )
        .is_err()
    );
}

#[test]
fn nested_parse_diagnostics_retain_file_and_field_path() {
    let json = example()
        .canonical_json()
        .unwrap()
        .replace("\"succeeded\"", "17");
    let diagnostic = parse(&json, Format::Json, "bad.json").unwrap_err();
    assert_eq!(diagnostic.file, "bad.json");
    assert!(diagnostic.path.contains("nodes[0].kind"));
}

#[test]
fn object_contracts_are_closed_and_types_are_checked_recursively() {
    let schema = ValueType::Object {
        fields: BTreeMap::from([("count".into(), ValueType::Integer)]),
    };
    assert!(schema.accepts(&json!({"count": 2})));
    for value in [
        json!({}),
        json!({"count": 2.5}),
        json!({"count": 2, "extra": true}),
        Value::Null,
    ] {
        assert!(!schema.accepts(&value));
    }
    assert!(ValueType::Number.assignable_from(&ValueType::Integer));
    assert!(!ValueType::Integer.assignable_from(&ValueType::Number));
}

#[test]
fn content_digest_ignores_node_order_but_preserves_behavioral_edge_order() {
    let mut w = example();
    let mut other = w.nodes[0].clone();
    other.id = "other".into();
    w.nodes.push(other);
    let digest = w.digest().unwrap();
    w.nodes.reverse();
    assert_eq!(digest, w.digest().unwrap());
    w.edges = vec![
        Edge {
            id: "a".into(),
            from: "done".into(),
            to: "other".into(),
            route: Route::Next,
        },
        Edge {
            id: "b".into(),
            from: "other".into(),
            to: "done".into(),
            route: Route::Next,
        },
    ];
    let digest = w.digest().unwrap();
    w.edges.reverse();
    assert_ne!(digest, w.digest().unwrap());
}
