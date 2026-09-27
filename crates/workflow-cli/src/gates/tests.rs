use super::*;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use workflow_artifact_local::LocalArtifactStore;
use workflow_artifacts::{
    AccessScope, ArtifactStore, ArtifactType, ContentSchema, PublishSpec, Retention, SourceRevision,
};
use workflow_ir::{ValueType, VersionRef};
use workflow_runstore::{
    Claimed, ExecutionStore, LeaseRequest, RunStore, StartRun, artifact_producer,
};
use workflow_runstore_sqlite::SqliteRunStore;
use workflow_worker::{AdapterOutcome, EvidenceRef, SystemClock};
static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    dir: PathBuf,
    db: String,
    root: String,
    request: Request,
    lease: workflow_runstore::Lease,
    attempt: String,
    result: workflow_worker::WorkResult,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn file(&self, name: &str, value: &impl serde::Serialize) -> String {
        let p = self.dir.join(name);
        std::fs::write(&p, serde_json::to_vec(value).unwrap()).unwrap();
        p.to_str().unwrap().into()
    }
    fn store(&self) -> SqliteRunStore {
        SqliteRunStore::open(&self.db)
            .unwrap()
            .with_artifacts(Box::new(LocalArtifactStore::open(&self.root).unwrap()))
    }
    fn finish(&self) {
        self.store()
            .finish_task(&self.lease, &self.attempt, &self.result, &SystemClock)
            .unwrap();
    }
    fn evaluate(&self, request: &Request) -> (i32, serde_json::Value) {
        invoke(&[
            "gate",
            "evaluate",
            &self.db,
            &self.root,
            &self.file("gate.json", request),
        ])
    }
}
fn base() -> PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn invoke(args: &[&str]) -> (i32, serde_json::Value) {
    let mut out = vec![];
    let mut err = vec![];
    let code = crate::run(args.iter().map(|s| s.to_string()), &mut out, &mut err);
    let value = serde_json::from_slice(&out).unwrap_or_else(|_| {
        panic!(
            "{args:?}: {} {}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        )
    });
    (code, value)
}
fn fixture(valid: bool) -> Fixture {
    fixture_with_input(valid, false)
}
fn fixture_with_input(valid: bool, malformed: bool) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "workflow-gate-cli-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir(&dir).unwrap();
    let db = dir.join("runs.db").to_str().unwrap().to_owned();
    let root = dir.join("artifacts").to_str().unwrap().to_owned();
    let mut artifacts = LocalArtifactStore::create(&root).unwrap();
    let mut store = SqliteRunStore::create(&db).unwrap();
    let file = if valid {
        "valid-start.json"
    } else {
        "invalid-start.json"
    };
    let mut start: StartRun =
        parse_message(&std::fs::read(base().join("examples/execution").join(file)).unwrap())
            .unwrap();
    if malformed {
        start
            .inputs
            .insert("format".into(), "unsupported-format".into());
    }
    let started = store.start(&start).unwrap();
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: start.run_id.clone(),
                owner: "gate-test".into(),
                acquisition_id: "gate-1".into(),
                ttl_ms: 300000,
            },
            &SystemClock,
        )
        .unwrap();
    let Claimed::Task { attempt } = store.claim_next(&lease, &SystemClock).unwrap() else {
        panic!("task")
    };
    let mut result = workflow_builtin_capabilities::worker()
        .unwrap()
        .execute_with_clock(&attempt.request, &attempt.grant, &SystemClock)
        .unwrap()
        .result()
        .clone();
    let ty = ArtifactType {
        identity: VersionRef {
            id: "validation.report".into(),
            version: "1.0.0".into(),
        },
        content: ContentSchema::Json {
            value_type: ValueType::Object {
                fields: BTreeMap::from([("valid".into(), ValueType::Boolean)]),
            },
        },
    };
    let source_revision = SourceRevision {
        repository: "test-fixture".into(),
        revision: "a".repeat(40),
    };
    let spec = PublishSpec {
        schema_version: 1,
        artifact_type: ty.clone(),
        producer: artifact_producer(&attempt.request).unwrap(),
        source_revision: source_revision.clone(),
        inputs: vec![],
        access: AccessScope::Run {
            run_id: start.run_id.clone(),
        },
        retention: Retention::RunDependency,
    };
    // A report can claim true; only the actual accepted worker output decides.
    let report = artifacts
        .publish(&spec, &mut br#"{"valid":true}"#.as_slice())
        .unwrap();
    let evidence = match &mut result.outcome {
        AdapterOutcome::Succeeded { outputs, evidence } => {
            assert!(!malformed);
            assert_eq!(outputs["valid"], valid);
            evidence
        }
        AdapterOutcome::Failed { evidence, .. } => {
            assert!(malformed);
            evidence
        }
    };
    evidence.push(EvidenceRef {
        artifact_id: report.artifact_id.clone(),
        digest: report.digest.clone(),
    });
    let workflow_worker::InvocationScope::Workflow { node_id, .. } = &attempt.request.scope else {
        panic!("workflow")
    };
    let request = Request {
        policy: Policy {
            schema_version: 1,
            identity: VersionRef {
                id: "definition.quality".into(),
                version: "1.0.0".into(),
            },
            requirements: vec![Requirement {
                id: "definition-valid".into(),
                node_id: node_id.clone(),
                capability: attempt.request.capability.clone(),
                contract_digest: attempt.request.contract_digest.clone(),
                report_type: ty,
                pass_field: "valid".into(),
                max_age_ms: 60000,
            }],
        },
        target: Target {
            run_id: start.run_id,
            run_digest: started.snapshot.run_digest,
            action: VersionRef {
                id: "definition.publish".into(),
                version: "1.0.0".into(),
            },
            source_revision,
            input_digest: attempt.request.input_digest.clone(),
            artifacts: vec![],
        },
        evidence: vec![Evidence {
            requirement_id: "definition-valid".into(),
            report: report.link(),
        }],
    };
    Fixture {
        dir,
        db,
        root,
        request,
        lease,
        attempt: attempt.attempt_id.clone(),
        result,
    }
}
#[test]
fn only_settled_actual_work_passes_and_checking_never_advances_the_run() {
    let f = fixture(true);
    let before = f.evaluate(&f.request);
    assert_eq!(before.0, 1);
    assert_eq!(
        before.1["result"]["checks"][0]["reason"],
        "unrecorded_evidence"
    );
    f.finish();
    let revision = f.store().get(&f.request.target.run_id).unwrap().revision;
    let pass = f.evaluate(&f.request);
    assert_eq!(pass.0, 0, "{}", pass.1);
    assert_eq!(pass.1["result"]["verdict"], "PASS");
    let previous = f.file("decision.json", &pass.1["result"]);
    let request = f.file("gate.json", &f.request);
    let checked = invoke(&["gate", "revalidate", &f.db, &f.root, &request, &previous]);
    assert_eq!(checked.0, 0, "{}", checked.1);
    assert_eq!(
        f.store().get(&f.request.target.run_id).unwrap().revision,
        revision
    );
    let mut current = f.request.clone();
    current.target.source_revision.revision = "b".repeat(40);
    let rejected = f.evaluate(&current);
    assert_eq!(rejected.0, 1);
    assert_eq!(
        rejected.1["result"]["checks"][0]["reason"],
        "target_mismatch"
    );
    let current = f.file("changed.json", &current);
    assert_eq!(
        invoke(&["gate", "revalidate", &f.db, &f.root, &current, &previous]).0,
        1
    );
    let mut fake = pass.1["result"].clone();
    fake["expires_at_unix_ms"] = json!(u64::MAX);
    let fake = f.file("fake.json", &fake);
    assert_eq!(
        invoke(&["gate", "revalidate", &f.db, &f.root, &request, &fake]).0,
        1
    );
}
#[test]
fn a_forged_true_report_cannot_override_an_actual_failed_validation() {
    let f = fixture(false);
    f.finish();
    let decision = f.evaluate(&f.request);
    assert_eq!(decision.0, 1);
    assert_eq!(decision.1["result"]["verdict"], "FAIL");
    assert_eq!(decision.1["result"]["checks"][0]["reason"], "check_failed");
}
#[test]
fn copied_provenance_without_committed_link_and_corrupt_bytes_cannot_pass() {
    let f = fixture(true);
    f.finish();
    let mut artifacts = LocalArtifactStore::open(&f.root).unwrap();
    use workflow_artifacts::ArtifactReader;
    let original = artifacts.verify(&f.request.evidence[0].report).unwrap();
    let substitute = artifacts
        .publish(
            &original.manifest.spec,
            &mut br#"{"valid":false}"#.as_slice(),
        )
        .unwrap();
    let mut request = f.request.clone();
    request.evidence[0].report = substitute.link();
    let rejected = f.evaluate(&request);
    assert_eq!(rejected.0, 1);
    assert_eq!(
        rejected.1["result"]["checks"][0]["reason"],
        "unrecorded_evidence"
    );
    std::fs::write(
        PathBuf::from(&f.root)
            .join("objects")
            .join(&original.manifest.content_digest[7..]),
        b"corrupt",
    )
    .unwrap();
    let rejected = f.evaluate(&f.request);
    assert_eq!(rejected.0, 1);
    assert_eq!(rejected.1["ok"], false);
    assert!(rejected.1.get("result").is_none());
}
#[test]
fn committed_schemas_match_and_cli_reports_invalid_documents() {
    for kind in ["request", "decision"] {
        let generated = invoke(&["schema", &format!("gate-{kind}")]);
        assert_eq!(generated.0, 0);
        let expected: serde_json::Value = parse_message(
            &std::fs::read(base().join(format!("schemas/gate-{kind}-v1.schema.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(generated.1, expected);
    }
    let result = invoke(&[
        "gate",
        "evaluate",
        "missing-db",
        "missing-artifacts",
        "missing-gate-request.json",
    ]);
    assert_eq!(result.0, 2);
}

#[test]
fn actual_invalid_input_is_unknown_instead_of_a_confirmed_quality_failure() {
    let f = fixture_with_input(true, true);
    f.finish();
    let decision = f.evaluate(&f.request);
    assert_eq!(decision.0, 1);
    assert_eq!(decision.1["result"]["verdict"], "UNKNOWN");
    assert_eq!(
        decision.1["result"]["checks"][0]["reason"],
        "check_inconclusive"
    );
}
