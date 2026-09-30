use super::*;
use serde_json::json;
use workflow_artifacts::{ArtifactLink, ArtifactType};
use workflow_worker::{AdapterOutcome, Capability, WorkResult};

fn fixture(path: &str) -> StartRun {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    serde_json::from_slice(&std::fs::read(root.join(path)).unwrap()).unwrap()
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn database_restore_verifies_dependencies_atomically_and_fences_every_old_identity() {
    let name = format!(
        "workflow_restore_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut host = client();
    host.batch_execute(&format!("CREATE DATABASE {name}"))
        .unwrap();
    let mut config: postgres::Config = std::env::var("WORKFLOW_TEST_POSTGRES")
        .unwrap()
        .parse()
        .unwrap();
    config.dbname(&name);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        restore_contract(&config, &name)
    }));
    host.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn restore_contract(config: &postgres::Config, name: &str) {
    let connect = || config.connect(postgres::NoTls).unwrap();
    let mut control = connect();
    let admin =
        AuthenticatedService::bootstrap(&mut control, "tenant", "project", "admin", MAX_TTL)
            .unwrap();
    AuthenticatedService::bootstrap(
        &mut control,
        "other-tenant",
        "project",
        "other-admin",
        MAX_TTL,
    )
    .unwrap();
    let mut service = AuthenticatedService::open(connect()).unwrap();
    let author = service
        .issue(
            admin.expose_secret(),
            "author",
            Role::DefinitionMaintainer,
            &[],
            MAX_TTL,
        )
        .unwrap();
    let runner = service
        .issue(admin.expose_secret(), "runner", Role::Runner, &[], MAX_TTL)
        .unwrap();
    let scheduler = service
        .issue(
            admin.expose_secret(),
            "scheduler",
            Role::Scheduler,
            &[],
            MAX_TTL,
        )
        .unwrap();
    let viewer = service
        .issue(admin.expose_secret(), "viewer", Role::Viewer, &[], MAX_TTL)
        .unwrap();

    let pending = request("pending-task");
    service
        .publish(author.expose_secret(), &pending.bundle)
        .unwrap();
    service.start(runner.expose_secret(), &pending).unwrap();
    let worker = service
        .issue(
            admin.expose_secret(),
            "worker",
            Role::Worker,
            &rules(),
            MAX_TTL,
        )
        .unwrap();
    let old_lease = service
        .acquire(
            scheduler.expose_secret(),
            &pending.run_id,
            "pending-lease",
            60000,
        )
        .unwrap();
    let assignment = next(&mut service, &scheduler, &old_lease, &worker);
    let task = service
        .assignment(worker.expose_secret(), &assignment)
        .unwrap();
    let old_result = execute(&task);

    let report_run = fixture("examples/artifacts/shared-report-start.json");
    service
        .publish(author.expose_secret(), &report_run.bundle)
        .unwrap();
    service.start(runner.expose_secret(), &report_run).unwrap();
    let cap = Capability::new(report_run.bundle.capabilities[0].clone()).unwrap();
    let report_type: ArtifactType = serde_json::from_value(
        json!({"identity":{"id":"restore-report","version":"1.0.0"},"content":{"format":"utf8"}}),
    )
    .unwrap();
    let rule = CapabilityRule {
        id: cap.descriptor().capability.id.clone(),
        version: cap.descriptor().capability.version.clone(),
        contract_digest: cap.digest().into(),
        model_policy: None,
        effect: None,
        artifacts: Some(ArtifactPolicy {
            inputs: Default::default(),
            output: Some(ArtifactOutputPolicy {
                types: vec![report_type.clone()],
                repository_input: "repository".into(),
                revision_input: "revision".into(),
            }),
        }),
    };
    let reporter = service
        .issue(
            admin.expose_secret(),
            "reporter",
            Role::Worker,
            &[rule],
            MAX_TTL,
        )
        .unwrap();
    let lease = service
        .acquire(
            scheduler.expose_secret(),
            &report_run.run_id,
            "report-lease",
            60000,
        )
        .unwrap();
    let report_assignment = next(&mut service, &scheduler, &lease, &reporter);
    let report_task = service
        .assignment(reporter.expose_secret(), &report_assignment)
        .unwrap();
    let content = b"retained committed report";
    let upload = service
        .begin_artifact_upload(
            reporter.expose_secret(),
            &ArtifactUploadRequest {
                request_id: "report".into(),
                assignment_id: report_assignment.clone(),
                artifact_type: report_type,
                bytes: content.len() as u64,
                content_digest: workflow_artifacts::content_digest(content),
            },
        )
        .unwrap();
    service
        .put_artifact_chunk(reporter.expose_secret(), &upload.upload_id, 0, content)
        .unwrap();
    let artifact = service
        .complete_artifact_upload(reporter.expose_secret(), &upload.upload_id)
        .unwrap();
    let completion = WorkResult {
        protocol_version: 1,
        request_digest: workflow_worker::digest(&report_task.request).unwrap(),
        completed_at_unix_ms: workflow_worker::SystemClock.now_unix_ms().unwrap(),
        outcome: AdapterOutcome::Succeeded {
            outputs: [("ok".into(), json!(true))].into(),
            evidence: vec![workflow_worker::EvidenceRef {
                artifact_id: artifact.artifact_id.clone(),
                digest: artifact.digest.clone(),
            }],
        },
        model_record: None,
    };
    service
        .finish(reporter.expose_secret(), &report_assignment, &completion)
        .unwrap();
    let completed = service
        .get(viewer.expose_secret(), &report_run.run_id)
        .unwrap();
    assert_eq!(completed.status, RunStatus::Succeeded);
    let pending_before = service
        .get(viewer.expose_secret(), &pending.run_id)
        .unwrap();
    let original: Vec<(String, String)> = control
        .query(
            "SELECT run_id,image_digest FROM workflow_authority.runs ORDER BY run_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let credential_count: i64 = control
        .query_one("SELECT count(*) FROM workflow_access.credentials", &[])
        .unwrap()
        .get(0);
    let request = DatabaseRestoreRequest {
        database: name.into(),
        backup_digest: hash(b"verified-test-dump"),
        actor: "recovery-operator".into(),
        reason: "isolated restored database acceptance".into(),
    };

    // A late corrupt dependency rolls back earlier ownership changes and all
    // credential replacement, including credentials from the other tenant.
    control
        .execute(
            "UPDATE workflow_artifacts.artifacts SET content=$1 WHERE id=$2",
            &[&b"corrupt".to_vec(), &artifact.artifact_id],
        )
        .unwrap();
    assert!(AuthenticatedService::fence_restored_database(&mut control, &request).is_err());
    let after: Vec<(String, String)> = control
        .query(
            "SELECT run_id,image_digest FROM workflow_authority.runs ORDER BY run_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(original, after);
    assert_eq!(
        control
            .query_one(
                "SELECT count(*) FROM workflow_access.credentials WHERE revoked=false",
                &[]
            )
            .unwrap()
            .get::<_, i64>(0),
        credential_count
    );
    assert_eq!(
        service
            .get(viewer.expose_secret(), &pending.run_id)
            .unwrap(),
        pending_before
    );
    control
        .execute(
            "UPDATE workflow_artifacts.artifacts SET content=$1 WHERE id=$2",
            &[&content.to_vec(), &artifact.artifact_id],
        )
        .unwrap();
    let mut wrong = request.clone();
    wrong.database = "wrong-database".into();
    assert_eq!(
        AuthenticatedService::fence_restored_database(&mut control, &wrong)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    let restored = AuthenticatedService::fence_restored_database(&mut control, &request).unwrap();
    assert_eq!(restored.report.runs, 2);
    assert_eq!(restored.report.scopes, 2);
    assert_eq!(restored.report.revoked_credentials, credential_count as u64);
    assert_eq!(restored.report.fenced_assignments, 1);
    assert_eq!(
        service
            .get(viewer.expose_secret(), &pending.run_id)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        service
            .finish(worker.expose_secret(), &assignment, &old_result)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert!(
        service
            .issue(
                admin.expose_secret(),
                "old-admin",
                Role::Viewer,
                &[],
                MAX_TTL
            )
            .is_err()
    );
    let admin = &restored
        .administrators
        .iter()
        .find(|a| a.tenant == "tenant")
        .unwrap()
        .credential;
    assert!(!format!("{restored:?}").contains(admin.expose_secret()));
    let recovery = service
        .issue(
            admin.expose_secret(),
            "recovery",
            Role::Recovery,
            &[],
            MAX_TTL,
        )
        .unwrap();
    assert_eq!(
        service
            .get(recovery.expose_secret(), &report_run.run_id)
            .unwrap(),
        completed
    );
    let held = service
        .get(recovery.expose_secret(), &pending.run_id)
        .unwrap();
    assert!(held.pause.is_some());
    assert_eq!(held.frames, pending_before.frames);
    let link = ArtifactLink {
        artifact_id: artifact.artifact_id.clone(),
        digest: artifact.digest.clone(),
    };
    let download = service
        .grant_artifact_download(recovery.expose_secret(), &link, None, 60000)
        .unwrap();
    assert_eq!(
        service
            .artifact_download_chunk(recovery.expose_secret(), &download.download_id, 0)
            .unwrap()
            .content,
        content
    );

    let mut store = PostgresRunStore::open(connect(), "tenant", "project").unwrap();
    assert_eq!(
        store
            .finish_task(
                &old_lease,
                &task.attempt_id,
                &old_result,
                &workflow_worker::SystemClock
            )
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    let barrier = store.recovery_barrier(&pending.run_id).unwrap().unwrap();
    assert_eq!(barrier.generation, restored.report.generation);
    assert_eq!(barrier.source_revision, pending_before.revision);
    assert_eq!(
        barrier.source_state_digest,
        workflow_worker::digest(&pending_before).unwrap()
    );
    let again = AuthenticatedService::fence_restored_database(&mut control, &request).unwrap();
    assert_ne!(again.report.generation, restored.report.generation);
    assert!(
        service
            .issue(
                admin.expose_secret(),
                "old-recovery-admin",
                Role::Viewer,
                &[],
                MAX_TTL
            )
            .is_err()
    );
    let after = store.get(&pending.run_id).unwrap();
    assert_eq!(
        after, held,
        "repeated recovery preserves the original pause/deadlines"
    );
}
