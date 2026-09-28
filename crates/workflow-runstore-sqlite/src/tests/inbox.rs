use super::*;
use workflow_kernel::{SignalDecision, SignalMessage, SignalRejection, SignalStatus};
struct Time(u64);
impl workflow_worker::Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0)
    }
}

struct Jump(std::cell::RefCell<std::collections::VecDeque<u64>>);
impl workflow_worker::Clock for Jump {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        let mut values = self.0.borrow_mut();
        Ok(if values.len() > 1 {
            values.pop_front().unwrap()
        } else {
            values[0]
        })
    }
}

#[test]
fn inbox_expiry_or_clock_reversal_before_commit_rolls_back_receipt_state_and_outbox() {
    for (expires, commit_at) in [(2000, 2000), (90_000_000, 86_401_000), (2000, 999)] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("review-approved");
        let before = store.start(&start(&s)).unwrap().snapshot;
        let mut request = submission(&mut store);
        request.message.expires_at_unix_ms = expires;
        let clock = Jump(std::cell::RefCell::new([1001, commit_at].into()));
        assert_eq!(
            store.receive_signal(&request, &clock).unwrap_err().code,
            ErrorCode::TransitionRejected
        );
        assert_eq!(store.get(&s.run_id).unwrap(), before);
        assert!(store.inbox(&s.run_id, 0, 100).unwrap().items.is_empty());
        assert!(store.history(&s.run_id, 0, 100).unwrap().items.is_empty());
        assert_eq!(
            store.outbox(&s.run_id, 0, 100, false).unwrap().items.len(),
            1
        );
        if commit_at >= 1001 {
            let receipt = store.receive_signal(&request, &Time(commit_at)).unwrap();
            assert!(!receipt.duplicate);
            assert!(matches!(
                receipt.entry.status,
                SignalStatus::Rejected { .. }
            ));
        }
        store.verify(&s.run_id).unwrap();
    }
}

