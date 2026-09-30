use super::*;
use std::cell::Cell;
use workflow_effects::{
    CallKind, EffectAdapter, EffectAttempt, EffectReceipt, EffectStatus, Observation,
};
use workflow_kernel::SignalDecision;
use workflow_worker::{Clock, FailureClass};
const ID: &str = "demo-effect-compensation";
struct Time(Cell<u64>);
impl Time {
    fn new(n: u64) -> Self {
        Self(Cell::new(n))
    }
    fn set(&self, n: u64) {
        self.0.set(n);
    }
}
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0.get())
    }
}
fn request() -> StartRun {
    workflow_worker::parse_message(
        &std::fs::read(base().join("examples/runs/effect-compensation.json")).unwrap(),
    )
    .unwrap()
}
fn lease(s: &mut SqliteRunStore, c: &dyn Clock, name: &str) -> Lease {
    s.acquire(
        &LeaseRequest {
            run_id: ID.into(),
            owner: name.into(),
            acquisition_id: format!("claim-{name}"),
            ttl_ms: 1000,
        },
        c,
    )
    .unwrap()
}
fn next(s: &mut SqliteRunStore, l: &Lease, c: &dyn Clock) -> EffectAttempt {
    for _ in 0..20 {
        match s.claim_effect(l, c).unwrap() {
            EffectClaim::Call { attempt } => return *attempt,
            EffectClaim::Handled => {}
            EffectClaim::Idle => assert!(matches!(
                s.claim_next(l, c).unwrap(),
                Claimed::Handled { .. }
            )),
            other => panic!("{other:?}"),
        }
    }
    panic!("expected compensation task");
}
struct Provider {
    path: PathBuf,
    reject_next: Cell<Option<FailureClass>>,
}
impl Provider {
    fn new(dir: &std::path::Path) -> Self {
        let p = Self {
            path: dir.join("compensation-provider.db"),
            reject_next: Cell::new(None),
        };
        let c = Connection::open(&p.path).unwrap();
        c.execute_batch("PRAGMA synchronous=FULL;PRAGMA foreign_keys=ON;CREATE TABLE resources(id TEXT PRIMARY KEY,parent TEXT REFERENCES resources(id));CREATE TABLE operations(key TEXT PRIMARY KEY,intent TEXT NOT NULL,receipt TEXT NOT NULL);CREATE TABLE calls(kind TEXT,node TEXT,key TEXT);").unwrap();
        p
    }
    fn count(&self) -> i64 {
        Connection::open(&self.path)
            .unwrap()
            .query_row("SELECT count(*) FROM resources", [], |r| r.get(0))
            .unwrap()
    }
}
impl EffectAdapter for Provider {
    fn execute(&self, p: &EffectAttempt, c: &dyn Clock) -> workflow_worker::Result<Observation> {
        use rusqlite::OptionalExtension;
        p.validate()?;
        assert!(c.now_unix_ms()? < p.deadline_unix_ms);
        let mut db = Connection::open(&self.path).unwrap();
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        db.pragma_update(None, "synchronous", "FULL").unwrap();
        db.execute(
            "INSERT INTO calls VALUES(?1,?2,?3)",
            rusqlite::params![
                if p.kind == CallKind::Write {
                    "write"
                } else {
                    "query"
                },
                p.intent.node_id,
                p.intent.operation_key
            ],
        )
        .unwrap();
        if p.intent.compensates.is_some()
            && p.kind == CallKind::Write
            && let Some(class) = self.reject_next.take()
        {
            return Ok(Observation::NotApplied {
                code: if class == FailureClass::Transient {
                    "retry"
                } else {
                    "forbidden"
                }
                .into(),
                class,
                message: "provider rejected before mutation".into(),
            });
        }
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let stored: Option<(String, String)> = tx
            .query_row(
                "SELECT intent,receipt FROM operations WHERE key=?1",
                [&p.intent.operation_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .unwrap();
        if let Some((intent, value)) = stored {
            assert_eq!(intent, workflow_effects::digest(&p.intent)?);
            return workflow_worker::parse_message(value.as_bytes());
        }
        if p.kind == CallKind::Query {
            return Ok(Observation::Absent);
        }
        let (resource_id, outputs) = if let Some(original) = &p.intent.compensates {
            let id = &original.receipt.resource_id;
            let input = if p.intent.node_id == "undo_deploy" {
                "deployment_id"
            } else {
                "environment_id"
            };
            assert_eq!(p.intent.inputs[input], serde_json::json!(id));
            assert_eq!(
                tx.execute("DELETE FROM resources WHERE id=?1", [id])
                    .unwrap(),
                1
            );
            (id.clone(), Values::new())
        } else {
            let id = if p.intent.node_id == "environment" {
                "env-1"
            } else {
                "deployment-1"
            };
            let parent = p
                .intent
                .inputs
                .get("environment_id")
                .and_then(|v| v.as_str());
            tx.execute(
                "INSERT INTO resources VALUES(?1,?2)",
                rusqlite::params![id, parent],
            )
            .unwrap();
            (
                id.into(),
                [(
                    if parent.is_some() {
                        "deployment_id"
                    } else {
                        "environment_id"
                    }
                    .into(),
                    serde_json::json!(id),
                )]
                .into(),
            )
        };
        let observation = Observation::Applied {
            receipt: EffectReceipt {
                operation_key: p.intent.operation_key.clone(),
                intent_digest: workflow_effects::digest(&p.intent)?,
                target: p.intent.policy.target.clone(),
                resource_id,
                provider_receipt: format!("confirmed-{}", p.intent.node_id),
                outputs,
            },
        };
        tx.execute(
            "INSERT INTO operations VALUES(?1,?2,?3)",
            rusqlite::params![
                p.intent.operation_key,
                workflow_effects::digest(&p.intent)?,
                serde_json::to_string(&observation).unwrap()
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        Ok(observation)
    }
}
fn forwards(
    s: &mut SqliteRunStore,
    l: &Lease,
    c: &dyn Clock,
    provider: &Provider,
) -> Vec<EffectAttempt> {
    let mut calls = vec![];
    for node in ["environment", "deploy"] {
        let p = next(s, l, c);
        assert_eq!(p.intent.node_id, node);
        let observed = provider.execute(&p, c).unwrap();
        s.observe_effect(l, &p.attempt_id, &observed, c).unwrap();
        calls.push(p);
    }
    calls
}
fn review(s: &mut SqliteRunStore, c: &dyn Clock, decision: SignalDecision) {
    let wait = s.waits(ID, 0, 100).unwrap().items.remove(0);
    let snapshot = s.get(ID).unwrap();
    s.receive_signal(
        &SignalSubmission {
            schema_version: 1,
            run_id: ID.into(),
            run_digest: snapshot.run_digest,
            message: workflow_kernel::SignalMessage {
                exception: None,
                schema_version: 1,
                message_id: "sandbox-review".into(),
                correlation_id: wait.correlation_id,
                target: wait.target,
                source: "test-human-simulation".into(),
                decision,
                reason: "fixture branch selection; not actual approval".into(),
                outputs: Values::new(),
                expires_at_unix_ms: 100000,
            },
        },
        c,
    )
    .unwrap();
}
#[test]
fn rejection_compensates_in_reverse_order_and_completed_steps_survive_restart() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let l = lease(&mut s, &c, "one");
    let provider = Provider::new(&db.dir);
    let original = forwards(&mut s, &l, &c, &provider);
    assert_eq!(provider.count(), 2);
    assert_eq!(
        original[1].intent.dependencies,
        [original[0].intent.operation_key.clone()]
    );
    review(&mut s, &c, SignalDecision::Reject);
    provider.reject_next.set(Some(FailureClass::Transient));
    let undo = next(&mut s, &l, &c);
    assert_eq!(undo.intent.node_id, "undo_deploy");
    assert_eq!(
        undo.intent.compensates.as_ref().unwrap().operation_key,
        original[1].intent.operation_key
    );
    s.observe_effect(
        &l,
        &undo.attempt_id,
        &provider.execute(&undo, &c).unwrap(),
        &c,
    )
    .unwrap();
    assert!(matches!(
        s.claim_effect(&l, &c).unwrap(),
        EffectClaim::Waiting { .. }
    ));
    c.set(1020);
    let retry = next(&mut s, &l, &c);
    assert_eq!(retry.intent, undo.intent);
    s.observe_effect(
        &l,
        &retry.attempt_id,
        &provider.execute(&retry, &c).unwrap(),
        &c,
    )
    .unwrap();
    assert_eq!(provider.count(), 1);
    drop(s);
    let mut s = SqliteRunStore::open(&db.path).unwrap();
    let last = next(&mut s, &l, &c);
    assert_eq!(last.intent.node_id, "undo_environment");
    assert_eq!(
        last.intent.dependencies.as_slice(),
        std::slice::from_ref(&undo.intent.operation_key)
    );
    s.observe_effect(
        &l,
        &last.attempt_id,
        &provider.execute(&last, &c).unwrap(),
        &c,
    )
    .unwrap();
    assert_eq!(provider.count(), 0);
    assert_eq!(s.get(ID).unwrap().status, RunStatus::Cancelled);
    let ledger = s.effects(ID, 0, 100).unwrap().items;
    assert_eq!(
        ledger.iter().filter(|s| s.compensated_by.is_some()).count(),
        2
    );
    let calls: i64 = Connection::open(&provider.path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM calls WHERE node='undo_deploy' AND kind='write'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(calls, 2);
    s.verify(ID).unwrap();
}

pub(super) fn process(s: &mut SqliteRunStore, dir: &std::path::Path, hook: impl Fn(&str)) {
    let l: Lease =
        workflow_worker::parse_message(&std::fs::read(dir.join("lease.json")).unwrap()).unwrap();
    let c = Time::new(1000);
    let p = next(s, &l, &c);
    assert_eq!(p.intent.node_id, "undo_environment");
    let provider = Provider {
        path: dir.join("compensation-provider.db"),
        reject_next: Cell::new(None),
    };
    let observation = provider.execute(&p, &c).unwrap();
    std::fs::write(dir.join("attempt.json"), document(&p).unwrap()).unwrap();
    hook("provider_committed");
    s.observe_effect(&l, &p.attempt_id, &observation, &c)
        .unwrap();
}

#[test]
fn killed_compensator_after_provider_commit_recovers_by_query_without_repeating_earlier_undo() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let l = lease(&mut s, &c, "one");
    let provider = Provider::new(&db.dir);
    forwards(&mut s, &l, &c, &provider);
    review(&mut s, &c, SignalDecision::Reject);
    let undo = next(&mut s, &l, &c);
    s.observe_effect(
        &l,
        &undo.attempt_id,
        &provider.execute(&undo, &c).unwrap(),
        &c,
    )
    .unwrap();
    assert_eq!(provider.count(), 1);
    std::fs::write(db.dir.join("lease.json"), document(&l).unwrap()).unwrap();
    drop(s);
    let mut worker = child(&db, "compensation-crash", "one", "provider_committed");
    wait_file(&db.dir.join("ready-one"), &mut worker);
    worker.kill().unwrap();
    worker.wait().unwrap();
    assert_eq!(provider.count(), 0);
    let old: EffectAttempt =
        workflow_worker::parse_message(&std::fs::read(db.dir.join("attempt.json")).unwrap())
            .unwrap();
    let mut s = SqliteRunStore::open(&db.path).unwrap();
    let before = s.effects(ID, 0, 100).unwrap().items;
    assert_eq!(
        before.iter().filter(|s| s.compensated_by.is_some()).count(),
        1
    );
    c.set(2000);
    let current = lease(&mut s, &c, "two");
    let q = next(&mut s, &current, &c);
    assert_eq!(q.kind, CallKind::Query);
    assert_eq!(q.intent, old.intent);
    assert_eq!(q.number, 2);
    let observed = provider.execute(&q, &c).unwrap();
    assert_eq!(
        s.observe_effect(&l, &old.attempt_id, &observed, &c)
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    s.observe_effect(&current, &q.attempt_id, &observed, &c)
        .unwrap();
    assert_eq!(s.get(ID).unwrap().status, RunStatus::Cancelled);
    let provider_db = Connection::open(&provider.path).unwrap();
    for node in ["undo_deploy", "undo_environment"] {
        assert_eq!(
            provider_db
                .query_row(
                    "SELECT count(*) FROM calls WHERE kind='write' AND node=?1",
                    [node],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }
    assert_eq!(
        provider_db
            .query_row("SELECT count(*) FROM calls WHERE kind='query'", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        1
    );
    assert_eq!(
        s.effects(ID, 0, 100)
            .unwrap()
            .items
            .iter()
            .filter(|s| s.compensated_by.is_some())
            .count(),
        2
    );
    s.verify(ID).unwrap();
}

#[test]
fn permanent_compensation_failure_requires_audited_manual_completion_without_resetting_budget() {
    use workflow_effects::{ManualOutcome, ManualResolution};
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let l = lease(&mut s, &c, "one");
    let provider = Provider::new(&db.dir);
    forwards(&mut s, &l, &c, &provider);
    review(&mut s, &c, SignalDecision::Reject);
    let p = next(&mut s, &l, &c);
    provider
        .reject_next
        .set(Some(FailureClass::PermissionDenied));
    s.observe_effect(&l, &p.attempt_id, &provider.execute(&p, &c).unwrap(), &c)
        .unwrap();
    assert_eq!(provider.count(), 2);
    drop(s);
    let mut s = SqliteRunStore::open(&db.path).unwrap();
    assert_eq!(
        s.get(ID).unwrap().frames[&1].nodes["undo_deploy"].state,
        workflow_kernel::NodeState::Reconciling
    );
    assert!(matches!(
        s.claim_effect(&l, &c).unwrap(),
        EffectClaim::Manual { .. }
    ));
    let item = s
        .effects(ID, 0, 100)
        .unwrap()
        .items
        .into_iter()
        .find(|s| s.intent.operation_key == p.intent.operation_key)
        .unwrap();
    assert!(matches!(item.status, EffectStatus::NeedsAttention { .. }));
    assert_eq!(item.calls.len(), 1);
    // A test operator performs the business operation with provider access after
    // fixing the permission. This is actual provider I/O, never an invented receipt.
    let Observation::Applied { receipt } = provider.execute(&p, &c).unwrap() else {
        panic!()
    };
    let resolution = ManualResolution {
        resolution_id: "manual-undo".into(),
        actor: "test-local-operator".into(),
        reason: "provider permission repaired; cleanup performed manually".into(),
        evidence: receipt.provider_receipt.clone(),
        outcome: ManualOutcome::Applied { receipt },
    };
    s.resolve_effect(&l, &p.intent.operation_key, &resolution, &c)
        .unwrap();
    assert!(
        s.resolve_effect(&l, &p.intent.operation_key, &resolution, &c)
            .unwrap()
            .transition
            .duplicate
    );
    let mut conflict = resolution;
    conflict.reason = "changed annotation".into();
    assert_eq!(
        s.resolve_effect(&l, &p.intent.operation_key, &conflict, &c)
            .unwrap_err()
            .code,
        ErrorCode::ReceiptConflict
    );
    let last = next(&mut s, &l, &c);
    s.observe_effect(
        &l,
        &last.attempt_id,
        &provider.execute(&last, &c).unwrap(),
        &c,
    )
    .unwrap();
    assert_eq!(provider.count(), 0);
    assert_eq!(
        s.effects(ID, 0, 100)
            .unwrap()
            .items
            .into_iter()
            .find(|s| s.intent.operation_key == p.intent.operation_key)
            .unwrap()
            .calls
            .len(),
        1
    );
    s.verify(ID).unwrap();
}

#[test]
fn accepted_release_keeps_resources_timeout_compensates_and_cancel_never_automatically_reverses() {
    for branch in ["accepted", "timeout", "cancel"] {
        let db = Db::new();
        let mut s = db.store();
        s.start(&request()).unwrap();
        let c = Time::new(1000);
        let l = lease(&mut s, &c, "one");
        let provider = Provider::new(&db.dir);
        forwards(&mut s, &l, &c, &provider);
        match branch {
            "accepted" => review(&mut s, &c, SignalDecision::Approve),
            "timeout" => {
                c.set(1100);
                assert!(s.tick_due(&l, &c).unwrap().is_some());
                for node in ["undo_deploy", "undo_environment"] {
                    let p = next(&mut s, &l, &c);
                    assert_eq!(p.intent.node_id, node);
                    s.observe_effect(&l, &p.attempt_id, &provider.execute(&p, &c).unwrap(), &c)
                        .unwrap();
                }
            }
            _ => {
                let snapshot = s.get(ID).unwrap();
                s.apply(&event(&snapshot, "cancel", EventKind::Cancel))
                    .unwrap();
            }
        }
        assert_eq!(
            s.get(ID).unwrap().status,
            if branch == "accepted" {
                RunStatus::Succeeded
            } else {
                RunStatus::Cancelled
            }
        );
        assert_eq!(provider.count(), if branch == "timeout" { 0 } else { 2 });
        let items = s.effects(ID, 0, 100).unwrap().items;
        assert_eq!(
            items
                .iter()
                .filter(|s| s.intent.compensates.is_some())
                .count(),
            if branch == "timeout" { 2 } else { 0 }
        );
        s.verify(ID).unwrap();
    }
}

#[test]
fn bundle_rejects_unmanaged_late_duplicate_irreversible_or_mismatched_compensation_dependencies() {
    let original = serde_json::to_value(request()).unwrap();
    for (path, value) in [
        (
            "/bundle/effect_bindings/1/depends_on",
            serde_json::json!(["missing"]),
        ),
        (
            "/bundle/effect_bindings/0/depends_on",
            serde_json::json!(["deploy"]),
        ),
        (
            "/bundle/effect_bindings/1/depends_on",
            serde_json::json!(["environment", "environment"]),
        ),
        (
            "/bundle/effect_bindings/2/compensates",
            serde_json::json!("environment"),
        ),
        (
            "/bundle/effect_bindings/3/compensates",
            serde_json::json!("undo_deploy"),
        ),
        (
            "/bundle/capabilities/0/effects/irreversible",
            serde_json::json!(true),
        ),
        (
            "/bundle/capabilities/0/effects/compensation",
            serde_json::Value::Null,
        ),
    ] {
        let mut changed = original.clone();
        // New optional fields are absent from the canonical fixture.
        let (parent, field) = path.rsplit_once('/').unwrap();
        changed
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(field.into(), value);
        let r: StartRun = serde_json::from_value(changed).unwrap();
        let db = Db::new();
        let mut s = db.store();
        assert!(s.start(&r).is_err(), "accepted invalid binding: {path}");
        assert!(s.list(None, 100).unwrap().items.is_empty());
    }
}

#[test]
fn abandoning_failed_compensation_never_marks_original_undone_or_deletes_its_parent() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let l = lease(&mut s, &c, "one");
    let provider = Provider::new(&db.dir);
    forwards(&mut s, &l, &c, &provider);
    review(&mut s, &c, SignalDecision::Reject);
    let p = next(&mut s, &l, &c);
    provider
        .reject_next
        .set(Some(FailureClass::PermissionDenied));
    s.observe_effect(&l, &p.attempt_id, &provider.execute(&p, &c).unwrap(), &c)
        .unwrap();
    s.resolve_effect(
        &l,
        &p.intent.operation_key,
        &workflow_effects::ManualResolution {
            resolution_id: "abandon-cleanup".into(),
            actor: "test-operator".into(),
            reason: "provider checked; cleanup not performed".into(),
            evidence: "fixture-resource-inventory-two".into(),
            outcome: workflow_effects::ManualOutcome::ConfirmedNotApplied,
        },
        &c,
    )
    .unwrap();
    assert_eq!(s.get(ID).unwrap().status, RunStatus::Cancelled);
    assert_eq!(provider.count(), 2);
    assert!(
        s.effects(ID, 0, 100)
            .unwrap()
            .items
            .iter()
            .all(|s| s.compensated_by.is_none())
    );
    assert!(
        !s.effects(ID, 0, 100)
            .unwrap()
            .items
            .iter()
            .any(|s| s.intent.node_id == "undo_environment")
    );
    s.verify(ID).unwrap();
}

#[test]
fn compiler_rejects_reverse_order_conflict_even_without_explicit_undo_dependencies() {
    let mut r = request();
    r.bundle
        .effect_bindings
        .iter_mut()
        .find(|b| b.node_id == "undo_environment")
        .unwrap()
        .depends_on
        .clear();
    for e in &mut r.bundle.workflows[0].edges {
        for id in [&mut e.from, &mut e.to] {
            match id.as_str() {
                "undo_deploy" => *id = "undo_environment".into(),
                "undo_environment" => *id = "undo_deploy".into(),
                _ => {}
            }
        }
    }
    let error = workflow_kernel::CompiledBundle::compile(r.bundle).unwrap_err();
    assert!(
        error.message.contains("reverse effect dependency order"),
        "{error}"
    );
}
