use super::*;
use workflow_runstore_postgres::access::{
    ArtifactOutputPolicy, ArtifactPolicy, ArtifactUploadRequest,
};
use workflow_worker::{AdapterOutcome, CapabilityAdapter, CapabilityDescriptor, Invocation};

struct Report(CapabilityDescriptor);
impl CapabilityAdapter for Report {
    fn descriptor(&self) -> CapabilityDescriptor {
        self.0.clone()
    }
    fn invoke(&self, request: Invocation<'_>) -> AdapterOutcome {
        assert_eq!(request.request.inputs["revision"], json!("a".repeat(40)));
        AdapterOutcome::Succeeded {
            outputs: [("ok".into(), json!(true))].into(),
            evidence: vec![],
        }
    }
}
fn start() -> StartRun {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    serde_json::from_slice(
        &std::fs::read(root.join("examples/artifacts/shared-report-start.json")).unwrap(),
    )
    .unwrap()
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn tls_artifact_transfer_resume_binding_and_result_recovery_contract() {
    let mut h = Harness::new(true);
    let tenant = format!(
        "artifact-tls-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let maintainer = credential(
        &mut service,
        &admin,
        "maintainer",
        Role::DefinitionMaintainer,
    );
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let viewer = credential(&mut service, &admin, "viewer", Role::Viewer);
    let scheduler = credential(&mut service, &admin, "scheduler", Role::Scheduler);
    let start = start();
    let cap = workflow_worker::Capability::new(start.bundle.capabilities[0].clone()).unwrap();
    let ty: workflow_artifacts::ArtifactType = serde_json::from_value(
        json!({"identity":{"id":"shared-report","version":"1.0.0"},"content":{"format":"utf8"}}),
    )
    .unwrap();
    let worker = service
        .issue(
            admin.expose_secret(),
            "producer",
            Role::Worker,
            &[CapabilityRule {
                id: "fixture.report".into(),
                version: "1.0.0".into(),
                contract_digest: cap.digest().into(),
                artifacts: Some(ArtifactPolicy {
                    inputs: Default::default(),
                    output: Some(ArtifactOutputPolicy {
                        types: vec![ty.clone()],
                        repository_input: "repository".into(),
                        revision_input: "revision".into(),
                    }),
                }),
            }],
            3600000,
        )
        .unwrap();
    let (_, maintainer_client) = h.client(&maintainer);
    let (_, runner_client) = h.client(&runner);
    let (_, viewer_client) = h.client(&viewer);
    let (_, scheduler_client) = h.client(&scheduler);
    let (worker_binding, worker_client) = h.client(&worker);
    call(
        &maintainer_client,
        Operation::Publish {
            bundle: Box::new(start.bundle.clone()),
        },
    )
    .unwrap();
    call(
        &runner_client,
        Operation::Start {
            request: Box::new(start.clone()),
        },
    )
    .unwrap();
    let Response::Lease(lease) = call(
        &scheduler_client,
        Operation::Acquire {
            run_id: start.run_id.clone(),
            acquisition_id: "report-lease".into(),
            ttl_ms: 60000,
        },
    )
    .unwrap() else {
        panic!("lease")
    };
    let Response::Dispatch(workflow_runstore_postgres::access::Dispatch::Task { assignment_id }) =
        call(
            &scheduler_client,
            Operation::Dispatch {
                lease: lease.clone(),
                worker_id: worker.id.clone(),
            },
        )
        .unwrap()
    else {
        panic!("assignment")
    };
    let content = vec![b'r'; 131101];
    let upload = ArtifactUploadRequest {
        request_id: "tls-report".into(),
        assignment_id: assignment_id.clone(),
        artifact_type: ty,
        bytes: content.len() as u64,
        content_digest: workflow_artifacts::content_digest(&content),
    };
    // Kill a separate upload process after its first acknowledged chunk. A
    // fresh invocation must discover durable progress and complete exact bytes.
    std::fs::write(h.dir.join("content"), &content).unwrap();
    let uploader = h.spawn(json!({"kind":"partial_uploader","binding":worker_binding,"request":upload,"content":h.dir.join("content"),"ready":h.dir.join("upload.ready")}));
    h.wait_file("upload.ready");
    h.children[uploader].kill().unwrap();
    assert!(!h.children[uploader].wait().unwrap().success());
    let artifact = worker_client.upload_artifact(&upload, &content).unwrap();
    assert_eq!(
        worker_client.upload_artifact(&upload, &content).unwrap(),
        artifact
    );
    let (downloaded, bytes) = viewer_client
        .download_artifact(&artifact.link(), None, 60000)
        .unwrap();
    assert_eq!(downloaded, artifact);
    assert_eq!(bytes, content);
    let foreign_admin = AuthenticatedService::bootstrap(
        &mut db(),
        &format!("{tenant}-foreign"),
        "project",
        "admin",
        3600000,
    )
    .unwrap();
    let foreign_viewer = credential(&mut service, &foreign_admin, "viewer", Role::Viewer);
    let (_, foreign_client) = h.client(&foreign_viewer);
    assert_eq!(
        foreign_client
            .download_artifact(&artifact.link(), None, 1000)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let Response::ArtifactGrant(grant) = call(
        &viewer_client,
        Operation::ArtifactGrant {
            artifact: artifact.link(),
            assignment_id: None,
            ttl_ms: 60000,
        },
    )
    .unwrap() else {
        panic!("grant")
    };
    assert_eq!(
        call(
            &foreign_client,
            Operation::ArtifactGet {
                download_id: grant.download_id,
                offset: 0
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Unauthorized
    );
    let Response::Assignment(task) = call(
        &worker_client,
        Operation::Assignment {
            assignment_id: assignment_id.clone(),
        },
    )
    .unwrap() else {
        panic!("task")
    };
    let mut executor = workflow_worker::Worker::default();
    executor.register(Report(cap.descriptor().clone())).unwrap();
    let mut result = executor
        .execute(&task.request, &task.grant)
        .unwrap()
        .into_result();
    if let AdapterOutcome::Succeeded { evidence, .. } = &mut result.outcome {
        evidence.push(workflow_worker::EvidenceRef {
            artifact_id: artifact.artifact_id.clone(),
            digest: artifact.digest.clone(),
        });
    }
    call(
        &worker_client,
        Operation::Finish {
            assignment_id,
            result: Box::new(result),
        },
    )
    .unwrap();
    for _ in 0..10 {
        let Response::Snapshot(s) = call(
            &viewer_client,
            Operation::Get {
                run_id: start.run_id.clone(),
            },
        )
        .unwrap() else {
            panic!("snapshot")
        };
        if s.status == RunStatus::Succeeded {
            break;
        }
        call(
            &scheduler_client,
            Operation::Dispatch {
                lease: lease.clone(),
                worker_id: worker.id.clone(),
            },
        )
        .unwrap();
    }
    let Response::Snapshot(expected) = call(
        &viewer_client,
        Operation::Get {
            run_id: start.run_id.clone(),
        },
    )
    .unwrap() else {
        panic!("snapshot")
    };
    assert_eq!(expected.status, RunStatus::Succeeded);
    // A different server instance reads the same persisted evidence and state.
    let other = Harness::new(true);
    let (_, other_viewer) = other.client(&viewer);
    let Response::Snapshot(recovered) = call(
        &other_viewer,
        Operation::Get {
            run_id: start.run_id.clone(),
        },
    )
    .unwrap() else {
        panic!("snapshot")
    };
    assert_eq!(recovered, expected);
    assert_eq!(
        other_viewer
            .download_artifact(&artifact.link(), None, 60000)
            .unwrap()
            .1,
        content
    );
    for child in std::fs::read_dir(&h.dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".log"))
    {
        let text = std::fs::read_to_string(child.path()).unwrap();
        assert!(!text.contains(worker.expose_secret()));
        assert!(!text.contains(viewer.expose_secret()));
    }
    unsafe { libc::kill(h.children[0].id() as i32, libc::SIGTERM) };
    h.wait_child(0);
}