#[test]
fn early_callback_expiring_during_task_settlement_cannot_commit_approval_and_retry_keeps_real_result()
 {
    let db = Db::new();
    let mut store = db.store();
    let mut request = start(&scenario("review-approved"));
    let workflow = &mut request.bundle.workflows[0];
    let mut prepare = workflow
        .nodes
        .iter()
        .find(|n| n.id == "implement")
        .unwrap()
        .clone();
    prepare.id = "prepare".into();
    workflow.nodes.push(prepare);
    workflow.entry = "prepare".into();
    workflow.edges.push(workflow_ir::Edge {
        id: "prepared".into(),
        from: "prepare".into(),
        to: "review".into(),
        route: workflow_ir::Route::Next,
    });
    let snapshot = store.start(&request).unwrap().snapshot;
    let target = workflow_kernel::WaitTarget {
        instance_id: snapshot.frames[&1].nodes["review"].instance_id,
        definition_digest: snapshot.frames[&1].definition_digest.clone(),
        input_digest: digest(&Values::new()).unwrap(),
        event: "design-review".into(),
    };
    let message = SignalSubmission {
        schema_version: 1,
        run_id: request.run_id.clone(),
        run_digest: snapshot.run_digest.clone(),
        message: SignalMessage {
            schema_version: 1,
            message_id: "early".into(),
            correlation_id: workflow_kernel::signal_correlation(&snapshot.run_digest, &target)
                .unwrap(),
            target,
            source: "test-host".into(),
            decision: SignalDecision::Approve,
            reason: "fixture".into(),
            outputs: Values::new(),
            expires_at_unix_ms: 1003,
        },
    };
    assert_eq!(
        store
            .receive_signal(&message, &Time(1001))
            .unwrap()
            .entry
            .status,
        SignalStatus::Pending
    );
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: request.run_id.clone(),
                owner: "owner".into(),
                acquisition_id: "lease".into(),
                ttl_ms: 100,
            },
            &Time(1001),
        )
        .unwrap();
    let Claimed::Task { attempt } = store.claim_next(&lease, &Time(1001)).unwrap() else {
        panic!("task expected")
    };
    let result = workflow_worker::WorkResult {
        protocol_version: workflow_worker::PROTOCOL_VERSION,
        request_digest: digest(&attempt.request).unwrap(),
        completed_at_unix_ms: 1002,
        outcome: workflow_worker::AdapterOutcome::Succeeded {
            outputs: Values::new(),
            evidence: vec![],
        },
        model_record: None,
    };
    let before = store.get(&request.run_id).unwrap();
    let clock = Jump(std::cell::RefCell::new([1002, 1003].into()));
    assert_eq!(
        store
            .finish_task(&lease, &attempt.attempt_id, &result, &clock)
            .unwrap_err()
            .code,
        ErrorCode::TransitionRejected
    );
    assert_eq!(store.get(&request.run_id).unwrap(), before);
    assert_eq!(
        store
            .execution_history(&request.run_id, 0, 100)
            .unwrap()
            .items
            .len(),
        2
    );
    let settled = store
        .finish_task(&lease, &attempt.attempt_id, &result, &Time(1003))
        .unwrap();
    assert!(matches!(
        settled.snapshot.inbox["early"].status,
        SignalStatus::Rejected {
            reason: SignalRejection::Expired,
            ..
        }
    ));
    assert_eq!(
        settled.snapshot.frames[&1].nodes["prepare"].state,
        workflow_kernel::NodeState::Succeeded
    );
    assert_eq!(
        settled.snapshot.frames[&1].nodes["implement"].state,
        workflow_kernel::NodeState::Pending
    );
    store.verify(&request.run_id).unwrap();
}
fn submission(store: &mut SqliteRunStore) -> SignalSubmission {
    let id = scenario("review-approved").run_id;
    let snapshot = store.get(&id).unwrap();
    let wait = store.waits(&id, 0, 100).unwrap().items.remove(0);
    SignalSubmission {
        schema_version: 1,
        run_id: id,
        run_digest: snapshot.run_digest,
        message: SignalMessage {
            schema_version: 1,
            message_id: "provider-event-1".into(),
            correlation_id: wait.correlation_id,
            target: wait.target,
            source: "trusted-ci".into(),
            decision: SignalDecision::Approve,
            reason: "review fixture".into(),
            outputs: Values::new(),
            expires_at_unix_ms: 2000,
        },
    }
}

#[test]
fn inbox_retries_report_current_receipt_without_a_new_transition_and_conflicts_are_refused() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("review-approved");
    store.start(&start(&s)).unwrap();
    let request = submission(&mut store);
    let first = store.receive_signal(&request, &Time(1001)).unwrap();
    assert!(!first.duplicate);
    assert!(matches!(
        first.entry.status,
        SignalStatus::Applied { revision: 2, .. }
    ));
    assert!(store.waits(&s.run_id, 0, 100).unwrap().items.is_empty());
    drop(store);
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    let again = store.receive_signal(&request, &Time(3000)).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.entry, first.entry);
    let mut conflict = request.clone();
    conflict.message.reason = "changed content".into();
    assert_eq!(
        store
            .receive_signal(&conflict, &Time(3000))
            .unwrap_err()
            .code,
        ErrorCode::SignalConflict
    );
    let mut foreign = request.clone();
    foreign.run_digest = digest(&"other run").unwrap();
    assert_eq!(
        store
            .receive_signal(&foreign, &Time(1001))
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        store.inbox(&s.run_id, 0, 1).unwrap().items,
        vec![first.entry]
    );
    assert_eq!(store.history(&s.run_id, 0, 100).unwrap().items.len(), 1);
    store.verify(&s.run_id).unwrap();
}

