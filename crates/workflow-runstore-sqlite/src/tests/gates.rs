use super::*;
use std::cell::Cell;
use workflow_artifact_local::LocalArtifactStore;
use workflow_artifacts::{AccessScope, ArtifactReader, ArtifactStore, PublishSpec, Retention};
use workflow_kernel::{GateContext, NodeState};
use workflow_worker::{AdapterOutcome, Clock, WorkResult};
struct Time(Cell<u64>);
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0.get())
    }
}
struct Fixture {
    db: Db,
    start: StartRun,
    lease: Lease,
    attempt: PreparedTask,
    result: WorkResult,
}
impl Fixture {
    fn root(&self) -> PathBuf {
        self.db.dir.join("artifacts")
    }
    fn store(&self) -> SqliteRunStore {
        SqliteRunStore::open(&self.db.path)
            .unwrap()
            .with_artifacts(Box::new(LocalArtifactStore::open(self.root()).unwrap()))
    }
    fn finish(&self) {
        self.store()
            .finish_task(
                &self.lease,
                &self.attempt.attempt_id,
                &self.result,
                &Time(Cell::new(1000)),
            )
            .unwrap();
    }
    fn snapshot(&self) -> Snapshot {
        self.store().get(&self.start.run_id).unwrap()
    }
    fn claim(&self) -> Claimed {
        self.store()
            .claim_next(&self.lease, &Time(Cell::new(1000)))
            .unwrap()
    }
}
fn fixture(valid: bool, attach: bool, max_age: u64) -> Fixture {
    fixture_with_gates(valid, attach, max_age, true)
}
fn fixture_with_gates(valid: bool, attach: bool, max_age: u64, gated: bool) -> Fixture {
    let db = Db::new();
    let mut artifacts = LocalArtifactStore::create(db.dir.join("artifacts")).unwrap();
    let mut start: StartRun = workflow_worker::parse_message(
        &std::fs::read(base().join("examples/gates/guarded-start.json")).unwrap(),
    )
    .unwrap();
    for g in &mut start.bundle.postconditions {
        g.policy.requirements[0].max_age_ms = max_age;
    }
    if !valid {
        let invalid: StartRun = workflow_worker::parse_message(
            &std::fs::read(base().join("examples/execution/invalid-start.json")).unwrap(),
        )
        .unwrap();
        start.inputs = invalid.inputs;
    }
    let gate = start.bundle.postconditions[0].clone();
    if !gated {
        start.bundle.postconditions.clear();
    }
    let mut store = db.store();
    store.start(&start).unwrap();
    let clock = Time(Cell::new(1000));
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: start.run_id.clone(),
                owner: "gate-owner".into(),
                acquisition_id: "first".into(),
                ttl_ms: 10000,
            },
            &clock,
        )
        .unwrap();
    let Claimed::Task { attempt } = store.claim_next(&lease, &clock).unwrap() else {
        panic!("task")
    };
    let mut result = workflow_builtin_capabilities::worker()
        .unwrap()
        .execute_with_clock(&attempt.request, &attempt.grant, &clock)
        .unwrap()
        .into_result();
    if attach {
        let g = &gate;
        let spec = PublishSpec {
            schema_version: 1,
            artifact_type: g.policy.requirements[0].report_type.clone(),
            producer: artifact_producer(&attempt.request).unwrap(),
            source_revision: workflow_artifacts::SourceRevision {
                repository: "fixture-repository".into(),
                revision: "a".repeat(40),
            },
            inputs: vec![],
            access: AccessScope::Run {
                run_id: start.run_id.clone(),
            },
            retention: Retention::RunDependency,
        };
        // The report asserts true even when the real worker returns false.
        let r = artifacts
            .publish(&spec, &mut br#"{"valid":true}"#.as_slice())
            .unwrap();
        let AdapterOutcome::Succeeded { evidence, .. } = &mut result.outcome else {
            panic!("inspection")
        };
        evidence.push(workflow_worker::EvidenceRef {
            artifact_id: r.artifact_id,
            digest: r.digest,
        });
    }
    Fixture {
        db,
        start,
        lease,
        attempt: *attempt,
        result,
    }
}
fn context(s: &Snapshot, node: &str) -> GateContext {
    let NodeState::CheckingGate { context, .. } = &s.frames[&1].nodes[node].state else {
        panic!("gate")
    };
    *context.clone()
}
#[test]
fn both_gates_require_fenced_proofs_and_survive_reopen_without_reinvoking_the_task() {
    let f = fixture(true, true, 60000);
    let clock = Time(Cell::new(1000));
    let before = f.snapshot();
    let fake = event(
        &before,
        "raw-success",
        EventKind::TaskCompleted {
            instance_id: before.frames[&1].nodes["inspect"].instance_id,
            result: TaskResult::Succeeded {
                outputs: match &f.result.outcome {
                    AdapterOutcome::Succeeded { outputs, .. } => outputs.clone(),
                    _ => unreachable!(),
                },
            },
        },
    );
    assert_eq!(
        f.store().apply(&fake).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    f.finish();
    let waiting = f.snapshot();
    assert_eq!(waiting.status, RunStatus::Running);
    assert_eq!(waiting.frames[&1].nodes["route"].state, NodeState::Pending);
    let command = f
        .store()
        .outbox(&f.start.run_id, 0, 100, true)
        .unwrap()
        .items
        .remove(0);
    assert_eq!(
        f.store()
            .acknowledge(&DeliveryReceipt {
                run_id: f.start.run_id.clone(),
                command_id: command.command_id.clone(),
                command_digest: command.command_digest.clone(),
                delivery_id: "skip-check".into()
            })
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert!(matches!(f.claim(), Claimed::Handled { .. }));
    assert_eq!(
        f.snapshot().frames[&1].nodes["inspect"].state,
        NodeState::Succeeded
    );
    assert!(matches!(
        f.snapshot().frames[&1].nodes["accepted"].state,
        NodeState::CheckingGate { .. }
    ));
    assert!(
        f.store()
            .finish_task(&f.lease, &f.attempt.attempt_id, &f.result, &clock)
            .unwrap()
            .transition
            .duplicate
    );
    assert!(matches!(f.claim(), Claimed::Handled { .. }));
    assert_eq!(f.snapshot().status, RunStatus::Succeeded);
    assert!(matches!(f.claim(), Claimed::Idle));
    let actions = f
        .store()
        .execution_history(&f.start.run_id, 0, 100)
        .unwrap()
        .items;
    assert_eq!(
        actions
            .iter()
            .filter(|r| matches!(r.action, ExecutionAction::Prepared { .. }))
            .count(),
        1
    );
    assert_eq!(
        actions
            .iter()
            .filter(|r| matches!(r.action, ExecutionAction::GateChecked { .. }))
            .count(),
        2
    );
    let gate_event = f
        .store()
        .history(&f.start.run_id, 0, 100)
        .unwrap()
        .items
        .into_iter()
        .find(|r| matches!(r.event.kind, EventKind::GateEvaluated { .. }))
        .unwrap()
        .event;
    assert_eq!(
        f.store().apply(&gate_event).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    f.store().verify(&f.start.run_id).unwrap();
}
#[test]
fn actual_false_output_cannot_be_overridden_by_a_true_report() {
    let f = fixture(false, true, 60000);
    f.finish();
    f.claim();
    let s = f.snapshot();
    assert_eq!(s.status, RunStatus::Failed);
    let n = &s.frames[&1].nodes["inspect"];
    assert_eq!(n.reason.as_deref(), Some("postcondition_fail"));
    assert_eq!(
        n.gate_decision.as_ref().unwrap().verdict,
        workflow_gates::Verdict::Fail
    );
}
#[test]
fn missing_evidence_waits_across_restart_and_explicit_retry_has_no_worker_cost() {
    let f = fixture(true, false, 60000);
    f.finish();
    f.claim();
    let s = f.snapshot();
    let c = context(&s, "inspect");
    assert_eq!(
        s.frames[&1].nodes["inspect"]
            .gate_decision
            .as_ref()
            .unwrap()
            .verdict,
        workflow_gates::Verdict::Unknown
    );
    let count = f
        .store()
        .execution_history(&f.start.run_id, 0, 100)
        .unwrap()
        .items
        .len();
    for _ in 0..3 {
        assert!(matches!(f.claim(), Claimed::Idle));
    }
    assert_eq!(
        f.store()
            .execution_history(&f.start.run_id, 0, 100)
            .unwrap()
            .items
            .len(),
        count
    );
    let retry = Event {
        event_id: "retry-gate".into(),
        run_id: s.run_id.clone(),
        run_digest: s.run_digest.clone(),
        expected_revision: s.revision,
        at_unix_ms: 1000,
        kind: EventKind::RetryGate {
            instance_id: s.frames[&1].nodes["inspect"].instance_id,
            context_digest: digest(&c).unwrap(),
        },
    };
    f.store().apply(&retry).unwrap();
    assert!(f.store().apply(&retry).unwrap().transition.duplicate);
    f.claim();
    assert_eq!(f.snapshot().status, RunStatus::Running);
    assert_eq!(
        f.store()
            .execution_history(&f.start.run_id, 0, 100)
            .unwrap()
            .items
            .iter()
            .filter(|r| matches!(r.action, ExecutionAction::Prepared { .. }))
            .count(),
        1
    );
    let s = f.snapshot();
    f.store()
        .apply(&event(&s, "cancel", EventKind::Cancel))
        .unwrap();
    assert_eq!(f.snapshot().status, RunStatus::Cancelled);
}
struct Jump(Cell<usize>);
impl Clock for Jump {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        let n = self.0.get();
        self.0.set(n + 1);
        Ok(if n == 0 { 1000 } else { 1001 })
    }
}
#[test]
fn expiry_before_commit_rolls_back_the_gate_event_receipt_and_execution_proof() {
    let f = fixture(true, true, 1);
    f.finish();
    let before = f.snapshot();
    assert_eq!(
        f.store()
            .claim_next(&f.lease, &Jump(Cell::new(0)))
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    assert_eq!(f.snapshot(), before);
    assert_eq!(
        f.store()
            .outbox(&f.start.run_id, 0, 100, true)
            .unwrap()
            .items
            .len(),
        1
    );
    f.store()
        .claim_next(&f.lease, &Time(Cell::new(1001)))
        .unwrap();
    let s = f.snapshot();
    assert_eq!(
        s.frames[&1].nodes["inspect"]
            .gate_decision
            .as_ref()
            .unwrap()
            .checks[0]
            .reason,
        workflow_gates::Reason::Expired
    );
}
#[test]
fn immutable_policy_and_run_identity_reject_deleting_or_weakening_a_gate() {
    let f = fixture(true, false, 60000);
    let mut edited = f.start.clone();
    edited.bundle.postconditions.clear();
    assert_eq!(
        f.store().start(&edited).unwrap_err().code,
        ErrorCode::StartConflict
    );
    edited = f.start.clone();
    edited.run_id = "new-run".into();
    for g in &mut edited.bundle.postconditions {
        g.policy.requirements[0].max_age_ms = 60001;
    }
    assert_eq!(
        f.store().start(&edited).unwrap_err().code,
        ErrorCode::BindingConflict
    );
}
#[test]
fn migration_requires_live_artifact_dependencies_and_preserves_existing_observations() {
    let f = fixture_with_gates(true, true, 60000, false);
    // An ungated v3-compatible fixture can be reconstructed from the same immutable start.
    let db = Db::new();
    let mut start = f.start.clone();
    start.bundle.postconditions.clear();
    let mut store = db.store();
    store.start(&start).unwrap();
    store
        .connection
        .pragma_update(None, "user_version", 3)
        .unwrap();
    drop(store);
    assert_eq!(
        SqliteRunStore::open(&db.path).err().unwrap().code,
        ErrorCode::UnsupportedStorage
    );
    assert_eq!(
        SqliteRunStore::migrate(&db.path)
            .unwrap()
            .get(&start.run_id)
            .unwrap()
            .revision,
        1
    );
    // Test the artifact-aware upgrade path with real persisted execution evidence.
    let mut original = f.store();
    original
        .finish_task(
            &f.lease,
            &f.attempt.attempt_id,
            &f.result,
            &Time(Cell::new(1000)),
        )
        .unwrap();
    let before = original.get(&f.start.run_id).unwrap();
    original
        .connection
        .pragma_update(None, "user_version", 3)
        .unwrap();
    drop(original);
    assert_eq!(
        SqliteRunStore::migrate(&f.db.path).err().unwrap().code,
        ErrorCode::ArtifactUnavailable
    );
    let c = Connection::open(&f.db.path).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    drop(c);
    let mut upgraded = SqliteRunStore::migrate_with_artifacts(
        &f.db.path,
        Some(Box::new(LocalArtifactStore::open(f.root()).unwrap())),
    )
    .unwrap();
    assert_eq!(upgraded.get(&f.start.run_id).unwrap(), before);
}
#[test]
fn missing_artifacts_during_gate_evaluation_abort_without_persisting_a_mutable_unknown() {
    use std::rc::Rc;
    struct Flaky {
        reader: LocalArtifactStore,
        fail: Rc<Cell<bool>>,
    }
    impl ArtifactReader for Flaky {
        fn verify(
            &self,
            r: &workflow_artifacts::ArtifactLink,
        ) -> workflow_artifacts::Result<workflow_artifacts::ArtifactRef> {
            if self.fail.replace(false) {
                return Err(workflow_artifacts::Error::new(
                    workflow_artifacts::ErrorCode::NotFound,
                    "temporarily unavailable",
                ));
            }
            self.reader.verify(r)
        }
    }
    let f = fixture(true, true, 60000);
    f.finish();
    let before = f.snapshot();
    let fail = Rc::new(Cell::new(false));
    let mut store = SqliteRunStore::open(&f.db.path)
        .unwrap()
        .with_artifacts(Box::new(Flaky {
            reader: LocalArtifactStore::open(f.root()).unwrap(),
            fail: fail.clone(),
        }));
    let error = store
        .claim_internal(&f.lease, &Time(Cell::new(1000)), |phase| {
            if phase == "before_gate_evaluation" {
                fail.set(true)
            }
        })
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ArtifactUnavailable);
    assert_eq!(f.snapshot(), before);
    f.claim();
    f.claim();
    assert_eq!(f.snapshot().status, RunStatus::Succeeded);
}

#[test]
fn recovery_recomputes_gate_evidence_and_rejects_a_forged_but_well_hashed_decision() {
    struct Lie {
        reader: LocalArtifactStore,
        check: workflow_gates::ExecutedCheck,
    }
    impl ArtifactReader for Lie {
        fn verify(
            &self,
            r: &workflow_artifacts::ArtifactLink,
        ) -> workflow_artifacts::Result<workflow_artifacts::ArtifactRef> {
            self.reader.verify(r)
        }
    }
    impl workflow_gates::EvidenceSource for Lie {
        fn executed_check(
            &self,
            _: &workflow_artifacts::Producer,
        ) -> workflow_artifacts::Result<Option<workflow_gates::ExecutedCheck>> {
            Ok(Some(self.check.clone()))
        }
    }
    for with_proof in [false, true] {
        let f = fixture(false, true, 60000);
        f.finish();
        let snapshot = f.snapshot();
        let context = context(&snapshot, "inspect");
        let mut check = executed_check(&snapshot.run_digest, &f.attempt, &f.result, 1000).unwrap();
        let workflow_gates::CheckOutcome::Succeeded { outputs } = &mut check.outcome else {
            panic!("observation")
        };
        outputs.insert("valid".into(), true.into());
        let request = workflow_gates::Request {
            policy: context.policy.clone(),
            target: context.target.clone(),
            evidence: vec![workflow_gates::Evidence {
                requirement_id: context.policy.requirements[0].id.clone(),
                report: check.evidence[0].clone(),
            }],
        };
        let decision = workflow_gates::evaluate(
            &request,
            &Lie {
                reader: LocalArtifactStore::open(f.root()).unwrap(),
                check,
            },
            1000,
        )
        .unwrap();
        let forged = Event {
            event_id: "forged-gate".into(),
            run_id: snapshot.run_id.clone(),
            run_digest: snapshot.run_digest.clone(),
            expected_revision: snapshot.revision,
            at_unix_ms: 1000,
            kind: EventKind::GateEvaluated {
                instance_id: snapshot.frames[&1].nodes["inspect"].instance_id,
                context_digest: digest(&context).unwrap(),
                evaluation: Box::new(workflow_kernel::GateEvaluation { request, decision }),
            },
        };
        assert_eq!(
            f.store().apply(&forged).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        // Simulate corruption below the public ingress: preserve all ordinary hashes,
        // snapshots, receipts and (optionally) the claimed execution proof.
        let mut store = f.store();
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut r =
            crate::recovery::recover(&tx, &f.start.run_id, store.artifacts.as_deref()).unwrap();
        let (mut a, _) = crate::execution::read(&tx, &r, store.artifacts.as_deref()).unwrap();
        let entry = r
            .outbox
            .iter()
            .find(|e| e.receipt.is_none())
            .unwrap()
            .clone();
        let committed = crate::writes::persist_event(&tx, &mut r, &forged, |_| {}).unwrap();
        crate::writes::persist_receipt(
            &tx,
            &r,
            &DeliveryReceipt {
                run_id: f.start.run_id.clone(),
                command_id: entry.command_id.clone(),
                command_digest: entry.command_digest,
                delivery_id: format!("runtime-{}-{}", f.lease.epoch, entry.sequence),
            },
        )
        .unwrap();
        if with_proof {
            crate::execution::append(
                &tx,
                &mut a,
                ExecutionAction::GateChecked {
                    epoch: f.lease.epoch,
                    command_id: entry.command_id,
                    event_id: forged.event_id,
                    event_revision: committed.snapshot.revision,
                    at_unix_ms: 1000,
                },
            )
            .unwrap();
        }
        tx.commit().unwrap();
        drop(store);
        assert_eq!(
            f.store().verify(&f.start.run_id).unwrap_err().code,
            ErrorCode::CorruptStorage
        );
    }
}
#[test]
fn cancellation_and_a_new_lease_fence_pending_gate_work() {
    let f = fixture(true, true, 60000);
    f.finish();
    let before = f.snapshot();
    f.store()
        .apply(&event(&before, "cancel", EventKind::Cancel))
        .unwrap();
    assert!(matches!(
        f.store()
            .claim_next(&f.lease, &Time(Cell::new(1001)))
            .unwrap(),
        Claimed::Handled { .. }
    ));
    assert_eq!(f.snapshot().status, RunStatus::Cancelled);
    assert!(
        !f.store()
            .execution_history(&f.start.run_id, 0, 100)
            .unwrap()
            .items
            .iter()
            .any(|r| matches!(r.action, ExecutionAction::GateChecked { .. }))
    );
    let f = fixture(true, true, 60000);
    f.finish();
    let clock = Time(Cell::new(11000));
    let next = f
        .store()
        .acquire(
            &LeaseRequest {
                run_id: f.start.run_id.clone(),
                owner: "next".into(),
                acquisition_id: "next".into(),
                ttl_ms: 1000,
            },
            &clock,
        )
        .unwrap();
    assert_eq!(
        f.store().claim_next(&f.lease, &clock).unwrap_err().code,
        ErrorCode::LeaseConflict
    );
    f.store().claim_next(&next, &clock).unwrap();
    f.store().claim_next(&next, &clock).unwrap();
    assert_eq!(f.snapshot().status, RunStatus::Succeeded);
}
#[test]
fn gate_process_worker() {
    let Ok(dir) = std::env::var("WORKFLOW_GATE_PROCESS") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let lease: Lease =
        workflow_worker::parse_message(&std::fs::read(dir.join("lease.json")).unwrap()).unwrap();
    if let Ok(ready) = std::env::var("WORKFLOW_GATE_READY") {
        std::fs::write(dir.join(ready), b"ready").unwrap();
        let start = Instant::now();
        while !dir.join("go").exists() {
            assert!(start.elapsed() < Duration::from_secs(15));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let phase = std::env::var("WORKFLOW_GATE_KILL_PHASE").unwrap_or_default();
    let mut store = SqliteRunStore::open(dir.join("runs.db"))
        .unwrap()
        .with_artifacts(Box::new(
            LocalArtifactStore::open(dir.join("artifacts")).unwrap(),
        ));
    let claimed = store
        .claim_internal(&lease, &Time(Cell::new(1000)), |at| {
            if at == phase {
                std::fs::write(dir.join("paused"), b"ready").unwrap();
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        })
        .unwrap();
    assert!(!matches!(claimed, Claimed::Task { .. }));
}
fn child(f: &Fixture) -> Process {
    std::fs::write(
        f.db.dir.join("lease.json"),
        workflow_worker::to_message(&f.lease).unwrap(),
    )
    .unwrap();
    let mut p = Process::new(std::env::current_exe().unwrap());
    p.args([
        "--exact",
        "tests::gates::gate_process_worker",
        "--nocapture",
    ])
    .env("WORKFLOW_GATE_PROCESS", &f.db.dir)
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    p
}
fn wait_file(path: &std::path::Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "{}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn killed_gate_transactions_recover_before_or_after_a_complete_proof_and_never_reinvoke() {
    for phase in [
        "state_written",
        "gate_written",
        "before_commit",
        "after_commit",
    ] {
        let f = fixture(true, true, 60000);
        f.finish();
        let mut p = child(&f)
            .env("WORKFLOW_GATE_KILL_PHASE", phase)
            .spawn()
            .unwrap();
        wait_file(&f.db.dir.join("paused"));
        p.kill().unwrap();
        p.wait().unwrap();
        let completed = f
            .store()
            .execution_history(&f.start.run_id, 0, 100)
            .unwrap()
            .items
            .iter()
            .filter(|r| matches!(r.action, ExecutionAction::GateChecked { .. }))
            .count();
        assert_eq!(completed, usize::from(phase == "after_commit"));
        for _ in 0..3 {
            assert!(!matches!(f.claim(), Claimed::Task { .. }));
        }
        assert_eq!(f.snapshot().status, RunStatus::Succeeded);
        let records = f
            .store()
            .execution_history(&f.start.run_id, 0, 100)
            .unwrap()
            .items;
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r.action, ExecutionAction::GateChecked { .. }))
                .count(),
            2
        );
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r.action, ExecutionAction::Prepared { .. }))
                .count(),
            1
        );
    }
}
#[test]
fn concurrent_gate_drivers_serialize_each_decision_once() {
    let f = fixture(true, true, 60000);
    f.finish();
    let mut a = child(&f)
        .env("WORKFLOW_GATE_READY", "ready-a")
        .spawn()
        .unwrap();
    let mut b = child(&f)
        .env("WORKFLOW_GATE_READY", "ready-b")
        .spawn()
        .unwrap();
    wait_file(&f.db.dir.join("ready-a"));
    wait_file(&f.db.dir.join("ready-b"));
    std::fs::write(f.db.dir.join("go"), b"go").unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert_eq!(f.snapshot().status, RunStatus::Succeeded);
    assert_eq!(
        f.store()
            .execution_history(&f.start.run_id, 0, 100)
            .unwrap()
            .items
            .iter()
            .filter(|r| matches!(r.action, ExecutionAction::GateChecked { .. }))
            .count(),
        2
    );
}

#[test]
fn gate_cannot_borrow_a_later_settlement_even_at_the_same_host_time() {
    let f = fixture(true, true, 60000);
    f.finish();
    let store = f.store();
    let r = crate::recovery::recover(
        &store.connection,
        &f.start.run_id,
        store.artifacts.as_deref(),
    )
    .unwrap();
    let (a, _) = crate::execution::read(&store.connection, &r, store.artifacts.as_deref()).unwrap();
    let context = context(r.engine.snapshot(), "inspect");
    let evaluate = |revision| {
        crate::execution::gates::evaluate(
            &r,
            &a,
            &context,
            store.artifacts.as_deref(),
            1000,
            revision,
        )
    };
    assert_eq!(evaluate(1).unwrap_err().code, ErrorCode::CorruptStorage);
    assert_eq!(
        evaluate(2).unwrap().decision.verdict,
        workflow_gates::Verdict::Pass
    );
}
