use super::*;
use postgres::NoTls;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn client() -> Client {
    Client::connect(
        &std::env::var("WORKFLOW_TEST_POSTGRES")
            .expect("disposable PostgreSQL connection required"),
        NoTls,
    )
    .unwrap()
}
fn root() -> std::path::PathBuf {
    if let Ok(root) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(root).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn request(id: &str) -> StartRun {
    let mut r: StartRun = serde_json::from_slice(
        &std::fs::read(root().join("examples/execution/offline-start.json")).unwrap(),
    )
    .unwrap();
    r.run_id = id.into();
    r.started_at_unix_ms = workflow_worker::SystemClock.now_unix_ms().unwrap();
    r
}
fn store(tenant: &str) -> PostgresRunStore {
    PostgresRunStore::open(client(), tenant, "project").unwrap()
}
fn lease(id: &str, who: &str, ttl: u64) -> LeaseRequest {
    LeaseRequest {
        run_id: id.into(),
        owner: who.into(),
        acquisition_id: who.into(),
        ttl_ms: ttl,
    }
}
struct InvalidClock;
impl Clock for InvalidClock {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        panic!("shared authority consulted caller clock")
    }
}

#[test]
fn authority_child() {
    let Ok(spec) = std::env::var("WORKFLOW_AUTHORITY_CHILD") else {
        return;
    };
    let spec: serde_json::Value = serde_json::from_str(&spec).unwrap();
    let mut s = store(spec["tenant"].as_str().unwrap());
    std::fs::write(spec["ready"].as_str().unwrap(), b"ready").unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !std::path::Path::new(spec["go"].as_str().unwrap()).exists() {
        assert!(
            std::time::Instant::now() < until,
            "parent barrier timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let r = s.acquire(
        &lease("race", spec["owner"].as_str().unwrap(), 30000),
        &InvalidClock,
    );
    std::fs::write(
        spec["out"].as_str().unwrap(),
        serde_json::to_vec(&r).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn real_postgres_parity_fencing_atomicity_and_outage_contract() {
    PostgresRunStore::initialize(&mut client()).unwrap();
    let tenant = format!(
        "test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut s = store(&tenant);
    let r = request("parity");
    let committed = s.start(&r).unwrap();
    assert!(s.start(&r).unwrap().transition.duplicate);
    let mut local = SqliteRunStore::image_reducer(None).unwrap();
    assert_eq!(local.start(&r).unwrap(), committed);
    assert_eq!(
        local.outbox("parity", 0, 100, false).unwrap(),
        s.outbox("parity", 0, 100, false).unwrap()
    );
    assert_eq!(local.verify("parity").unwrap(), s.verify("parity").unwrap());
    let mut other = store(&format!("{tenant}-other"));
    assert_eq!(other.get("parity").unwrap_err().code, ErrorCode::NotFound);
    assert!(other.list(None, 100).unwrap().items.is_empty());
    assert_eq!(
        other
            .acquire(&lease("parity", "forged", 1000), &InvalidClock)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    // Same IDs can have different immutable content in different scopes, while
    // rebinding within one scope rolls back the new run and its version entries.
    let mut altered = request("different");
    let mut changed = serde_json::to_value(&altered).unwrap();
    for node in changed["bundle"]["workflows"][0]["nodes"]
        .as_array_mut()
        .unwrap()
    {
        if node["id"] == "review" {
            node["kind"]["timeout_ms"] = serde_json::json!(600001);
        }
    }
    altered = serde_json::from_value(changed).unwrap();
    assert_eq!(
        s.start(&altered).unwrap_err().code,
        ErrorCode::BindingConflict
    );
    assert_eq!(s.get("different").unwrap_err().code, ErrorCode::NotFound);
    other.start(&altered).unwrap();

    let acquired = s
        .acquire(&lease("parity", "old", 2000), &InvalidClock)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2020));
    let fresh = s
        .acquire(&lease("parity", "fresh", 30000), &InvalidClock)
        .unwrap();
    assert!(fresh.epoch > acquired.epoch);
    assert_eq!(
        s.claim_next(&acquired, &InvalidClock).unwrap_err().code,
        ErrorCode::LeaseConflict
    );
    s.release(&fresh, &InvalidClock).unwrap();
    // Execute the same complete graph through the production runtime/worker.
    let worker = workflow_builtin_capabilities::worker().unwrap();
    let report = workflow_runtime::drive(
        &mut s,
        &worker,
        "parity",
        &workflow_runtime::DriveOptions {
            owner: "runtime".into(),
            acquisition_id: "runtime-acquire".into(),
            lease_ms: 30000,
            max_commands: 100,
        },
        &workflow_worker::SystemClock,
    )
    .unwrap();
    assert_eq!(report.snapshot.status, RunStatus::Running);
    let target = s.waits("parity", 0, 100).unwrap().items.remove(0);
    let snapshot = s.get("parity").unwrap();
    let submission:SignalSubmission=serde_json::from_value(serde_json::json!({"schema_version":1,"run_id":"parity","run_digest":snapshot.run_digest,"message":{"schema_version":1,"message_id":"approval","source":"trusted-test-operator","target":target.target,"correlation_id":target.correlation_id,"decision":"approve","outputs":{},"expires_at_unix_ms":target.deadline_unix_ms,"reason":"test"}})).unwrap();
    let receipt = s.receive_signal(&submission, &InvalidClock).unwrap();
    assert!(!receipt.duplicate);
    assert!(
        s.receive_signal(&submission, &InvalidClock)
            .unwrap()
            .duplicate
    );
    assert_eq!(s.get("parity").unwrap().status, RunStatus::Succeeded);
    drop(s);
    let mut s = store(&tenant);
    s.verify("parity").unwrap();
    assert_eq!(
        s.execution_history("parity", 0, 100)
            .unwrap()
            .items
            .iter()
            .filter(|r| matches!(r.action, ExecutionAction::Finished { .. }))
            .count(),
        2
    );
    // Independent client processes cannot both acquire one current run lease.
    s.start(&request("race")).unwrap();
    let dir = std::env::temp_dir().join(&tenant);
    std::fs::create_dir(&dir).unwrap();
    let mut children = vec![];
    for i in 0..2 {
        let spec = serde_json::json!({"tenant":tenant,"owner":format!("process-{i}"),"out":dir.join(format!("{i}.json")),"ready":dir.join(format!("{i}.ready")),"go":dir.join("go")});
        children.push(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::authority_child"])
                .env("WORKFLOW_AUTHORITY_CHILD", spec.to_string())
                .spawn()
                .unwrap(),
        );
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !(0..2).all(|i| dir.join(format!("{i}.ready")).exists()) {
        assert!(
            std::time::Instant::now() < until,
            "child readiness timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::write(dir.join("go"), b"go").unwrap();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let results: Vec<std::result::Result<Lease, Error>> = (0..2)
        .map(|i| {
            serde_json::from_slice(&std::fs::read(dir.join(format!("{i}.json"))).unwrap()).unwrap()
        })
        .collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.iter().find_map(|r| r.as_ref().err()).unwrap().code,
        ErrorCode::LeaseBusy
    );
    std::fs::remove_dir_all(dir).unwrap();
    // PostgreSQL is unavailable for writes: no in-memory success may escape.
    let mut c = client();
    c.batch_execute("SET default_transaction_read_only = on")
        .unwrap();
    let mut readonly = PostgresRunStore::open(c, &tenant, "project").unwrap();
    assert_eq!(
        readonly.start(&request("must-not-exist")).unwrap_err().code,
        ErrorCode::Storage
    );
    assert_eq!(
        s.get("must-not-exist").unwrap_err().code,
        ErrorCode::NotFound
    );
    // A failed reducer cannot leak partial transaction changes.
    assert!(
        s.change("race", false, |store, _| {
            let old = store.get("race")?;
            store.apply(&Event {
                event_id: "rolled-back".into(),
                run_id: "race".into(),
                run_digest: old.run_digest,
                expected_revision: old.revision,
                at_unix_ms: old.now_unix_ms,
                kind: workflow_kernel::EventKind::Pause {
                    reason: "rollback".into(),
                },
            })?;
            Err::<(), _>(Error::new(ErrorCode::InvalidRequest, "abort"))
        })
        .is_err()
    );
    assert!(s.get("race").unwrap().pause.is_none());
    // The reducer can finish while valid, then expire before external durability.
    // The final PostgreSQL write must reject and leave no accepted lease record.
    s.start(&request("late-commit")).unwrap();
    let rejected = s.change("late-commit", false, |store, clock| {
        let l = store.acquire(&lease("late-commit", "late", 3000), clock)?;
        std::thread::sleep(std::time::Duration::from_millis(3100));
        Ok(l)
    });
    assert_eq!(rejected.unwrap_err().code, ErrorCode::LeaseConflict);
    assert!(
        s.execution_history("late-commit", 0, 100)
            .unwrap()
            .items
            .is_empty()
    );
    s.verify("late-commit").unwrap();
    // Corrupt content and missing heads are errors even on status/list reads.
    s.start(&request("corrupt")).unwrap();
    client().execute("UPDATE workflow_authority.runs SET image_digest='sha256:invalid' WHERE tenant=$1 AND run_id='corrupt'", &[&tenant]).unwrap();
    assert_eq!(
        s.get("corrupt").unwrap_err().code,
        ErrorCode::CorruptStorage
    );
    assert_eq!(
        s.list(None, 100).unwrap_err().code,
        ErrorCode::CorruptStorage
    );
    client()
        .execute(
            "UPDATE workflow_authority.runs SET image=NULL WHERE tenant=$1 AND run_id='corrupt'",
            &[&tenant],
        )
        .unwrap();
    assert_eq!(
        s.get("corrupt").unwrap_err().code,
        ErrorCode::CorruptStorage
    );
    // A lost live database connection never returns the speculative transition.
    let mut c = client();
    let pid: i32 = c.query_one("SELECT pg_backend_pid()", &[]).unwrap().get(0);
    let mut disconnected = PostgresRunStore::open(c, &tenant, "project").unwrap();
    client()
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .unwrap();
    assert_eq!(
        disconnected.start(&request("lost")).unwrap_err().code,
        ErrorCode::Storage
    );
    assert_eq!(s.get("lost").unwrap_err().code, ErrorCode::NotFound);
    // Preserve uncertain write identity across independent reducer instances.
    let mut r: StartRun = serde_json::from_slice(
        &std::fs::read(root().join("examples/runs/effect-release.json")).unwrap(),
    )
    .unwrap();
    r.run_id = "effect".into();
    r.started_at_unix_ms = workflow_worker::SystemClock.now_unix_ms().unwrap();
    s.start(&r).unwrap();
    let owner = s
        .acquire(&lease("effect", "writer", 30000), &InvalidClock)
        .unwrap();
    let EffectClaim::Call { attempt } = s.claim_effect(&owner, &InvalidClock).unwrap() else {
        panic!("write intent missing")
    };
    let observation = workflow_effects::Observation::Applied {
        receipt: workflow_effects::EffectReceipt {
            operation_key: attempt.intent.operation_key.clone(),
            intent_digest: workflow_worker::digest(&attempt.intent).unwrap(),
            target: attempt.intent.policy.target.clone(),
            resource_id: "sandbox-release".into(),
            provider_receipt: "synthetic-receipt-for-storage-contract".into(),
            outputs: [("release_id".into(), "sandbox-release".into())].into(),
        },
    };
    let mut wrong = observation.clone();
    if let workflow_effects::Observation::Applied { receipt } = &mut wrong {
        receipt.target.id = "other-target".into();
    }
    assert!(
        s.observe_effect(&owner, &attempt.attempt_id, &wrong, &InvalidClock)
            .is_err()
    );
    assert_eq!(
        s.effects("effect", 0, 100).unwrap().items[0].status,
        workflow_effects::EffectStatus::InFlight
    );
    assert_eq!(
        s.observe_effect(&owner, &attempt.attempt_id, &observation, &InvalidClock)
            .unwrap()
            .snapshot
            .status,
        RunStatus::Succeeded
    );
    let mut reopened = store(&tenant);
    assert!(
        reopened
            .observe_effect(&owner, &attempt.attempt_id, &observation, &InvalidClock)
            .unwrap()
            .transition
            .duplicate
    );
    assert_eq!(
        reopened.effects("effect", 0, 100).unwrap().items[0]
            .calls
            .len(),
        1
    );
    reopened.verify("effect").unwrap();
}