#[test]
fn paused_inbox_persists_across_sessions_and_cancelled_or_expired_receipts_never_advance() {
    for cancel in [false, true] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("review-approved");
        let snapshot = store.start(&start(&s)).unwrap().snapshot;
        let request = submission(&mut store);
        store
            .apply(&event(
                &snapshot,
                "pause",
                EventKind::Pause {
                    reason: "maintenance".into(),
                },
            ))
            .unwrap();
        assert!(store.waits(&s.run_id, 0, 100).unwrap().items[0].paused);
        let first = store.receive_signal(&request, &Time(1001)).unwrap();
        assert_eq!(first.entry.status, SignalStatus::Pending);
        drop(store);
        let mut store = SqliteRunStore::open(&db.path).unwrap();
        assert_eq!(
            store.inbox(&s.run_id, 0, 100).unwrap().items[0].status,
            SignalStatus::Pending
        );
        let snapshot = store.get(&s.run_id).unwrap();
        let kind = if cancel {
            EventKind::Cancel
        } else {
            EventKind::Resume {
                reason: "ready".into(),
            }
        };
        let mut control = event(&snapshot, "control", kind);
        control.at_unix_ms = 2000;
        store.apply(&control).unwrap();
        let repeated = store.receive_signal(&request, &Time(2000)).unwrap();
        assert!(repeated.duplicate);
        assert!(
            matches!(repeated.entry.status, SignalStatus::Rejected { reason, .. } if reason == if cancel { SignalRejection::RunCancelled } else { SignalRejection::Expired })
        );
        let mut late = request;
        late.message.message_id = "provider-event-2".into();
        let receipt = store.receive_signal(&late, &Time(2001)).unwrap();
        assert!(matches!(
            receipt.entry.status,
            SignalStatus::Rejected { .. }
        ));
        let page = store.inbox(&s.run_id, 0, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            store
                .inbox(&s.run_id, page.next_cursor.unwrap(), 1)
                .unwrap()
                .items[0],
            receipt.entry
        );
        store.verify(&s.run_id).unwrap();
    }
}

pub(super) fn process(
    store: &mut SqliteRunStore,
    dir: &std::path::Path,
    mode: &str,
    slot: &str,
    phase: &str,
) {
    let request = submission(store);
    if mode.starts_with("inbox-race") {
        let snapshot = store.get(&request.run_id).unwrap();
        std::fs::write(dir.join(format!("ready-{slot}")), "ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !dir.join("go").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        if mode == "inbox-race-cancel" && slot == "cancel" {
            let cancelled = store.apply(&event(&snapshot, "cancel", EventKind::Cancel));
            std::fs::write(
                dir.join(format!("result-{slot}")),
                document(&cancelled).unwrap(),
            )
            .unwrap();
            return;
        }
        let receipt = store.receive_signal(&request, &Time(1001)).unwrap();
        std::fs::write(
            dir.join(format!("result-{slot}")),
            document(&receipt).unwrap(),
        )
        .unwrap();
    } else {
        store
            .receive_signal_internal(&request, &Time(1001), |at| {
                if at == phase {
                    std::fs::write(dir.join(format!("ready-{slot}")), at).unwrap();
                    loop {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            })
            .unwrap();
    }
}

#[test]
fn cancellation_races_record_a_real_approval_or_a_cancelled_receipt_without_double_progress() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("review-approved");
    store.start(&start(&s)).unwrap();
    drop(store);
    let mut one = child(&db, "inbox-race-cancel", "signal", "");
    let mut two = child(&db, "inbox-race-cancel", "cancel", "");
    wait_file(&db.dir.join("ready-signal"), &mut one);
    wait_file(&db.dir.join("ready-cancel"), &mut two);
    std::fs::write(db.dir.join("go"), "go").unwrap();
    assert!(one.wait().unwrap().success());
    assert!(two.wait().unwrap().success());
    let cancel: Result<Committed> =
        workflow_worker::parse_message(&std::fs::read(db.dir.join("result-cancel")).unwrap())
            .unwrap();
    let signal: SignalReceipt =
        workflow_worker::parse_message(&std::fs::read(db.dir.join("result-signal")).unwrap())
            .unwrap();
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    if let Err(error) = cancel {
        assert!(matches!(signal.entry.status, SignalStatus::Applied { .. }));
        assert_eq!(
            error.kernel_code,
            Some(workflow_kernel::ErrorCode::RevisionConflict)
        );
    } else {
        assert!(matches!(
            signal.entry.status,
            SignalStatus::Rejected {
                reason: SignalRejection::RunCancelled,
                ..
            }
        ));
        assert_eq!(store.get(&s.run_id).unwrap().status, RunStatus::Cancelled);
    }
    assert_eq!(store.inbox(&s.run_id, 0, 100).unwrap().items.len(), 1);
    store.verify(&s.run_id).unwrap();
}

#[test]
fn altered_inbox_snapshot_is_detected_by_full_journal_and_checkpoint_replay() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("review-approved");
    store.start(&start(&s)).unwrap();
    let request = submission(&mut store);
    store.receive_signal(&request, &Time(1001)).unwrap();
    let mut snapshot = store.get(&s.run_id).unwrap();
    snapshot.inbox.clear();
    store
        .connection
        .execute_batch("DROP TRIGGER monotonic_heads")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE heads SET snapshot=?2,state_digest=?3 WHERE run_id=?1",
            rusqlite::params![
                s.run_id,
                document(&snapshot).unwrap(),
                digest(&snapshot).unwrap()
            ],
        )
        .unwrap();
    assert_eq!(
        store.verify(&s.run_id).unwrap_err().code,
        ErrorCode::CorruptStorage
    );
    assert_eq!(
        store
            .receive_signal(&request, &Time(1001))
            .unwrap_err()
            .code,
        ErrorCode::CorruptStorage
    );
}

