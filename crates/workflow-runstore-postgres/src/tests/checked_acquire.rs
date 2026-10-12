use super::*;

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn checked_acquire_postgres_row_lock_serializes_competing_baselines() {
    PostgresRunStore::initialize(&mut client()).unwrap();
    let tenant = format!(
        "checked-race-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut s = store(&tenant);
    s.start(&request("race")).unwrap();
    let baseline = s.acceptance("race").unwrap();
    let checked = AcquireCheckedV1 {
        schema_version: 1,
        lease: lease("race", "first", 30000),
        expected_run_digest: baseline.run_digest,
        expected_bundle_digest: baseline.bundle_digest,
        expected_snapshot_digest: baseline.snapshot_digest,
        expected_execution_digest: baseline.execution_digest,
        expected_snapshot_revision: baseline.revision,
    };
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|owner| {
            let tenant = tenant.clone();
            let barrier = barrier.clone();
            let mut checked = checked.clone();
            checked.lease = lease("race", owner, 30000);
            std::thread::spawn(move || {
                let mut store = store(&tenant);
                barrier.wait();
                store.acquire_checked(&checked, &InvalidClock)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.into_iter().find_map(Result::err).unwrap().code,
        ErrorCode::AdmissionConflict
    );
    let records = s.execution_history("race", 0, 100).unwrap().items;
    assert_eq!(records.len(), 1);
    assert_eq!(s.get("race").unwrap().revision, 1);
    assert_eq!(
        s.acquire_checked(&checked, &InvalidClock).unwrap_err().code,
        ErrorCode::AdmissionConflict
    );
    assert_eq!(s.execution_history("race", 0, 100).unwrap().items, records);
}
