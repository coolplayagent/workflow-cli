use super::*;
use workflow_ir::Format;
use workflow_validator::{ValidationReport, validate_source};

struct Case {
    name: &'static str,
    source: String,
    format: Format,
    error: Option<&'static str>,
}
fn mutated(
    base: &Value,
    name: &'static str,
    code: &'static str,
    edit: impl FnOnce(&mut Value),
) -> Case {
    let mut v = base.clone();
    edit(&mut v);
    Case {
        name,
        source: v.to_string(),
        format: Format::Json,
        error: Some(code),
    }
}
fn cases() -> Vec<Case> {
    let base = json!({"schema_version":1,"id":"compile-fixture","version":"1.0.0","entry":"start","nodes":[
        {"id":"start","kind":{"type":"task","capability":{"id":"fixture.task","version":"1.0.0"},"policy":null}},
        {"id":"done","kind":{"type":"terminal","outcome":"succeeded"}}
    ],"edges":[{"id":"finish","from":"start","to":"done","route":{"type":"next"}}]});
    let workflow: workflow_ir::Workflow = serde_json::from_value(base.clone()).unwrap();
    vec![
        Case {
            name: "valid-json",
            source: base.to_string(),
            format: Format::Json,
            error: None,
        },
        Case {
            name: "valid-yaml",
            source: workflow.to_yaml().unwrap(),
            format: Format::Yaml,
            error: None,
        },
        Case {
            name: "bad-json",
            source: "{\"schema_version\":".into(),
            format: Format::Json,
            error: Some("parse_error"),
        },
        Case {
            name: "bad-yaml",
            source: "nodes: [".into(),
            format: Format::Yaml,
            error: Some("parse_error"),
        },
        Case {
            name: "trailing-document",
            source: format!("{}\n---\n{{}}", workflow.to_yaml().unwrap()),
            format: Format::Yaml,
            error: Some("parse_error"),
        },
        Case {
            name: "oversized",
            source: " ".repeat(workflow_ir::MAX_DOCUMENT_BYTES + 1),
            format: Format::Json,
            error: Some("document_too_large"),
        },
        mutated(&base, "unknown-field", "parse_error", |v| {
            v["unknown"] = json!(true)
        }),
        mutated(&base, "schema", "unsupported_schema", |v| {
            v["schema_version"] = json!(99)
        }),
        mutated(&base, "duplicate-node", "duplicate_node", |v| {
            let n = v["nodes"][0].clone();
            v["nodes"].as_array_mut().unwrap().push(n);
        }),
        mutated(&base, "duplicate-edge", "duplicate_edge", |v| {
            let e = v["edges"][0].clone();
            v["edges"].as_array_mut().unwrap().push(e);
        }),
        mutated(&base, "dangling", "dangling_edge", |v| {
            v["edges"][0]["to"] = json!("missing")
        }),
        mutated(&base, "cycle", "implicit_cycle", |v| {
            v["edges"][0]["to"] = json!("start")
        }),
        mutated(&base, "unreachable", "unreachable_node", |v| {
            let mut n = v["nodes"][1].clone();
            n["id"] = json!("orphan");
            v["nodes"].as_array_mut().unwrap().push(n);
        }),
        mutated(&base, "unbounded-loop", "unbounded_loop", |v| {
            v["nodes"][0]["kind"] = json!({"type":"loop","body":{"id":"repair","version":"1.0.0"},"max_iterations":0,"deadline_ms":0})
        }),
        mutated(&base, "input-type", "input_type", |v| {
            v["nodes"][0]["inputs"] =
                json!({"count":{"required":true,"value_type":{"type":"integer"}}});
            v["nodes"][0]["bindings"] = json!({"count":{"source":"literal","value":"three"}});
        }),
        mutated(&base, "capability-version", "unpinned_version", |v| {
            v["nodes"][0]["kind"]["capability"]["version"] = json!("latest")
        }),
        mutated(&base, "diagnostic-capacity", "diagnostic_limit", |v| {
            for i in 0..300 {
                v["nodes"].as_array_mut().unwrap().push(json!({"id":format!("orphan-{i}"),"kind":{"type":"terminal","outcome":"failed"}}));
            }
        }),
        mutated(&base, "graph-capacity", "graph_limit", |v| {
            for i in 0..4096 {
                v["nodes"].as_array_mut().unwrap().push(json!({"id":format!("orphan-{i}"),"kind":{"type":"terminal","outcome":"failed"}}));
            }
        }),
    ]
}
fn remote(
    client: &RemoteClient,
    source: &str,
    format: Format,
    file: &str,
) -> Result<ValidationReport> {
    match call(
        client,
        Operation::ValidateDefinition {
            source: source.into(),
            format,
            file: file.into(),
        },
    )? {
        Response::Validation(report) => Ok(report),
        _ => panic!("wrong validation response"),
    }
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn tls_definition_diagnostic_matrix_authorization_and_immutable_binding_contract() {
    let mut h = Harness::new(true);
    let tenant = format!(
        "validation-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let maintainer = credential(&mut service, &admin, "author", Role::DefinitionMaintainer);
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let (author_binding, author_client) = h.client(&maintainer);
    let (_, runner_client) = h.client(&runner);
    let started = Instant::now();
    let cases = cases();
    let invalid = cases.iter().filter(|c| c.error.is_some()).count();
    let mut valid_digest = None;
    for case in &cases {
        let file = format!("fixtures/{}.input", case.name);
        let expected = validate_source(&case.source, case.format, &file);
        if let Some(code) = case.error {
            assert!(!expected.valid, "{}", case.name);
            assert!(expected.digest.is_none());
            assert!(
                expected.diagnostics.iter().any(|d| d.code == code),
                "{}: {:?}",
                case.name,
                expected
            );
        } else {
            assert!(expected.valid, "{}", case.name);
            if let Some(digest) = &valid_digest {
                assert_eq!(expected.digest.as_ref(), Some(digest));
            }
            valid_digest = expected.digest.clone();
        }
        let received = remote(&author_client, &case.source, case.format, &file).unwrap();
        assert_eq!(received, expected, "{}", case.name);
        assert!(
            serde_json::to_vec(&received).unwrap().len() <= workflow_validator::MAX_REPORT_BYTES
        );
        assert!(received.diagnostics.iter().all(|d| d.file == file));
        if case.name == "dangling" {
            let d = received
                .diagnostics
                .iter()
                .find(|d| d.code == "dangling_edge")
                .unwrap();
            assert_eq!(d.edge.as_deref(), Some("finish"));
            assert_eq!(d.path, "edges[0].to");
        }
        if case.name == "input-type" {
            assert!(received.diagnostics.iter().any(|d| d.code == "input_type"
                && d.node.as_deref() == Some("start")
                && d.path.contains("bindings")));
        }
    }
    let label = "x".repeat(workflow_validator::MAX_DIAGNOSTIC_LABEL_BYTES + 1);
    assert_eq!(
        remote(&author_client, "{}", Format::Json, &label).unwrap(),
        validate_source("{}", Format::Json, &label)
    );
    // Repeated escaped labels fit the input bounds, but would expand the
    // diagnostic response beyond its encoded byte budget.
    let label = "\u{1}".repeat(workflow_validator::MAX_DIAGNOSTIC_LABEL_BYTES);
    let crowded = cases
        .iter()
        .find(|c| c.name == "diagnostic-capacity")
        .unwrap();
    let report = remote(&author_client, &crowded.source, crowded.format, &label).unwrap();
    assert_eq!(
        report,
        validate_source(&crowded.source, crowded.format, &label)
    );
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(report.diagnostics[0].code, "diagnostic_limit");
    // The wire envelope is independently bounded: JSON escaping can make an
    // admissible source exceed the HTTP request limit. Never report it valid
    // remotely when the request could not be delivered.
    let source = format!(
        "{}{}",
        cases[0].source,
        "\t".repeat(workflow_ir::MAX_DOCUMENT_BYTES - cases[0].source.len())
    );
    let wire_label = "w".repeat(workflow_validator::MAX_DIAGNOSTIC_LABEL_BYTES);
    assert!(validate_source(&source, Format::Json, &wire_label).valid);
    assert_eq!(
        remote(&author_client, &source, Format::Json, &wire_label)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    private(
        &h.dir.join("private-probe"),
        b"private-file-content-not-a-definition",
    );
    let label = h.dir.join("private-probe").display().to_string();
    let report = remote(&runner_client, &cases[0].source, Format::Json, &label).unwrap();
    assert!(
        report.valid,
        "diagnostic label must not be opened as a path"
    );
    for (index, role) in [
        Role::Administrator,
        Role::Viewer,
        Role::Approver,
        Role::Scheduler,
        Role::Worker,
        Role::Recovery,
    ]
    .into_iter()
    .enumerate()
    {
        let denied = credential(&mut service, &admin, &format!("denied-{index}"), role);
        let (_, client) = h.client(&denied);
        assert_eq!(
            remote(&client, &cases[0].source, Format::Json, "file.json")
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
    }
    let first = fixture("bound-v1");
    for case in cases.iter().filter(|c| {
        matches!(
            c.name,
            "unreachable" | "dangling" | "cycle" | "unbounded-loop" | "input-type"
        )
    }) {
        let mut invalid = first.clone();
        invalid.run_id = format!("invalid-{}", case.name);
        let workflow: workflow_ir::Workflow = serde_json::from_str(&case.source).unwrap();
        invalid.bundle.root = workflow_ir::VersionRef {
            id: workflow.id.clone(),
            version: workflow.version.clone(),
        };
        invalid.bundle.workflows = vec![workflow];
        assert_eq!(
            workflow_kernel::CompiledBundle::compile(invalid.bundle.clone())
                .unwrap_err()
                .code,
            workflow_kernel::ErrorCode::InvalidBundle
        );
        for operation in [
            Operation::Publish {
                bundle: Box::new(invalid.bundle.clone()),
            },
            Operation::Start {
                request: Box::new(invalid.clone()),
            },
        ] {
            let client = if matches!(operation, Operation::Publish { .. }) {
                &author_client
            } else {
                &runner_client
            };
            let error = call(client, operation).unwrap_err();
            assert_eq!(error.code, ErrorCode::TransitionRejected);
            assert_eq!(
                error.kernel_code, None,
                "RPC redacts internal error details"
            );
        }
        assert_eq!(
            call(
                &runner_client,
                Operation::Get {
                    run_id: invalid.run_id
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::NotFound
        );
    }
    // Validation has not granted publication, even for an authorized runner.
    assert_eq!(
        call(
            &runner_client,
            Operation::Start {
                request: Box::new(first.clone())
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Unauthorized
    );
    let audit = service.audit(admin.expose_secret(), 0, 100).unwrap();
    let validation: Vec<_> = audit
        .items
        .iter()
        .filter(|a| a.operation == "validate_definition")
        .collect();
    assert!(validation.iter().all(|a| a.resource == "definition"));
    assert!(
        !serde_json::to_string(&audit)
            .unwrap()
            .contains("private-file-content-not-a-definition")
    );
    call(
        &author_client,
        Operation::Publish {
            bundle: Box::new(first.bundle.clone()),
        },
    )
    .unwrap();

    let mut local =
        workflow_runstore_sqlite::SqliteRunStore::create(h.dir.join("local.db")).unwrap();
    local.start(&first).unwrap();
    for variant in ["capability", "workflow"] {
        let mut conflict = first.clone();
        conflict.run_id = format!("conflict-{variant}");
        if variant == "capability" {
            conflict.bundle.capabilities[0].usage.push_str(" changed");
        } else {
            let workflow = &mut conflict.bundle.workflows[0];
            let node = workflow
                .nodes
                .iter_mut()
                .find(|n| matches!(n.kind, workflow_ir::NodeKind::Wait { .. }))
                .unwrap();
            if let workflow_ir::NodeKind::Wait { timeout_ms, .. } = &mut node.kind {
                *timeout_ms += 1;
            }
        }
        // An earlier sorted insertion must also roll back on a later conflict.
        let mut extra = conflict.bundle.capabilities[0].clone();
        extra.capability.id = format!("aaa-uncommitted-{variant}");
        conflict.bundle.capabilities.push(extra);
        let remote_error = call(
            &author_client,
            Operation::Publish {
                bundle: Box::new(conflict.bundle.clone()),
            },
        )
        .unwrap_err();
        assert_eq!(
            call(
                &runner_client,
                Operation::Start {
                    request: Box::new(conflict.clone())
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::Unauthorized
        );
        assert_eq!(
            db().query_one(
                "SELECT count(*) FROM workflow_authority.bindings WHERE tenant=$1 AND id=$2",
                &[&tenant, &format!("aaa-uncommitted-{variant}")]
            )
            .unwrap()
            .get::<_, i64>(0),
            0
        );
        let local_error = local.start(&conflict).unwrap_err();
        assert_eq!(remote_error.code, ErrorCode::BindingConflict);
        assert_eq!(remote_error.code, local_error.code);
        assert_eq!(
            call(
                &runner_client,
                Operation::Get {
                    run_id: conflict.run_id.clone()
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::NotFound
        );
    }
    // Publishing identical content is idempotent; no run exists until Start.
    assert_eq!(
        db().query_one(
            "SELECT count(*) FROM workflow_authority.runs WHERE tenant=$1",
            &[&tenant]
        )
        .unwrap()
        .get::<_, i64>(0),
        0
    );
    call(
        &author_client,
        Operation::Publish {
            bundle: Box::new(first.bundle.clone()),
        },
    )
    .unwrap();
    call(
        &runner_client,
        Operation::Start {
            request: Box::new(first.clone()),
        },
    )
    .unwrap();
    // Independent HTTPS clients race to freeze a new version before any run.
    let mut races = vec![];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    for timeout_delta in [1, 2] {
        let mut bundle = first.bundle.clone();
        bundle.root.version = "publish-race".into();
        let root = bundle
            .workflows
            .iter_mut()
            .find(|w| w.id == bundle.root.id)
            .unwrap();
        root.version = bundle.root.version.clone();
        for node in &mut root.nodes {
            if let workflow_ir::NodeKind::Wait { timeout_ms, .. } = &mut node.kind {
                *timeout_ms += timeout_delta;
            }
        }
        let binding = author_binding.clone();
        let barrier = barrier.clone();
        races.push(std::thread::spawn(move || {
            let client = RemoteClient::new(binding).unwrap();
            barrier.wait();
            call(
                &client,
                Operation::Publish {
                    bundle: Box::new(bundle),
                },
            )
        }));
    }
    let races: Vec<_> = races.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(races.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        races.into_iter().find_map(Result::err).unwrap().code,
        ErrorCode::BindingConflict
    );
    let before = call(
        &runner_client,
        Operation::Get {
            run_id: first.run_id.clone(),
        },
    )
    .unwrap();
    service
        .revoke(admin.expose_secret(), &maintainer.id)
        .unwrap();
    assert_eq!(
        remote(&author_client, &cases[0].source, Format::Json, "file.json")
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let after = call(
        &runner_client,
        Operation::Get {
            run_id: first.run_id,
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    println!(
        "r01_https_validation_baseline: cases={} invalid={} invalid_detected={} false_accepts=0 parity_mismatches=0 elapsed_ms={} transport=https storage=postgres seed=deterministic repetitions=1",
        cases.len(),
        invalid,
        invalid,
        started.elapsed().as_millis()
    );
    unsafe { libc::kill(h.children[0].id() as i32, libc::SIGTERM) };
    h.wait_child(0);
}