#[test]
fn competing_inbox_processes_acknowledge_one_receipt_and_one_duplicate() {
    let db = Db::new();
    let mut store = db.store();
    let s = scenario("review-approved");
    store.start(&start(&s)).unwrap();
    drop(store);
    let mut one = child(&db, "inbox-race", "one", "");
    let mut two = child(&db, "inbox-race", "two", "");
    wait_file(&db.dir.join("ready-one"), &mut one);
    wait_file(&db.dir.join("ready-two"), &mut two);
    std::fs::write(db.dir.join("go"), "go").unwrap();
    assert!(one.wait().unwrap().success());
    assert!(two.wait().unwrap().success());
    let receipts: Vec<SignalReceipt> = ["one", "two"]
        .iter()
        .map(|slot| {
            workflow_worker::parse_message(
                &std::fs::read(db.dir.join(format!("result-{slot}"))).unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_ne!(receipts[0].duplicate, receipts[1].duplicate);
    assert_eq!(receipts[0].entry, receipts[1].entry);
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    assert_eq!(store.get(&s.run_id).unwrap().revision, 2);
    assert_eq!(store.inbox(&s.run_id, 0, 100).unwrap().items.len(), 1);
    store.verify(&s.run_id).unwrap();
}

#[test]
fn killed_inbox_writers_recover_the_entire_signal_transition_or_nothing() {
    for phase in [
        "before_transaction",
        "event_written",
        "state_written",
        "before_commit",
        "after_commit",
    ] {
        let db = Db::new();
        let mut store = db.store();
        let s = scenario("review-approved");
        store.start(&start(&s)).unwrap();
        let request = submission(&mut store);
        drop(store);
        let mut writer = child(&db, "inbox-crash", "one", phase);
        wait_file(&db.dir.join("ready-one"), &mut writer);
        writer.kill().unwrap();
        assert!(!writer.wait().unwrap().success());
        let mut store = SqliteRunStore::open(&db.path).unwrap();
        let committed = phase == "after_commit";
        assert_eq!(
            store.inbox(&s.run_id, 0, 100).unwrap().items.len(),
            usize::from(committed)
        );
        assert_eq!(
            store.history(&s.run_id, 0, 100).unwrap().items.len(),
            usize::from(committed)
        );
        assert_eq!(
            store.get(&s.run_id).unwrap().revision,
            if committed { 2 } else { 1 }
        );
        assert_eq!(
            store.outbox(&s.run_id, 0, 100, true).unwrap().items.len(),
            if committed { 3 } else { 1 }
        );
        assert_eq!(
            store
                .receive_signal(&request, &Time(1001))
                .unwrap()
                .duplicate,
            committed
        );
        store.verify(&s.run_id).unwrap();
    }
}
