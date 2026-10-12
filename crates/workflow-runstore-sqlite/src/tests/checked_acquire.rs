use super::*;
use workflow_worker::Clock;

struct FixedClock(u64);
impl Clock for FixedClock {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0)
    }
}

fn image(store: &mut SqliteRunStore, id: &str) -> serde_json::Value {
    serde_json::to_value(store.export_image(id).unwrap()).unwrap()
}

fn checked(store: &mut SqliteRunStore, id: &str, owner: &str) -> AcquireCheckedV1 {
    let baseline = store.acceptance(id).unwrap();
    AcquireCheckedV1 {
        schema_version: 1,
        lease: LeaseRequest {
            run_id: id.into(),
            owner: owner.into(),
            acquisition_id: owner.into(),
            ttl_ms: 100,
        },
        expected_run_digest: baseline.run_digest,
        expected_bundle_digest: baseline.bundle_digest,
        expected_snapshot_digest: baseline.snapshot_digest,
        expected_execution_digest: baseline.execution_digest,
        expected_snapshot_revision: baseline.revision,
    }
}

#[test]
fn checked_acquire_each_baseline_conflict_preserves_entire_image() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let request = checked(&mut store, &s.run_id, "owner");
    let before = image(&mut store, &s.run_id);
    for field in [
        "expected_run_digest",
        "expected_bundle_digest",
        "expected_snapshot_digest",
        "expected_execution_digest",
        "expected_snapshot_revision",
    ] {
        let mut value = serde_json::to_value(&request).unwrap();
        value[field] = if field == "expected_snapshot_revision" {
            serde_json::json!(2)
        } else {
            serde_json::json!(format!("sha256:{}", "0".repeat(64)))
        };
        let stale = serde_json::from_value(value).unwrap();
        assert_eq!(
            store
                .acquire_checked(&stale, &FixedClock(1000))
                .unwrap_err()
                .code,
            ErrorCode::AdmissionConflict
        );
        assert_eq!(image(&mut store, &s.run_id), before, "{field}");
    }
    let lease = store.acquire_checked(&request, &FixedClock(1000)).unwrap();
    assert_eq!(lease.owner, "owner");
    assert_eq!(lease.epoch, 1);
    assert_eq!(store.get(&s.run_id).unwrap().revision, 1);
    let after = image(&mut store, &s.run_id);
    assert_eq!(
        store
            .acquire_checked(&request, &FixedClock(1000))
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(image(&mut store, &s.run_id), after);
}

#[test]
fn checked_acquire_fences_execution_advance_without_snapshot_revision_change() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let initial = checked(&mut store, &s.run_id, "checked-owner");
    let mut ordinary = initial.lease.clone();
    ordinary.owner = "ordinary".into();
    ordinary.acquisition_id = "ordinary".into();
    let lease = store.acquire(&ordinary, &FixedClock(1000)).unwrap();
    let _attempt = store.claim_next(&lease, &FixedClock(1000)).unwrap();
    let current = store.acceptance(&s.run_id).unwrap();
    assert_eq!(current.revision, initial.expected_snapshot_revision);
    assert_eq!(current.snapshot_digest, initial.expected_snapshot_digest);
    assert_ne!(current.execution_digest, initial.expected_execution_digest);
    let before = image(&mut store, &s.run_id);
    assert_eq!(
        store
            .acquire_checked(&initial, &FixedClock(1100))
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(image(&mut store, &s.run_id), before);
    let fresh = checked(&mut store, &s.run_id, "checked-owner");
    assert_eq!(
        store
            .acquire_checked(&fresh, &FixedClock(1099))
            .unwrap_err()
            .code,
        ErrorCode::LeaseBusy
    );
    assert_eq!(
        store
            .acquire_checked(&fresh, &FixedClock(1100))
            .unwrap()
            .epoch,
        2
    );
}

#[test]
fn checked_acquire_two_connections_compare_and_append_atomically() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let request = checked(&mut store, &s.run_id, "left");
    drop(store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = ["left", "right"]
        .into_iter()
        .map(|owner| {
            let path = db.path.clone();
            let barrier = barrier.clone();
            let mut request = request.clone();
            request.lease.owner = owner.into();
            request.lease.acquisition_id = owner.into();
            std::thread::spawn(move || {
                let mut store = SqliteRunStore::open(path).unwrap();
                barrier.wait();
                store.acquire_checked(&request, &FixedClock(1000))
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.into_iter().find_map(Result::err).unwrap().code,
        ErrorCode::AdmissionConflict
    );
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    assert_eq!(
        store
            .execution_history(&s.run_id, 0, 100)
            .unwrap()
            .items
            .len(),
        1
    );
    store.verify(&s.run_id).unwrap();
}

#[test]
fn checked_acquire_run_binding_and_released_execution_remain_fenced() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let original = checked(&mut store, &s.run_id, "checked-owner");
    let mut other = start(&s);
    other.run_id = "other-run".into();
    // RunImage deliberately represents one aggregate, so use a separate store
    // for the foreign-run baseline rather than exporting a multi-run database.
    let other_db = Db::new();
    let mut other_store = other_db.store();
    other_store.start(&other).unwrap();
    let other_before = image(&mut other_store, &other.run_id);
    let mut wrong_run = original.clone();
    wrong_run.lease.run_id = other.run_id.clone();
    assert_eq!(
        other_store
            .acquire_checked(&wrong_run, &FixedClock(1000))
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(image(&mut other_store, &other.run_id), other_before);
    let lease = store.acquire(&original.lease, &FixedClock(1000)).unwrap();
    store.release(&lease, &FixedClock(1001)).unwrap();
    let after = store.acceptance(&s.run_id).unwrap();
    assert_eq!(after.snapshot_digest, original.expected_snapshot_digest);
    assert_eq!(after.revision, original.expected_snapshot_revision);
    let before = image(&mut store, &s.run_id);
    assert_eq!(
        store
            .acquire_checked(&original, &FixedClock(1002))
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(image(&mut store, &s.run_id), before);
}
