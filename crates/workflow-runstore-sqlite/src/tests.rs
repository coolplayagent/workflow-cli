use super::*;
use std::{
    path::PathBuf,
    process::{Command as Process, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use workflow_kernel::{EventKind, Scenario, TaskResult};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Db {
    dir: PathBuf,
    path: PathBuf,
}
impl Db {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "workflow-runstore-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runs.db");
        Self { dir, path }
    }
    fn store(&self) -> SqliteRunStore {
        SqliteRunStore::create(&self.path).unwrap()
    }
}
impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn base() -> PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn scenario(name: &str) -> Scenario {
    workflow_worker::parse_message(
        &std::fs::read(base().join(format!("examples/kernel/{name}.json"))).unwrap(),
    )
    .unwrap()
}
fn start(s: &Scenario) -> StartRun {
    StartRun {
        schema_version: 1,
        bundle: s.bundle.clone(),
        run_id: s.run_id.clone(),
        inputs: s.inputs.clone(),
        started_at_unix_ms: s.started_at_unix_ms,
        limits: s.limits.clone(),
    }
}
fn event(s: &Snapshot, id: &str, kind: EventKind) -> Event {
    Event {
        event_id: id.into(),
        run_id: s.run_id.clone(),
        run_digest: s.run_digest.clone(),
        expected_revision: s.revision,
        at_unix_ms: s.now_unix_ms + 1,
        kind,
    }
}

