use super::*;
use std::cell::{Cell, RefCell};
use workflow_worker::{AdapterOutcome, Clock, PROTOCOL_VERSION, WorkResult};
struct Time(Cell<u64>);
impl Time {
    fn new(now: u64) -> Self {
        Self(Cell::new(now))
    }
    fn set(&self, now: u64) {
        self.0.set(now);
    }
}
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0.get())
    }
}
fn acquire(store: &mut SqliteRunStore, id: &str, owner: &str, clock: &dyn Clock) -> Lease {
    store
        .acquire(
            &LeaseRequest {
                run_id: id.into(),
                owner: owner.into(),
                acquisition_id: format!("claim-{owner}"),
                ttl_ms: 100,
            },
            clock,
        )
        .unwrap()
}
fn claimed(store: &mut SqliteRunStore, lease: &Lease, clock: &dyn Clock) -> PreparedTask {
    let Claimed::Task { attempt } = store.claim_next(lease, clock).unwrap() else {
        panic!("expected task")
    };
    *attempt
}
fn success(attempt: &PreparedTask, at: u64) -> WorkResult {
    WorkResult {
        protocol_version: PROTOCOL_VERSION,
        request_digest: digest(&attempt.request).unwrap(),
        completed_at_unix_ms: at,
        outcome: AdapterOutcome::Succeeded {
            outputs: Values::new(),
            evidence: vec![],
        },
    }
}
#[test]
fn expired_owner_is_fenced_and_only_current_readonly_attempt_can_commit() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let clock = Time::new(1000);
    let first = acquire(&mut store, &s.run_id, "one", &clock);
    let old = claimed(&mut store, &first, &clock);
    assert_eq!(
        store.claim_next(&first, &clock).unwrap_err().code,
        ErrorCode::AttemptInProgress
    );
    let request = LeaseRequest {
        run_id: s.run_id.clone(),
        owner: "two".into(),
        acquisition_id: "claim-two".into(),
        ttl_ms: 100,
    };
    clock.set(1099);
    assert_eq!(
        store.acquire(&request, &clock).unwrap_err().code,
        ErrorCode::LeaseBusy
    );
    drop(store);
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    clock.set(1100);
    let second = store.acquire(&request, &clock).unwrap();
    assert_eq!(second.epoch, first.epoch + 1);
    assert_eq!(
        store
            .finish_task(&first, &old.attempt_id, &success(&old, 1001), &clock)
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    let new = claimed(&mut store, &second, &clock);
    assert_eq!(new.number, 2);
    assert_eq!(new.command_id, old.command_id);
    assert_ne!(new.attempt_id, old.attempt_id);
    let result = success(&new, 1100);
    let committed = store
        .finish_task(&second, &new.attempt_id, &result, &clock)
        .unwrap();
    assert_eq!(committed.snapshot.revision, 2);
    clock.set(1201);
    let duplicate = store
        .finish_task(&second, &new.attempt_id, &result, &clock)
        .unwrap();
    assert!(duplicate.transition.duplicate && duplicate.transition.commands.is_empty());
    let mut changed = result;
    changed.completed_at_unix_ms += 1;
    assert_eq!(
        store
            .finish_task(&second, &new.attempt_id, &changed, &clock)
            .unwrap_err()
            .code,
        ErrorCode::ReceiptConflict
    );
    assert_eq!(store.history(&s.run_id, 0, 100).unwrap().items.len(), 1);
    assert_eq!(
        store.outbox(&s.run_id, 0, 100, true).unwrap().items.len(),
        1
    );
    store.verify(&s.run_id).unwrap();
}
#[test]
fn renewal_preserves_epoch_and_rejects_rollback_or_old_tokens() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let clock = Time::new(1000);
    let first = acquire(&mut store, &s.run_id, "one", &clock);
    clock.set(999);
    assert_eq!(
        store.claim_next(&first, &clock).unwrap_err().code,
        ErrorCode::LeaseConflict
    );
    clock.set(1050);
    let renewed = store.renew(&first, 100, &clock).unwrap();
    assert_eq!(renewed.epoch, first.epoch);
    assert_eq!(renewed.expires_at_unix_ms, 1150);
    assert_eq!(
        store.claim_next(&first, &clock).unwrap_err().code,
        ErrorCode::LeaseConflict
    );
    store.release(&renewed, &clock).unwrap();
    assert_eq!(
        store.claim_next(&renewed, &clock).unwrap_err().code,
        ErrorCode::LeaseConflict
    );
    let second = acquire(&mut store, &s.run_id, "two", &clock);
    assert_eq!(second.epoch, 2);
    store.verify(&s.run_id).unwrap();
}
struct Jump(RefCell<std::collections::VecDeque<u64>>);
impl Clock for Jump {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        let mut q = self.0.borrow_mut();
        Ok(if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            q[0]
        })
    }
}
#[test]
fn expiry_or_clock_reversal_before_commit_rolls_back_result_event_and_receipt() {
    for last in [1100, 999] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("parallel-all");
        store.start(&start(&s)).unwrap();
        let clock = Time::new(1000);
        let lease = acquire(&mut store, &s.run_id, "one", &clock);
        let p = claimed(&mut store, &lease, &clock);
        let before = store.get(&s.run_id).unwrap();
        let jump = Jump(RefCell::new([1001, last].into()));
        assert_eq!(
            store
                .finish_task(&lease, &p.attempt_id, &success(&p, 1001), &jump)
                .unwrap_err()
                .code,
            ErrorCode::LeaseConflict
        );
        assert_eq!(store.get(&s.run_id).unwrap(), before);
        assert!(store.history(&s.run_id, 0, 100).unwrap().items.is_empty());
        assert_eq!(
            store.outbox(&s.run_id, 0, 100, true).unwrap().items.len(),
            2
        );
        assert_eq!(
            store
                .execution_history(&s.run_id, 0, 100)
                .unwrap()
                .items
                .len(),
            2
        );
    }
}
#[test]
fn cancel_races_preserve_real_result_and_suppress_unstarted_readonly_calls() {
    for prepare in [false, true] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("parallel-all");
        let snapshot = store.start(&start(&s)).unwrap().snapshot;
        let clock = Time::new(1000);
        let lease = acquire(&mut store, &s.run_id, "one", &clock);
        let p = prepare.then(|| claimed(&mut store, &lease, &clock));
        store
            .apply(&event(&snapshot, "cancel", EventKind::Cancel))
            .unwrap();
        clock.set(1001);
        if let Some(p) = p {
            store
                .finish_task(&lease, &p.attempt_id, &success(&p, 1001), &clock)
                .unwrap();
        }
        for _ in 0..5 {
            if matches!(store.claim_next(&lease, &clock).unwrap(), Claimed::Idle) {
                break;
            }
        }
        assert_eq!(store.get(&s.run_id).unwrap().status, RunStatus::Cancelled);
        assert!(
            store
                .outbox(&s.run_id, 0, 100, true)
                .unwrap()
                .items
                .is_empty()
        );
        store.verify(&s.run_id).unwrap();
    }
}
#[test]
fn retries_are_bounded_and_uncertain_or_write_commands_are_never_auto_executed() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let clock = Time::new(1000);
    let lease = acquire(&mut store, &s.run_id, "one", &clock);
    for n in 1..=3 {
        let p = claimed(&mut store, &lease, &clock);
        assert_eq!(p.number, n);
        store
            .fail_task(
                &lease,
                &p.attempt_id,
                &workflow_worker::Error::new(
                    workflow_worker::ErrorCode::MissingCapability,
                    "adapter unavailable",
                ),
                &clock,
            )
            .unwrap();
    }
    assert_eq!(
        store.claim_next(&lease, &clock).unwrap_err().code,
        ErrorCode::AttemptBudget
    );
    let db = Db::new();
    let mut store = db.store();
    let mut req = start(&s);
    for c in &mut req.bundle.capabilities {
        c.effects = workflow_worker::EffectContract::Write {
            idempotency: workflow_worker::Idempotency::None,
            query: None,
            compensation: None,
        };
    }
    let snap = store.start(&req).unwrap().snapshot;
    let lease = acquire(&mut store, &s.run_id, "one", &clock);
    assert_eq!(
        store.claim_next(&lease, &clock).unwrap_err().code,
        ErrorCode::UnsupportedEffect
    );
    assert_eq!(store.get(&s.run_id).unwrap(), snap);
    let db = Db::new();
    let mut store = db.store();
    let snap = store.start(&start(&s)).unwrap().snapshot;
    let lease = acquire(&mut store, &s.run_id, "one", &clock);
    let first = store.outbox(&s.run_id, 0, 1, true).unwrap().items.remove(0);
    let Command::ExecuteTask { instance_id, .. } = first.command else {
        panic!()
    };
    store
        .apply(&event(
            &snap,
            "unknown",
            EventKind::TaskCompleted {
                instance_id,
                result: TaskResult::Uncertain {
                    reason: "missing result".into(),
                },
            },
        ))
        .unwrap();
    clock.set(1001);
    assert_eq!(
        store.claim_next(&lease, &clock).unwrap_err().code,
        ErrorCode::ManualReconciliation
    );
}
#[test]
fn due_waits_are_advanced_once_and_timer_registration_is_durable() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("review-approved");
    store.start(&start(&s)).unwrap();
    let clock = Time::new(1000);
    let lease = acquire(&mut store, &s.run_id, "one", &clock);
    assert!(matches!(
        store.claim_next(&lease, &clock).unwrap(),
        Claimed::Handled { .. }
    ));
    assert!(store.tick_due(&lease, &clock).unwrap().is_none());
    store.release(&lease, &clock).unwrap();
    drop(store);
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    clock.set(86401000);
    let lease = acquire(&mut store, &s.run_id, "two", &clock);
    let timed = store.tick_due(&lease, &clock).unwrap().unwrap();
    assert_eq!(timed.snapshot.status, RunStatus::Failed);
    assert!(store.tick_due(&lease, &clock).unwrap().is_none());
    store.verify(&s.run_id).unwrap();
}
#[test]
fn explicit_v1_migration_preserves_runs_and_rolls_back_on_corruption() {
    for corrupt_old in [false, true] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("parallel-all");
        let before = store.start(&start(&s)).unwrap().snapshot;
        store
            .connection
            .execute_batch(
                "DROP TABLE execution_events; DROP TABLE execution_heads; PRAGMA user_version=1;",
            )
            .unwrap();
        if corrupt_old {
            store
                .connection
                .execute_batch("UPDATE heads SET revision=revision+1")
                .unwrap();
        }
        drop(store);
        assert!(matches!(
            SqliteRunStore::open(&db.path),
            Err(Error {
                code: ErrorCode::UnsupportedStorage,
                ..
            })
        ));
        if corrupt_old {
            assert!(matches!(
                SqliteRunStore::migrate(&db.path),
                Err(Error {
                    code: ErrorCode::CorruptStorage,
                    ..
                })
            ));
            let c = Connection::open(&db.path).unwrap();
            let version: i64 = c
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(version, 1);
            let count: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'execution_%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0);
        } else {
            let mut store = SqliteRunStore::migrate(&db.path).unwrap();
            assert_eq!(store.get(&s.run_id).unwrap(), before);
            drop(store);
            let mut store = SqliteRunStore::migrate(&db.path).unwrap();
            assert_eq!(store.get(&s.run_id).unwrap(), before);
        }
    }
}
#[test]
fn deleted_execution_tail_is_detected_before_a_new_owner_can_be_granted() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    let clock = Time::new(1000);
    let lease = acquire(&mut store, &s.run_id, "one", &clock);
    claimed(&mut store, &lease, &clock);
    store.connection.execute_batch("DROP TRIGGER immutable_execution_delete; DELETE FROM execution_events WHERE sequence=2;").unwrap();
    assert_eq!(
        store.verify(&s.run_id).unwrap_err().code,
        ErrorCode::CorruptStorage
    );
    clock.set(1200);
    assert_eq!(
        store
            .acquire(
                &LeaseRequest {
                    run_id: s.run_id,
                    owner: "two".into(),
                    acquisition_id: "new".into(),
                    ttl_ms: 100
                },
                &clock
            )
            .unwrap_err()
            .code,
        ErrorCode::CorruptStorage
    );
}

