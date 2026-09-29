use super::*;
use serde_json::json;
use workflow_artifacts::{ArtifactRef, ArtifactType, content_digest};
use workflow_worker::{AdapterOutcome, CapabilityAdapter, CapabilityDescriptor, Invocation};

fn artifact_type() -> ArtifactType {
    serde_json::from_value(
        json!({"identity":{"id":"build-report","version":"1.0.0"},"content":{"format":"utf8"}}),
    )
    .unwrap()
}
fn start() -> StartRun {
    let inputs = json!({"repository":{"required":true,"value_type":{"type":"string"}},"revision":{"required":true,"value_type":{"type":"string"}}});
    let outputs = json!({"ok":{"required":true,"value_type":{"type":"boolean"}}});
    serde_json::from_value(json!({
        "schema_version":1,"run_id":"artifact-run","started_at_unix_ms":1,
        "inputs":{"repository":"fixture-repository","revision":"a".repeat(40)},
        "bundle":{"schema_version":1,"root":{"id":"artifact-flow","version":"1.0.0"},
          "workflows":[{"schema_version":1,"id":"artifact-flow","version":"1.0.0","entry":"report","inputs":inputs,
            "nodes":[{"id":"report","kind":{"type":"task","capability":{"id":"artifact.report","version":"1.0.0"},"policy":null},"inputs":inputs,"outputs":outputs,
                "bindings":{"repository":{"source":"workflow_input","field":"repository"},"revision":{"source":"workflow_input","field":"revision"}}},
                {"id":"done","kind":{"type":"terminal","outcome":"succeeded"}}],
            "edges":[{"id":"next","from":"report","to":"done","route":{"type":"next"}}]}],
          "capabilities":[{"schema_version":1,"capability":{"id":"artifact.report","version":"1.0.0"},"inputs":inputs,"outputs":outputs,"timeout_ms":120000,
              "error_codes":{},"effects":{"type":"read_only"},"usage":"Produce a bounded test report","skill":null}]}
    })).unwrap()
}
struct Report(CapabilityDescriptor);
impl CapabilityAdapter for Report {
    fn descriptor(&self) -> CapabilityDescriptor {
        self.0.clone()
    }
    fn invoke(&self, invocation: Invocation<'_>) -> AdapterOutcome {
        assert_eq!(invocation.request.inputs["revision"], json!("a".repeat(40)));
        AdapterOutcome::Succeeded {
            outputs: [("ok".into(), json!(true))].into(),
            evidence: vec![],
        }
    }
}
struct Artifacts {
    base: Fixture,
    worker: IssuedCredential,
    scheduler: IssuedCredential,
    viewer: IssuedCredential,
    lease: Lease,
    assignment: String,
    request: ArtifactUploadRequest,
    content: Vec<u8>,
}
impl Artifacts {
    fn new() -> Self {
        Self::with_start(start())
    }
    fn with_start(start: StartRun) -> Self {
        let mut base = Fixture::new();
        let maintainer = base.credential("artifact-maintainer", Role::DefinitionMaintainer);
        let runner = base.credential("artifact-runner", Role::Runner);
        let viewer = base.credential("artifact-viewer", Role::Viewer);
        let scheduler = base.credential("artifact-scheduler", Role::Scheduler);
        let capability =
            workflow_worker::Capability::new(start.bundle.capabilities[0].clone()).unwrap();
        let policy = ArtifactPolicy {
            inputs: Default::default(),
            output: Some(ArtifactOutputPolicy {
                types: vec![artifact_type()],
                repository_input: "repository".into(),
                revision_input: "revision".into(),
            }),
        };
        let worker = base
            .service
            .issue(
                base.admin.expose_secret(),
                "artifact-worker",
                Role::Worker,
                &[CapabilityRule {
                    id: "artifact.report".into(),
                    version: "1.0.0".into(),
                    contract_digest: capability.digest().into(),
                    artifacts: Some(policy),
                }],
                MAX_TTL,
            )
            .unwrap();
        base.service
            .publish(maintainer.expose_secret(), &start.bundle)
            .unwrap();
        base.service.start(runner.expose_secret(), &start).unwrap();
        let lease = base
            .service
            .acquire(
                scheduler.expose_secret(),
                &start.run_id,
                "artifact-lease",
                60000,
            )
            .unwrap();
        let assignment = next(&mut base.service, &scheduler, &lease, &worker);
        let content = vec![b'x'; ARTIFACT_CHUNK_BYTES + 17];
        let request = ArtifactUploadRequest {
            request_id: "report-upload".into(),
            assignment_id: assignment.clone(),
            artifact_type: artifact_type(),
            bytes: content.len() as u64,
            content_digest: content_digest(&content),
        };
        Self {
            base,
            worker,
            scheduler,
            viewer,
            lease,
            assignment,
            request,
            content,
        }
    }
    fn upload(&mut self) -> ArtifactRef {
        let token = self.worker.expose_secret();
        let upload = self
            .base
            .service
            .begin_artifact_upload(token, &self.request)
            .unwrap();
        assert!(upload.completed.is_none());
        assert_eq!(
            self.base
                .service
                .begin_artifact_upload(token, &self.request)
                .unwrap()
                .upload_id,
            upload.upload_id
        );
        assert_eq!(
            self.base
                .service
                .complete_artifact_upload(token, &upload.upload_id)
                .unwrap_err()
                .code,
            ErrorCode::ReceiptConflict
        );
        let mut changed = self.request.clone();
        changed.content_digest = content_digest(b"other");
        assert_eq!(
            self.base
                .service
                .begin_artifact_upload(token, &changed)
                .unwrap_err()
                .code,
            ErrorCode::ReceiptConflict
        );
        let first = &self.content[..ARTIFACT_CHUNK_BYTES];
        self.base
            .service
            .put_artifact_chunk(token, &upload.upload_id, 0, first)
            .unwrap();
        assert_eq!(
            self.base
                .service
                .put_artifact_chunk(token, &upload.upload_id, 0, first)
                .unwrap()
                .received_bytes,
            ARTIFACT_CHUNK_BYTES as u64
        );
        assert_eq!(
            self.base
                .service
                .put_artifact_chunk(
                    token,
                    &upload.upload_id,
                    0,
                    &vec![b'y'; ARTIFACT_CHUNK_BYTES]
                )
                .unwrap_err()
                .code,
            ErrorCode::ReceiptConflict
        );
        let mut foreign = Fixture::new();
        let foreign_worker = foreign
            .service
            .issue(
                foreign.admin.expose_secret(),
                "foreign-producer",
                Role::Worker,
                &self
                    .base
                    .service
                    .client
                    .query_one(
                        "SELECT capabilities FROM workflow_access.credentials WHERE id=$1",
                        &[&self.worker.id],
                    )
                    .map(|row| serde_json::from_str::<Vec<CapabilityRule>>(row.get(0)).unwrap())
                    .unwrap(),
                MAX_TTL,
            )
            .unwrap();
        assert_eq!(
            self.base
                .service
                .put_artifact_chunk(
                    foreign_worker.expose_secret(),
                    &upload.upload_id,
                    ARTIFACT_CHUNK_BYTES as u64,
                    &self.content[ARTIFACT_CHUNK_BYTES..]
                )
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
        self.base
            .service
            .put_artifact_chunk(
                token,
                &upload.upload_id,
                ARTIFACT_CHUNK_BYTES as u64,
                &self.content[ARTIFACT_CHUNK_BYTES..],
            )
            .unwrap();
        let artifact = self
            .base
            .service
            .complete_artifact_upload(token, &upload.upload_id)
            .unwrap();
        assert_eq!(
            self.base
                .service
                .complete_artifact_upload(token, &upload.upload_id)
                .unwrap(),
            artifact
        );
        assert_eq!(
            self.base
                .service
                .put_artifact_chunk(token, &upload.upload_id, 0, first)
                .unwrap()
                .completed,
            Some(artifact.clone())
        );
        artifact
    }
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn authenticated_artifact_upload_download_recovery_and_scope_contract() {
    let mut f = Artifacts::new();
    let artifact = f.upload();
    assert_eq!(
        artifact.manifest.spec.producer,
        artifact_producer(
            &f.base
                .service
                .assignment(f.worker.expose_secret(), &f.assignment)
                .unwrap()
                .request
        )
        .unwrap()
    );
    assert_eq!(
        artifact.manifest.spec.source_revision.revision,
        "a".repeat(40)
    );
    let grant = f
        .base
        .service
        .grant_artifact_download(f.viewer.expose_secret(), &artifact.link(), None, 60000)
        .unwrap();
    let first = f
        .base
        .service
        .artifact_download_chunk(f.viewer.expose_secret(), &grant.download_id, 0)
        .unwrap();
    let second = f
        .base
        .service
        .artifact_download_chunk(
            f.viewer.expose_secret(),
            &grant.download_id,
            first.next_offset.unwrap(),
        )
        .unwrap();
    assert_eq!([first.content, second.content].concat(), f.content);
    assert!(second.next_offset.is_none());
    let mut foreign = Fixture::new();
    let foreign_viewer = foreign.credential("foreign-viewer", Role::Viewer);
    assert_eq!(
        f.base
            .service
            .grant_artifact_download(foreign_viewer.expose_secret(), &artifact.link(), None, 1000)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        f.base
            .service
            .artifact_download_chunk(foreign_viewer.expose_secret(), &grant.download_id, 0)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let other = f.base.credential("different-viewer", Role::Viewer);
    assert_eq!(
        f.base
            .service
            .artifact_download_chunk(other.expose_secret(), &grant.download_id, 0)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    // Output permission is not implicit permission to read every artifact.
    assert_eq!(
        f.base
            .service
            .grant_artifact_download(
                f.worker.expose_secret(),
                &artifact.link(),
                Some(&f.assignment),
                1000
            )
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let expired = f
        .base
        .service
        .grant_artifact_download(f.viewer.expose_secret(), &artifact.link(), None, 200)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert_eq!(
        f.base
            .service
            .artifact_download_chunk(f.viewer.expose_secret(), &expired.download_id, 0)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert!(
        f.base
            .service
            .artifact_download_chunk(f.viewer.expose_secret(), "../artifact", 0)
            .is_err()
    );
    let task = f
        .base
        .service
        .assignment(f.worker.expose_secret(), &f.assignment)
        .unwrap();
    let mut worker = workflow_worker::Worker::default();
    worker
        .register(Report(start().bundle.capabilities[0].clone()))
        .unwrap();
    let mut result = worker
        .execute(&task.request, &task.grant)
        .unwrap()
        .into_result();
    if let AdapterOutcome::Succeeded { evidence, .. } = &mut result.outcome {
        evidence.push(workflow_worker::EvidenceRef {
            artifact_id: artifact.artifact_id.clone(),
            digest: artifact.digest.clone(),
        });
    }
    f.base
        .service
        .finish(f.worker.expose_secret(), &f.assignment, &result)
        .unwrap();
    for _ in 0..10 {
        if f.base
            .service
            .get(f.viewer.expose_secret(), "artifact-run")
            .unwrap()
            .status
            == RunStatus::Succeeded
        {
            break;
        }
        f.base
            .service
            .dispatch(f.scheduler.expose_secret(), &f.lease, &f.worker.id)
            .unwrap();
    }
    let expected = f
        .base
        .service
        .get(f.viewer.expose_secret(), "artifact-run")
        .unwrap();
    assert_eq!(expected.status, RunStatus::Succeeded);
    assert_eq!(
        AuthenticatedService::open(client())
            .unwrap()
            .get(f.viewer.expose_secret(), "artifact-run")
            .unwrap(),
        expected
    );
    f.base.service.client.execute("UPDATE workflow_artifacts.artifacts SET content=$4 WHERE tenant=$1 AND project=$2 AND id=$3", &[&f.base.tenant,&"project",&artifact.artifact_id,&vec![b'y';f.content.len()]]).unwrap();
    assert!(
        f.base
            .service
            .get(f.viewer.expose_secret(), "artifact-run")
            .is_err()
    );
    f.base.service.client.execute("UPDATE workflow_artifacts.artifacts SET content=$4 WHERE tenant=$1 AND project=$2 AND id=$3", &[&f.base.tenant,&"project",&artifact.artifact_id,&f.content]).unwrap();
    f.base
        .service
        .cleanup_artifact_transfers(f.base.admin.expose_secret(), 100)
        .unwrap();
    assert_eq!(
        f.base
            .service
            .get(f.viewer.expose_secret(), "artifact-run")
            .unwrap(),
        expected
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn artifact_invalid_content_revocation_stale_lease_and_cleanup_contract() {
    let mut f = Artifacts::new();
    let token = f.worker.expose_secret();
    let mut wrong_type = f.request.clone();
    wrong_type.artifact_type.identity.id = "unapproved-report".into();
    assert_eq!(
        f.base
            .service
            .begin_artifact_upload(token, &wrong_type)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let mut wrong_digest = f.request.clone();
    wrong_digest.request_id = "bad-content".into();
    wrong_digest.bytes = 3;
    wrong_digest.content_digest = content_digest(b"abc");
    let bad = f
        .base
        .service
        .begin_artifact_upload(token, &wrong_digest)
        .unwrap();
    f.base
        .service
        .put_artifact_chunk(token, &bad.upload_id, 0, b"def")
        .unwrap();
    assert_eq!(
        f.base
            .service
            .complete_artifact_upload(token, &bad.upload_id)
            .unwrap_err()
            .code,
        ErrorCode::ArtifactRejected
    );
    let count: i64 = f
        .base
        .service
        .client
        .query_one(
            "SELECT count(*) FROM workflow_artifacts.artifacts WHERE tenant=$1",
            &[&f.base.tenant],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        count, 0,
        "invalid bytes must not publish any durable reference"
    );
    let artifact = f.upload();
    let grant = f
        .base
        .service
        .grant_artifact_download(f.viewer.expose_secret(), &artifact.link(), None, 60000)
        .unwrap();
    f.base
        .service
        .revoke(f.base.admin.expose_secret(), &f.viewer.id)
        .unwrap();
    assert_eq!(
        f.base
            .service
            .artifact_download_chunk(f.viewer.expose_secret(), &grant.download_id, 0)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let progress = f
        .base
        .service
        .begin_artifact_upload(f.worker.expose_secret(), &f.request)
        .unwrap();
    f.base
        .service
        .release(f.scheduler.expose_secret(), &f.lease)
        .unwrap();
    let replacement = f
        .base
        .service
        .acquire(
            f.scheduler.expose_secret(),
            &f.lease.run_id,
            "replacement",
            60000,
        )
        .unwrap();
    assert!(replacement.epoch > f.lease.epoch);
    for error in [
        f.base
            .service
            .begin_artifact_upload(f.worker.expose_secret(), &f.request)
            .unwrap_err(),
        f.base
            .service
            .put_artifact_chunk(
                f.worker.expose_secret(),
                &progress.upload_id,
                0,
                &f.content[..ARTIFACT_CHUNK_BYTES],
            )
            .unwrap_err(),
        f.base
            .service
            .complete_artifact_upload(f.worker.expose_secret(), &progress.upload_id)
            .unwrap_err(),
    ] {
        assert_eq!(error.code, ErrorCode::LeaseConflict);
    }
    f.base
        .service
        .revoke(f.base.admin.expose_secret(), &f.worker.id)
        .unwrap();
    assert_eq!(
        f.base
            .service
            .complete_artifact_upload(f.worker.expose_secret(), &bad.upload_id)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    // Expire only this fixture's transfer rows, without changing retained content.
    f.base
        .service
        .client
        .execute(
            "UPDATE workflow_artifacts.uploads SET expires_at=1 WHERE tenant=$1",
            &[&f.base.tenant],
        )
        .unwrap();
    f.base
        .service
        .client
        .execute(
            "UPDATE workflow_artifacts.downloads SET expires_at=1 WHERE tenant=$1",
            &[&f.base.tenant],
        )
        .unwrap();
    let reclaimed = f
        .base
        .service
        .cleanup_artifact_transfers(f.base.admin.expose_secret(), 100)
        .unwrap();
    assert_eq!(reclaimed.uploads, 2);
    assert_eq!(reclaimed.downloads, 1);
    let orphan_chunks: i64 = f
        .base
        .service
        .client
        .query_one(
            "SELECT count(*) FROM workflow_artifacts.chunks WHERE upload_id=$1",
            &[&bad.upload_id],
        )
        .unwrap()
        .get(0);
    assert_eq!(orphan_chunks, 0);
    let fresh_viewer = f.base.credential("fresh-viewer", Role::Viewer);
    let grant = f
        .base
        .service
        .grant_artifact_download(fresh_viewer.expose_secret(), &artifact.link(), None, 60000)
        .unwrap();
    assert_eq!(
        f.base
            .service
            .artifact_download_chunk(fresh_viewer.expose_secret(), &grant.download_id, 0)
            .unwrap()
            .content,
        f.content[..ARTIFACT_CHUNK_BYTES]
    );
}

struct LinkReport(CapabilityDescriptor, ArtifactRef);
impl CapabilityAdapter for LinkReport {
    fn descriptor(&self) -> CapabilityDescriptor {
        self.0.clone()
    }
    fn invoke(&self, _: Invocation<'_>) -> AdapterOutcome {
        AdapterOutcome::Succeeded {
            outputs: [
                ("ok".into(), json!(true)),
                ("report".into(), json!(self.1.link())),
            ]
            .into(),
            evidence: vec![workflow_worker::EvidenceRef {
                artifact_id: self.1.artifact_id.clone(),
                digest: self.1.digest.clone(),
            }],
        }
    }
}
fn finish_report(
    f: &mut Artifacts,
    worker: &IssuedCredential,
    assignment: &str,
    descriptor: CapabilityDescriptor,
    reference: ArtifactRef,
) {
    let task = f
        .base
        .service
        .assignment(worker.expose_secret(), assignment)
        .unwrap();
    let mut executor = workflow_worker::Worker::default();
    executor
        .register(LinkReport(descriptor, reference))
        .unwrap();
    let result = executor
        .execute(&task.request, &task.grant)
        .unwrap()
        .into_result();
    f.base
        .service
        .finish(worker.expose_secret(), assignment, &result)
        .unwrap();
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn worker_reads_only_declared_typed_inputs_and_transitive_lineage() {
    let mut definition = serde_json::to_value(start()).unwrap();
    let field = json!({"required":true,"value_type":{"type":"object","fields":{"artifact_id":{"type":"string"},"digest":{"type":"string"}}}});
    definition["bundle"]["workflows"][0]["nodes"][0]["outputs"]["report"] = field.clone();
    definition["bundle"]["capabilities"][0]["outputs"]["report"] = field.clone();
    for (node, capability, previous) in [
        ("derived", "artifact.derive", "report"),
        ("consume", "artifact.consume", "derived"),
    ] {
        let mut next = definition["bundle"]["workflows"][0]["nodes"][0].clone();
        next["id"] = json!(node);
        next["kind"]["capability"]["id"] = json!(capability);
        next["inputs"]["source"] = field.clone();
        next["bindings"]["source"] =
            json!({"source":"node_output","node":previous,"field":"report"});
        definition["bundle"]["workflows"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(next);
        let mut contract = definition["bundle"]["capabilities"][0].clone();
        contract["capability"]["id"] = json!(capability);
        contract["inputs"]["source"] = field.clone();
        definition["bundle"]["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(contract);
    }
    definition["bundle"]["workflows"][0]["edges"] = json!([
        {"id":"one","from":"report","to":"derived","route":{"type":"next"}},
        {"id":"two","from":"derived","to":"consume","route":{"type":"next"}},
        {"id":"three","from":"consume","to":"done","route":{"type":"next"}}
    ]);
    let start: StartRun = serde_json::from_value(definition).unwrap();
    let mut f = Artifacts::with_start(start.clone());
    let root = f.upload();
    let mut unrelated_request = f.request.clone();
    unrelated_request.request_id = "unrelated".into();
    unrelated_request.bytes = 1;
    unrelated_request.content_digest = content_digest(b"z");
    let unrelated = f
        .base
        .service
        .begin_artifact_upload(f.worker.expose_secret(), &unrelated_request)
        .unwrap();
    f.base
        .service
        .put_artifact_chunk(f.worker.expose_secret(), &unrelated.upload_id, 0, b"z")
        .unwrap();
    let unrelated = f
        .base
        .service
        .complete_artifact_upload(f.worker.expose_secret(), &unrelated.upload_id)
        .unwrap();
    // Move the producer credential temporarily to satisfy Rust's disjoint borrow
    // rules; the server-issued identity and assignment remain unchanged.
    let producer = std::mem::replace(&mut f.worker, f.base.credential("unused", Role::Viewer));
    let assignment = f.assignment.clone();
    finish_report(
        &mut f,
        &producer,
        &assignment,
        start.bundle.capabilities[0].clone(),
        root.clone(),
    );
    let mut previous = root.clone();
    for index in 1..=2 {
        let descriptor = start.bundle.capabilities[index].clone();
        let capability = workflow_worker::Capability::new(descriptor.clone()).unwrap();
        let policy = ArtifactPolicy {
            inputs: [("source".into(), artifact_type())].into(),
            output: Some(ArtifactOutputPolicy {
                types: vec![artifact_type()],
                repository_input: "repository".into(),
                revision_input: "revision".into(),
            }),
        };
        if index == 1 {
            let mut mismatched = policy.clone();
            mismatched.inputs.get_mut("source").unwrap().identity.id = "different-type".into();
            let wrong = f
                .base
                .service
                .issue(
                    f.base.admin.expose_secret(),
                    "wrong-input-type",
                    Role::Worker,
                    &[CapabilityRule {
                        id: descriptor.capability.id.clone(),
                        version: descriptor.capability.version.clone(),
                        contract_digest: capability.digest().into(),
                        artifacts: Some(mismatched),
                    }],
                    MAX_TTL,
                )
                .unwrap();
            let assignment = next(&mut f.base.service, &f.scheduler, &f.lease, &wrong);
            assert_eq!(
                f.base
                    .service
                    .grant_artifact_download(
                        wrong.expose_secret(),
                        &root.link(),
                        Some(&assignment),
                        1000
                    )
                    .unwrap_err()
                    .code,
                ErrorCode::ArtifactRejected
            );
            f.base
                .service
                .release(f.scheduler.expose_secret(), &f.lease)
                .unwrap();
            f.lease = f
                .base
                .service
                .acquire(
                    f.scheduler.expose_secret(),
                    &f.lease.run_id,
                    "correct-input-type",
                    60000,
                )
                .unwrap();
        }
        let worker = f
            .base
            .service
            .issue(
                f.base.admin.expose_secret(),
                &format!("consumer-{index}"),
                Role::Worker,
                &[CapabilityRule {
                    id: descriptor.capability.id.clone(),
                    version: descriptor.capability.version.clone(),
                    contract_digest: capability.digest().into(),
                    artifacts: Some(policy),
                }],
                MAX_TTL,
            )
            .unwrap();
        let assignment = next(&mut f.base.service, &f.scheduler, &f.lease, &worker);
        for reference in [&previous, &root] {
            let grant = f
                .base
                .service
                .grant_artifact_download(
                    worker.expose_secret(),
                    &reference.link(),
                    Some(&assignment),
                    60000,
                )
                .unwrap();
            assert!(
                !f.base
                    .service
                    .artifact_download_chunk(worker.expose_secret(), &grant.download_id, 0)
                    .unwrap()
                    .content
                    .is_empty()
            );
        }
        assert_eq!(
            f.base
                .service
                .grant_artifact_download(
                    worker.expose_secret(),
                    &unrelated.link(),
                    Some(&assignment),
                    60000
                )
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
        if index == 1 {
            let upload = f
                .base
                .service
                .begin_artifact_upload(
                    worker.expose_secret(),
                    &ArtifactUploadRequest {
                        request_id: "derived".into(),
                        assignment_id: assignment.clone(),
                        artifact_type: artifact_type(),
                        bytes: 7,
                        content_digest: content_digest(b"derived"),
                    },
                )
                .unwrap();
            f.base
                .service
                .put_artifact_chunk(worker.expose_secret(), &upload.upload_id, 0, b"derived")
                .unwrap();
            let derived = f
                .base
                .service
                .complete_artifact_upload(worker.expose_secret(), &upload.upload_id)
                .unwrap();
            assert_eq!(derived.manifest.spec.inputs, vec![root.link()]);
            finish_report(&mut f, &worker, &assignment, descriptor, derived.clone());
            previous = derived;
        } else {
            let grant = f
                .base
                .service
                .grant_artifact_download(
                    worker.expose_secret(),
                    &root.link(),
                    Some(&assignment),
                    60000,
                )
                .unwrap();
            f.base
                .service
                .release(f.scheduler.expose_secret(), &f.lease)
                .unwrap();
            assert_eq!(
                f.base
                    .service
                    .artifact_download_chunk(worker.expose_secret(), &grant.download_id, 0)
                    .unwrap_err()
                    .code,
                ErrorCode::LeaseConflict
            );
        }
    }
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn artifact_schema_unknown_version_is_never_silently_upgraded() {
    let f = Fixture::new();
    let mut db = client();
    let mut tx = db.transaction().unwrap();
    tx.execute(
        "UPDATE workflow_artifacts.schema_version SET version=99",
        &[],
    )
    .unwrap();
    assert_eq!(
        artifact_catalog::initialize(&mut tx).unwrap_err().code,
        ErrorCode::UnsupportedStorage
    );
    let version: i32 = tx
        .query_one("SELECT version FROM workflow_artifacts.schema_version", &[])
        .unwrap()
        .get(0);
    assert_eq!(version, 99);
    tx.rollback().unwrap();
    drop(f);
    AuthenticatedService::initialize_artifacts(&mut db).unwrap();
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn completed_upload_receipts_cannot_bypass_transfer_capacity() {
    let mut f = Artifacts::new();
    let artifact = f.upload();
    // Seed a bounded near-capacity history of distinct request IDs for the same
    // immutable content. Completed deduplicated objects still consume metadata.
    f.base.service.client.execute("INSERT INTO workflow_artifacts.uploads(id,tenant,project,run_id,worker_id,assignment_id,request_id,reference,expected_bytes,received_bytes,expires_at,completed_id) SELECT u.id||'-quota-'||n,u.tenant,u.project,u.run_id,u.worker_id,u.assignment_id,u.request_id||'-quota-'||n,u.reference,u.expected_bytes,u.received_bytes,u.expires_at,u.completed_id FROM workflow_artifacts.uploads u CROSS JOIN generate_series(1,511) n WHERE u.worker_id=$1", &[&f.worker.id]).unwrap();
    let mut request = f.request.clone();
    request.request_id = "one-more".into();
    assert_eq!(
        f.base
            .service
            .begin_artifact_upload(f.worker.expose_secret(), &request)
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    assert_eq!(
        f.base
            .service
            .begin_artifact_upload(f.worker.expose_secret(), &f.request)
            .unwrap()
            .completed,
        Some(artifact)
    );
    f.base.service.client.execute("UPDATE workflow_artifacts.uploads SET expires_at=1 WHERE worker_id=$1 AND request_id<>$2", &[&f.worker.id,&f.request.request_id]).unwrap();
    let reclaimed = f
        .base
        .service
        .cleanup_artifact_transfers(f.base.admin.expose_secret(), 100)
        .unwrap();
    assert_eq!(reclaimed.uploads, 100);
    assert!(
        f.base
            .service
            .begin_artifact_upload(f.worker.expose_secret(), &request)
            .is_ok()
    );
}