#[test]
fn committed_scenarios_recover_without_dispatch_and_lock_the_original_start() {
    for name in [
        "review-approved",
        "review-rejected",
        "parallel-all",
        "parallel-any-cancel",
        "repair-third-round",
    ] {
        let db = Db::new();
        let s = scenario(name);
        let req = start(&s);
        let mut store = db.store();
        let initial = store.start(&req).unwrap();
        assert_eq!(initial.snapshot.revision, 1);
        let mut expected = initial.snapshot;
        for e in &s.events {
            drop(store);
            store = SqliteRunStore::open(&db.path).unwrap();
            let applied = store.apply(e).unwrap();
            expected = applied.snapshot;
            assert!(!applied.transition.duplicate);
            assert_eq!(store.get(&s.run_id).unwrap(), expected);
        }
        let duplicate = store.start(&req).unwrap();
        assert!(duplicate.transition.duplicate && duplicate.transition.commands.is_empty());
        assert_eq!(duplicate.snapshot, expected);
        assert!(
            store
                .apply(s.events.last().unwrap())
                .unwrap()
                .transition
                .duplicate
        );
        let mut changed = req;
        changed.started_at_unix_ms += 1;
        assert_eq!(
            store.start(&changed).unwrap_err().code,
            ErrorCode::StartConflict
        );
        let verified = store.verify(&s.run_id).unwrap();
        assert_eq!(verified.events_checked, s.events.len() as u64);
        let pending = store.outbox(&s.run_id, 0, 100, true).unwrap();
        assert!(!pending.items.is_empty());
        assert_eq!(store.get(&s.run_id).unwrap(), expected);
    }
}
#[test]
fn periodic_checkpoints_and_wait_deadlines_agree_with_complete_history() {
    let db = Db::new();
    let s = scenario("review-approved");
    let mut store = db.store();
    let mut snapshot = store.start(&start(&s)).unwrap().snapshot;
    let deadline = snapshot.frames[&1].nodes["review"].state.clone();
    for i in 0..34 {
        let e = event(&snapshot, &format!("tick-{i}"), EventKind::AdvanceTime);
        snapshot = store.apply(&e).unwrap().snapshot;
    }
    drop(store);
    let mut store = SqliteRunStore::open_readonly(&db.path).unwrap();
    assert_eq!(store.get(&s.run_id).unwrap(), snapshot);
    assert_eq!(snapshot.frames[&1].nodes["review"].state, deadline);
    let v = store.verify(&s.run_id).unwrap();
    assert_eq!(v.checkpoint_revision, 32);
    assert_eq!(v.events_checked, 34);
    let p = store.history(&s.run_id, 0, 10).unwrap();
    assert_eq!(p.items.len(), 10);
    assert_eq!(p.next_cursor, Some(11));
    assert_eq!(store.history(&s.run_id, 31, 100).unwrap().items.len(), 4);
}
#[test]
fn receipts_are_ordered_immutable_and_separate_from_run_state() {
    let db = Db::new();
    let s = scenario("parallel-all");
    let mut store = db.store();
    let initial = store.start(&start(&s)).unwrap().snapshot;
    let entries = store.outbox(&s.run_id, 0, 100, true).unwrap().items;
    let receipt = |e: &OutboxEntry| DeliveryReceipt {
        run_id: e.run_id.clone(),
        command_id: e.command_id.clone(),
        command_digest: e.command_digest.clone(),
        delivery_id: format!("delivery-{}", e.sequence),
    };
    assert_eq!(
        store.acknowledge(&receipt(&entries[1])).unwrap_err().code,
        ErrorCode::DeliveryOrder
    );
    let first = receipt(&entries[0]);
    let accepted = store.acknowledge(&first).unwrap();
    assert_eq!(accepted.receipt, Some(first.clone()));
    assert_eq!(store.acknowledge(&first).unwrap(), accepted);
    let mut changed = first.clone();
    changed.delivery_id = "different".into();
    assert_eq!(
        store.acknowledge(&changed).unwrap_err().code,
        ErrorCode::ReceiptConflict
    );
    let mut changed = receipt(&entries[1]);
    changed.command_digest = first.command_id;
    assert_eq!(
        store.acknowledge(&changed).unwrap_err().code,
        ErrorCode::ReceiptConflict
    );
    drop(store);
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    assert_eq!(store.get(&s.run_id).unwrap(), initial);
    assert_eq!(
        store.outbox(&s.run_id, 0, 1, true).unwrap().items[0].sequence,
        2
    );
    store.acknowledge(&receipt(&entries[1])).unwrap();
    assert!(
        store
            .outbox(&s.run_id, 0, 100, true)
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(store.get(&s.run_id).unwrap(), initial);
}
#[test]
fn conflicts_duplicate_completion_and_terminal_events_have_no_partial_effect() {
    let db = Db::new();
    let s = scenario("parallel-all");
    let mut store = db.store();
    store.start(&start(&s)).unwrap();
    let first = store.apply(&s.events[0]).unwrap();
    let mut changed = s.events[0].clone();
    changed.at_unix_ms += 1;
    assert_eq!(
        store.apply(&changed).unwrap_err().kernel_code,
        Some(workflow_kernel::ErrorCode::EventConflict)
    );
    changed.event_id = "fresh-id".into();
    assert_eq!(
        store.apply(&changed).unwrap_err().kernel_code,
        Some(workflow_kernel::ErrorCode::RevisionConflict)
    );
    changed.expected_revision = first.snapshot.revision;
    assert_eq!(
        store.apply(&changed).unwrap_err().kernel_code,
        Some(workflow_kernel::ErrorCode::InvalidTaskResult)
    );
    assert_eq!(store.get(&s.run_id).unwrap(), first.snapshot);
    assert_eq!(store.history(&s.run_id, 0, 100).unwrap().items.len(), 1);
    let done = store.apply(&s.events[1]).unwrap().snapshot;
    let cancel = event(&done, "late-cancel", EventKind::Cancel);
    assert_eq!(
        store.apply(&cancel).unwrap_err().kernel_code,
        Some(workflow_kernel::ErrorCode::TerminalRun)
    );
    assert_eq!(store.get(&s.run_id).unwrap(), done);
}
#[test]
fn sql_immutability_and_replay_detect_missing_or_corrupt_records() {
    for target in [
        "bundle",
        "seed",
        "event",
        "head",
        "checkpoint",
        "outbox",
        "receipt",
    ] {
        let db = Db::new();
        let s = scenario("parallel-all");
        let mut store = db.store();
        store.start(&start(&s)).unwrap();
        store.apply(&s.events[0]).unwrap();
        let first = store.outbox(&s.run_id, 0, 1, true).unwrap().items.remove(0);
        store
            .acknowledge(&DeliveryReceipt {
                run_id: s.run_id.clone(),
                command_id: first.command_id,
                command_digest: first.command_digest,
                delivery_id: "sent".into(),
            })
            .unwrap();
        let sql = match target {
            "bundle" => "UPDATE bundles SET document='{}'",
            "seed" => "UPDATE runs SET seed='{}'",
            "event" => "DELETE FROM events",
            "head" => "UPDATE heads SET revision=revision+2",
            "checkpoint" => "DELETE FROM checkpoints",
            "outbox" => "DELETE FROM outbox WHERE sequence=1",
            _ => "DELETE FROM receipts",
        };
        assert!(store.connection.execute_batch(sql).is_err());
        let trigger = match target {
            "bundle" => "immutable_bundles_update",
            "seed" => "immutable_runs_update",
            "event" => "immutable_events_delete",
            "head" => "monotonic_heads",
            "checkpoint" => "immutable_checkpoints_delete",
            "outbox" => "immutable_outbox_delete",
            _ => "immutable_receipts_delete",
        };
        store
            .connection
            .execute_batch(&format!("DROP TRIGGER {trigger}"))
            .unwrap();
        // Simulate damaged storage beyond the normal API/SQL guards.
        store
            .connection
            .pragma_update(None, "foreign_keys", "OFF")
            .unwrap();
        store.connection.execute_batch(sql).unwrap();
        assert_eq!(
            store.verify(&s.run_id).unwrap_err().code,
            ErrorCode::CorruptStorage,
            "{target}"
        );
    }
}
#[test]
fn only_init_creates_storage_and_foreign_or_future_databases_are_refused() {
    let db = Db::new();
    assert!(SqliteRunStore::open(&db.path).is_err());
    assert!(!db.path.exists());
    let c = Connection::open(&db.path).unwrap();
    c.execute_batch("CREATE TABLE foreign_table(id INTEGER)")
        .unwrap();
    drop(c);
    assert!(matches!(
        SqliteRunStore::create(&db.path),
        Err(Error {
            code: ErrorCode::UnsupportedStorage,
            ..
        })
    ));
    let db = Db::new();
    let store = db.store();
    store
        .connection
        .pragma_update(None, "user_version", STORAGE_VERSION + 1)
        .unwrap();
    drop(store);
    assert!(matches!(
        SqliteRunStore::create(&db.path),
        Err(Error {
            code: ErrorCode::UnsupportedStorage,
            ..
        })
    ));
    assert!(matches!(
        SqliteRunStore::open_readonly(&db.path),
        Err(Error {
            code: ErrorCode::UnsupportedStorage,
            ..
        })
    ));
}
#[test]
fn pages_are_bounded_and_readonly_connections_cannot_confirm_a_write() {
    let db = Db::new();
    let mut store = db.store();
    let mut req = start(&scenario("parallel-all"));
    for id in ["c", "a", "b"] {
        req.run_id = id.into();
        store.start(&req).unwrap();
    }
    let p = store.list(None, 2).unwrap();
    assert_eq!(p.next_cursor.as_deref(), Some("b"));
    assert_eq!(p.items[0].run_id, "a");
    assert_eq!(
        store.list(p.next_cursor.as_deref(), 2).unwrap().items[0].run_id,
        "c"
    );
    assert_eq!(
        store.list(None, 0).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        store.outbox("a", 0, 101, false).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let snap = store.get("a").unwrap();
    drop(store);
    let mut readonly = SqliteRunStore::open_readonly(&db.path).unwrap();
    assert_eq!(
        readonly
            .apply(&event(&snap, "cancel", EventKind::Cancel))
            .unwrap_err()
            .code,
        ErrorCode::Storage
    );
    assert_eq!(readonly.get("a").unwrap(), snap);
}
#[test]
fn sqlite_disk_full_rolls_back_without_confirming_start_or_transition() {
    let db = Db::new();
    let c = Connection::open(&db.path).unwrap();
    c.execute_batch("PRAGMA page_size=512; VACUUM;").unwrap();
    drop(c);
    let mut store = db.store();
    let pages: i64 = store
        .connection
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    store
        .connection
        .pragma_update(None, "max_page_count", pages)
        .unwrap();
    let s = scenario("parallel-all");
    let error = store.start(&start(&s)).unwrap_err();
    assert_eq!(error.code, ErrorCode::Storage);
    assert!(error.message.contains("full"), "{error}");
    assert!(store.list(None, 100).unwrap().items.is_empty());
    store
        .connection
        .pragma_update(None, "max_page_count", 100000)
        .unwrap();
    let before = store.start(&start(&s)).unwrap().snapshot;
    let pages: i64 = store
        .connection
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    store
        .connection
        .pragma_update(None, "max_page_count", pages)
        .unwrap();
    let e = event(
        &before,
        "unknown",
        EventKind::TaskCompleted {
            instance_id: before.frames[&1].nodes["unit"].instance_id,
            result: TaskResult::Uncertain {
                reason: "x".repeat(1024),
            },
        },
    );
    let error = store.apply(&e).unwrap_err();
    assert_eq!(error.code, ErrorCode::Storage);
    assert!(error.message.contains("full"), "{error}");
    assert_eq!(store.get(&s.run_id).unwrap(), before);
    assert!(store.history(&s.run_id, 0, 100).unwrap().items.is_empty());
    assert_eq!(
        store.outbox(&s.run_id, 0, 100, true).unwrap().items.len(),
        2
    );
    store
        .connection
        .pragma_update(None, "max_page_count", 100000)
        .unwrap();
    assert_eq!(store.apply(&e).unwrap().snapshot.revision, 2);
}
#[test]
fn a_locked_writer_returns_busy_and_preserves_the_committed_state() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    let before = store.start(&start(&s)).unwrap().snapshot;
    let c = Connection::open(&db.path).unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    store
        .connection
        .busy_timeout(Duration::from_millis(20))
        .unwrap();
    assert_eq!(store.apply(&s.events[0]).unwrap_err().code, ErrorCode::Busy);
    c.execute_batch("ROLLBACK").unwrap();
    assert_eq!(store.get(&s.run_id).unwrap(), before);
}

fn child(db: &Db, mode: &str, slot: &str, phase: &str) -> std::process::Child {
    Process::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::process_worker", "--nocapture"])
        .env("WORKFLOW_STORE_PROCESS_DB", &db.path)
        .env("WORKFLOW_STORE_PROCESS_MODE", mode)
        .env("WORKFLOW_STORE_PROCESS_SLOT", slot)
        .env("WORKFLOW_STORE_PROCESS_PHASE", phase)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
fn wait_file(path: &std::path::Path, child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before marker: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "worker marker timeout: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn process_worker() {
    let Ok(db) = std::env::var("WORKFLOW_STORE_PROCESS_DB") else {
        return;
    };
    let db = PathBuf::from(db);
    let dir = db.parent().unwrap();
    let mode = std::env::var("WORKFLOW_STORE_PROCESS_MODE").unwrap();
    let slot = std::env::var("WORKFLOW_STORE_PROCESS_SLOT").unwrap();
    let phase = std::env::var("WORKFLOW_STORE_PROCESS_PHASE").unwrap();
    let mut store = SqliteRunStore::open(&db).unwrap();
    store
        .connection
        .pragma_update(None, "cache_size", 1)
        .unwrap();
    let hook = |at: &str| {
        if at == phase {
            std::fs::write(dir.join(format!("ready-{slot}")), at).unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    if mode.starts_with("execution-") {
        execution::process(&mut store, dir, &mode, &slot, &phase);
        return;
    }
    let s = scenario("review-approved");
    if mode == "start" {
        store.start_internal(&start(&s), hook).unwrap();
        return;
    }
    if mode == "apply" {
        store.apply_internal(&s.events[0], hook).unwrap();
        return;
    }
    let s = scenario(if mode.ends_with("-final") {
        "review-approved"
    } else {
        "parallel-all"
    });
    let snapshot = store.get(&s.run_id).unwrap();
    let e = if mode.starts_with("cancel") {
        event(&snapshot, "cancel-race", EventKind::Cancel)
    } else if mode.ends_with("-final") {
        s.events[1].clone()
    } else {
        s.events[0].clone()
    };
    std::fs::write(dir.join(format!("ready-{slot}")), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dir.join("go").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let result = store.apply(&e);
    std::fs::write(
        dir.join(format!("result-{slot}")),
        document(&result).unwrap(),
    )
    .unwrap();
}
#[test]
fn killed_processes_never_leave_partial_state_or_lose_committed_outbox() {
    for mode in ["start", "apply"] {
        for phase in [
            "before_transaction",
            "event_written",
            "state_written",
            "before_commit",
            "after_commit",
        ] {
            if mode == "start" && phase == "event_written" {
                continue;
            }
            let db = Db::new();
            let mut store = db.store();
            let s = scenario("review-approved");
            if mode == "apply" {
                store.start(&start(&s)).unwrap();
            }
            drop(store);
            let mut worker = child(&db, mode, "one", phase);
            wait_file(&db.dir.join("ready-one"), &mut worker);
            worker.kill().unwrap();
            assert!(!worker.wait().unwrap().success());
            let mut store = SqliteRunStore::open(&db.path).unwrap();
            let committed = phase == "after_commit";
            if mode == "start" && !committed {
                assert_eq!(store.get(&s.run_id).unwrap_err().code, ErrorCode::NotFound);
                assert!(store.list(None, 100).unwrap().items.is_empty());
                for table in [
                    "runs",
                    "heads",
                    "events",
                    "checkpoints",
                    "outbox",
                    "bundles",
                    "binding_locks",
                    "delivery_heads",
                    "receipts",
                ] {
                    let count: i64 = store
                        .connection
                        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                        .unwrap();
                    assert_eq!(count, 0, "orphan rows in {table} after {phase}");
                }
            } else {
                let snapshot = store.get(&s.run_id).unwrap();
                assert_eq!(
                    snapshot.revision,
                    if mode == "apply" && committed { 2 } else { 1 }
                );
                assert_eq!(
                    store.outbox(&s.run_id, 0, 100, true).unwrap().items.len(),
                    if mode == "apply" && committed { 3 } else { 1 }
                );
            }
            let retried = if mode == "start" {
                store.start(&start(&s)).unwrap()
            } else {
                store.apply(&s.events[0]).unwrap()
            };
            assert_eq!(retried.transition.duplicate, committed);
            assert_eq!(
                store.verify(&s.run_id).unwrap().revision,
                if mode == "start" { 1 } else { 2 }
            );
        }
    }
}
#[test]
fn independent_processes_deduplicate_events_and_arbitrate_cancel_by_cas() {
    for cancel in [false, true] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("parallel-all");
        store.start(&start(&s)).unwrap();
        drop(store);
        let mut a = child(&db, "complete", "a", "");
        let mut b = child(&db, if cancel { "cancel" } else { "complete" }, "b", "");
        wait_file(&db.dir.join("ready-a"), &mut a);
        wait_file(&db.dir.join("ready-b"), &mut b);
        std::fs::write(db.dir.join("go"), b"go").unwrap();
        assert!(a.wait().unwrap().success());
        assert!(b.wait().unwrap().success());
        let parse = |slot: &str| {
            workflow_worker::parse_message::<std::result::Result<Committed, Error>>(
                &std::fs::read(db.dir.join(format!("result-{slot}"))).unwrap(),
            )
            .unwrap()
        };
        let results = [parse("a"), parse("b")];
        if cancel {
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
            assert_eq!(
                results
                    .iter()
                    .find_map(|r| r.as_ref().err())
                    .unwrap()
                    .kernel_code,
                Some(workflow_kernel::ErrorCode::RevisionConflict)
            );
        } else {
            assert!(results.iter().all(|r| r.is_ok()));
            assert_eq!(
                results
                    .iter()
                    .filter(|r| r.as_ref().unwrap().transition.duplicate)
                    .count(),
                1
            );
        }
        let mut store = SqliteRunStore::open(&db.path).unwrap();
        assert_eq!(store.get(&s.run_id).unwrap().revision, 2);
        assert_eq!(store.history(&s.run_id, 0, 100).unwrap().items.len(), 1);
        store.verify(&s.run_id).unwrap();
    }
}

#[test]
fn definition_and_capability_versions_cannot_be_rebound_by_another_run() {
    let db = Db::new();
    let mut store = db.store();
    let mut request = start(&scenario("review-approved"));
    let first = store.start(&request).unwrap().snapshot;
    request.run_id = "second".into();
    let mut changed = request.clone();
    changed.bundle.capabilities[0].timeout_ms += 1;
    assert_eq!(
        store.start(&changed).unwrap_err().code,
        ErrorCode::BindingConflict
    );
    let mut changed = request.clone();
    changed.bundle.workflows[0].edges.reverse();
    assert_eq!(
        store.start(&changed).unwrap_err().code,
        ErrorCode::BindingConflict
    );
    assert_eq!(
        store.get(&request.run_id).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(store.get(&first.run_id).unwrap(), first);
    request.bundle.workflows[0].nodes.reverse();
    assert_eq!(store.start(&request).unwrap().snapshot.revision, 1);
}

#[test]
fn final_completion_and_cancel_race_preserves_the_winner_and_late_observation() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("review-approved");
    store.start(&start(&s)).unwrap();
    store.apply(&s.events[0]).unwrap();
    drop(store);
    let mut a = child(&db, "complete-final", "a", "");
    let mut b = child(&db, "cancel-final", "b", "");
    wait_file(&db.dir.join("ready-a"), &mut a);
    wait_file(&db.dir.join("ready-b"), &mut b);
    std::fs::write(db.dir.join("go"), b"go").unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    let snapshot = store.get(&s.run_id).unwrap();
    assert_eq!(snapshot.revision, 3);
    let complete: std::result::Result<Committed, Error> =
        workflow_worker::parse_message(&std::fs::read(db.dir.join("result-a")).unwrap()).unwrap();
    let cancel: std::result::Result<Committed, Error> =
        workflow_worker::parse_message(&std::fs::read(db.dir.join("result-b")).unwrap()).unwrap();
    assert_ne!(complete.is_ok(), cancel.is_ok());
    if complete.is_ok() {
        assert_eq!(snapshot.status, RunStatus::Succeeded);
        assert_eq!(
            cancel.unwrap_err().kernel_code,
            Some(workflow_kernel::ErrorCode::RevisionConflict)
        );
    } else {
        assert_eq!(snapshot.status, RunStatus::Cancelling);
        let mut late = s.events[1].clone();
        late.event_id = "observed-after-cancel".into();
        late.expected_revision = 3;
        let after = store.apply(&late).unwrap();
        assert_eq!(after.snapshot.status, RunStatus::Cancelled);
        assert_eq!(
            store.history(&s.run_id, 3, 100).unwrap().items[0].event,
            late
        );
    }
    store.verify(&s.run_id).unwrap();
}

mod execution;