pub(super) fn process(
    store: &mut SqliteRunStore,
    dir: &std::path::Path,
    mode: &str,
    slot: &str,
    phase: &str,
) {
    let s = scenario("parallel-all");
    let clock = Time::new(1000);
    if mode == "execution-race" {
        std::fs::write(dir.join(format!("ready-{slot}")), "ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !dir.join("go").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let result = store.acquire(
            &LeaseRequest {
                run_id: s.run_id,
                owner: slot.into(),
                acquisition_id: slot.into(),
                ttl_ms: 100,
            },
            &clock,
        );
        std::fs::write(
            dir.join(format!("result-{slot}")),
            document(&result).unwrap(),
        )
        .unwrap();
        return;
    }
    let lease = acquire(store, &s.run_id, slot, &clock);
    let task = claimed(store, &lease, &clock);
    let result = success(&task, 1000);
    std::fs::write(
        dir.join("attempt.json"),
        document(&(lease.clone(), task.clone(), result.clone())).unwrap(),
    )
    .unwrap();
    store
        .finish_task_internal(&lease, &task.attempt_id, &result, &clock, |at| {
            if at == phase {
                std::fs::write(dir.join(format!("ready-{slot}")), at).unwrap();
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        })
        .unwrap();
}
#[test]
fn independent_processes_grant_exactly_one_owner_and_persist_the_fence() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    store.start(&start(&s)).unwrap();
    drop(store);
    let mut one = child(&db, "execution-race", "one", "");
    let mut two = child(&db, "execution-race", "two", "");
    wait_file(&db.dir.join("ready-one"), &mut one);
    wait_file(&db.dir.join("ready-two"), &mut two);
    std::fs::write(db.dir.join("go"), "go").unwrap();
    assert!(one.wait().unwrap().success());
    assert!(two.wait().unwrap().success());
    let outcomes: Vec<Result<Lease>> = ["one", "two"]
        .iter()
        .map(|slot| {
            workflow_worker::parse_message(
                &std::fs::read(db.dir.join(format!("result-{slot}"))).unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes.iter().find_map(|r| r.as_ref().err()).unwrap().code,
        ErrorCode::LeaseBusy
    );
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    let next = acquire(&mut store, &s.run_id, "three", &Time::new(1100));
    assert_eq!(next.epoch, 2);
    store.verify(&s.run_id).unwrap();
}
#[test]
fn killed_result_writers_recover_an_orphan_or_one_complete_result_transaction() {
    for phase in [
        "before_transaction",
        "event_written",
        "state_written",
        "execution_written",
        "before_commit",
        "after_commit",
    ] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("parallel-all");
        store.start(&start(&s)).unwrap();
        drop(store);
        let mut worker = child(&db, "execution-finish", "one", phase);
        wait_file(&db.dir.join("ready-one"), &mut worker);
        worker.kill().unwrap();
        assert!(!worker.wait().unwrap().success());
        let (old, p, result): (Lease, PreparedTask, WorkResult) =
            workflow_worker::parse_message(&std::fs::read(db.dir.join("attempt.json")).unwrap())
                .unwrap();
        let mut store = SqliteRunStore::open(&db.path).unwrap();
        let committed = phase == "after_commit";
        assert_eq!(
            store.get(&s.run_id).unwrap().revision,
            if committed { 2 } else { 1 }
        );
        assert_eq!(
            store.history(&s.run_id, 0, 100).unwrap().items.len(),
            usize::from(committed)
        );
        assert_eq!(
            store.outbox(&s.run_id, 0, 100, true).unwrap().items.len(),
            if committed { 1 } else { 2 }
        );
        assert_eq!(
            store
                .execution_history(&s.run_id, 0, 100)
                .unwrap()
                .items
                .len(),
            if committed { 3 } else { 2 }
        );
        let clock = Time::new(1100);
        if committed {
            assert!(
                store
                    .finish_task(&old, &p.attempt_id, &result, &clock)
                    .unwrap()
                    .transition
                    .duplicate
            );
        }
        let current = acquire(&mut store, &s.run_id, "two", &clock);
        let retry = claimed(&mut store, &current, &clock);
        if committed {
            assert_ne!(retry.command_id, p.command_id);
            assert_eq!(retry.number, 1);
        } else {
            assert_eq!(retry.command_id, p.command_id);
            assert_eq!(retry.number, 2);
            assert_eq!(
                store
                    .finish_task(&old, &p.attempt_id, &result, &clock)
                    .unwrap_err()
                    .code,
                ErrorCode::LeaseConflict
            );
        }
        store
            .finish_task(&current, &retry.attempt_id, &success(&retry, 1100), &clock)
            .unwrap();
        store.verify(&s.run_id).unwrap();
    }
}
#[test]
fn foreign_result_identity_and_unverified_artifacts_cannot_commit() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("parallel-all");
    let before = store.start(&start(&s)).unwrap().snapshot;
    let clock = Time::new(1000);
    let lease = acquire(&mut store, &s.run_id, "one", &clock);
    let p = claimed(&mut store, &lease, &clock);
    let mut forged = success(&p, 1000);
    forged.request_digest = format!("sha256:{}", "0".repeat(64));
    assert!(
        store
            .finish_task(&lease, &p.attempt_id, &forged, &clock)
            .is_err()
    );
    let mut artifact = success(&p, 1000);
    if let AdapterOutcome::Succeeded { evidence, .. } = &mut artifact.outcome {
        evidence.push(workflow_worker::EvidenceRef {
            artifact_id: "report".into(),
            digest: format!("sha256:{}", "0".repeat(64)),
        });
    }
    assert_eq!(
        store
            .finish_task(&lease, &p.attempt_id, &artifact, &clock)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(store.get(&s.run_id).unwrap(), before);
    assert!(store.history(&s.run_id, 0, 100).unwrap().items.is_empty());
    assert_eq!(
        store
            .execution_history(&s.run_id, 0, 100)
            .unwrap()
            .items
            .len(),
        2
    );
}
