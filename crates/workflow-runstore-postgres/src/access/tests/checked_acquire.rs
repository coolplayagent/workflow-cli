use super::*;

fn checked(b: &AcceptanceManifest, owner: &str) -> AcquireCheckedV1 {
    AcquireCheckedV1 {
        schema_version: 1,
        lease: LeaseRequest {
            run_id: b.run_id.clone(),
            owner: owner.into(),
            acquisition_id: "checked-acquisition".into(),
            ttl_ms: 30000,
        },
        expected_run_digest: b.run_digest.clone(),
        expected_bundle_digest: b.bundle_digest.clone(),
        expected_snapshot_digest: b.snapshot_digest.clone(),
        expected_execution_digest: b.execution_digest.clone(),
        expected_snapshot_revision: b.revision,
    }
}

// Read only this disposable tenant's persisted state; comparing DB/WAL bytes
// would not establish transactional semantics or stable audit behavior.
fn durable(tenant: &str) -> (Vec<u8>, String, i64, i64) {
    let mut c = client();
    let row = c.query_one("SELECT image,image_digest,generation FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id='checked'", &[&tenant]).unwrap();
    let count: i64 = c
        .query_one(
            "SELECT count(*) FROM workflow_access.audit WHERE tenant=$1 AND project='project'",
            &[&tenant],
        )
        .unwrap()
        .get(0);
    (row.get(0), row.get(1), row.get(2), count)
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn checked_acquire_conflict_has_zero_run_or_audit_write_and_denials_stay_audited() {
    let mut f = Fixture::new();
    let runner = f.credential("runner", Role::Runner);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    let viewer = f.credential("viewer", Role::Viewer);
    f.service
        .start(runner.expose_secret(), &request("checked"))
        .unwrap();
    let baseline = f
        .service
        .acceptance(scheduler.expose_secret(), "checked")
        .unwrap();
    let good = checked(&baseline, &scheduler.id);
    let mut stale = good.clone();
    stale.expected_execution_digest = format!("sha256:{}", "0".repeat(64));
    let before = durable(&f.tenant);
    assert_eq!(
        f.service
            .acquire_checked(scheduler.expose_secret(), &stale)
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(durable(&f.tenant), before);
    let mut wrong = good.clone();
    wrong.lease.owner = "forged-owner".into();
    assert_eq!(
        f.service
            .acquire_checked(scheduler.expose_secret(), &wrong)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let after_owner = durable(&f.tenant);
    assert_eq!(
        (&after_owner.0, &after_owner.1, after_owner.2),
        (&before.0, &before.1, before.2)
    );
    assert_eq!(after_owner.3, before.3 + 1);
    wrong.lease.owner = viewer.id.clone();
    assert_eq!(
        f.service
            .acquire_checked(viewer.expose_secret(), &wrong)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(durable(&f.tenant).3, before.3 + 2);
    let lease = f
        .service
        .acquire_checked(scheduler.expose_secret(), &good)
        .unwrap();
    assert_eq!(lease.owner, scheduler.id);
    let acquired = durable(&f.tenant);
    assert_eq!(
        f.service
            .acquire_checked(scheduler.expose_secret(), &good)
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(durable(&f.tenant), acquired);
    let current = f
        .service
        .acceptance(scheduler.expose_secret(), "checked")
        .unwrap();
    assert_eq!(current.revision, baseline.revision);
    assert_eq!(current.snapshot_digest, baseline.snapshot_digest);
    assert_ne!(current.execution_digest, baseline.execution_digest);
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn checked_acquire_conflict_cannot_outlive_credential_authority() {
    let mut f = Fixture::new();
    let runner = f.credential("runner", Role::Runner);
    f.service
        .start(runner.expose_secret(), &request("checked"))
        .unwrap();
    let baseline = f
        .service
        .acceptance(runner.expose_secret(), "checked")
        .unwrap();
    let short = f
        .service
        .issue(f.admin.expose_secret(), "short", Role::Scheduler, &[], 2000)
        .unwrap();
    let mut stale = checked(&baseline, &short.id);
    stale.expected_snapshot_revision += 1;
    let before = durable(&f.tenant);
    let mut locker = client();
    let mut lock = locker.transaction().unwrap();
    lock.query_one("SELECT run_id FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id='checked' FOR UPDATE", &[&f.tenant]).unwrap();
    let mut connection = client();
    let backend: i32 = connection
        .query_one("SELECT pg_backend_pid()", &[])
        .unwrap()
        .get(0);
    let secret = short.expose_secret().to_string();
    let thread = std::thread::spawn(move || {
        AuthenticatedService::open(connection)
            .unwrap()
            .acquire_checked(&secret, &stale)
    });
    // Observe this exact disposable connection blocked behind our run lock,
    // after successful credential authentication. Never inspect another store.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut observer = client();
    loop {
        let waiting: Option<String> = observer
            .query_one(
                "SELECT wait_event_type FROM pg_stat_activity WHERE pid=$1",
                &[&backend],
            )
            .unwrap()
            .get(0);
        if waiting.as_deref() == Some("Lock") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "checked caller did not reach owned run lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::thread::sleep(std::time::Duration::from_millis(2050));
    lock.commit().unwrap();
    let result = thread.join().unwrap();
    assert_eq!(result.unwrap_err().code, ErrorCode::Unauthorized);
    assert_eq!(durable(&f.tenant), before);
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn checked_acquire_authenticated_race_and_foreign_scope_preserve_zero_write_conflict() {
    let mut f = Fixture::new();
    let runner = f.credential("runner", Role::Runner);
    let left = f.credential("left", Role::Scheduler);
    let right = f.credential("right", Role::Scheduler);
    f.service
        .start(runner.expose_secret(), &request("checked"))
        .unwrap();
    let baseline = f
        .service
        .acceptance(runner.expose_secret(), "checked")
        .unwrap();
    let mut foreign = Fixture::new();
    let foreign_owner = foreign.credential("elsewhere", Role::Scheduler);
    let before = durable(&f.tenant);
    assert_eq!(
        f.service
            .acquire_checked(
                foreign_owner.expose_secret(),
                &checked(&baseline, &foreign_owner.id)
            )
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(durable(&f.tenant), before);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [&left, &right]
        .into_iter()
        .map(|credential| {
            let secret = credential.expose_secret().to_owned();
            let req = checked(&baseline, &credential.id);
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut service = AuthenticatedService::open(client()).unwrap();
                barrier.wait();
                service.acquire_checked(&secret, &req)
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.iter().find_map(|r| r.as_ref().err()).unwrap().code,
        ErrorCode::AdmissionConflict
    );
    let after = durable(&f.tenant);
    assert_eq!(after.2, before.2 + 1);
    assert_eq!(after.3, before.3 + 1);
    let lease = results.into_iter().find_map(Result::ok).unwrap();
    let winner = if lease.owner == left.id {
        &left
    } else {
        &right
    };
    f.service.release(winner.expose_secret(), &lease).unwrap();
    let released = durable(&f.tenant);
    assert_eq!(
        f.service
            .acquire_checked(left.expose_secret(), &checked(&baseline, &left.id))
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(durable(&f.tenant), released);
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn checked_acquire_storage_failure_is_not_a_zero_write_certificate() {
    let mut f = Fixture::new();
    let runner = f.credential("runner", Role::Runner);
    let owner = f.credential("scheduler", Role::Scheduler);
    f.service
        .start(runner.expose_secret(), &request("checked"))
        .unwrap();
    let baseline = f
        .service
        .acceptance(owner.expose_secret(), "checked")
        .unwrap();
    let before = durable(&f.tenant);
    let mut fault = client();
    // This trigger exists only in the dedicated test database and is removed
    // before assertions. It fails the actual authoritative image UPDATE.
    fault.batch_execute("CREATE FUNCTION workflow_authority.checked_test_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'owned test update failure'; END $$; CREATE TRIGGER checked_test_fault BEFORE UPDATE ON workflow_authority.runs FOR EACH ROW EXECUTE FUNCTION workflow_authority.checked_test_fault();").unwrap();
    let result = f
        .service
        .acquire_checked(owner.expose_secret(), &checked(&baseline, &owner.id));
    fault.batch_execute("DROP TRIGGER checked_test_fault ON workflow_authority.runs; DROP FUNCTION workflow_authority.checked_test_fault();").unwrap();
    assert_eq!(result.unwrap_err().code, ErrorCode::Storage);
    let after = durable(&f.tenant);
    assert_eq!(
        (&after.0, &after.1, after.2),
        (&before.0, &before.1, before.2)
    );
    assert_eq!(
        after.3,
        before.3 + 1,
        "ordinary storage rejection keeps its audit"
    );
}
