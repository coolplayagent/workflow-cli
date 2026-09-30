use super::*;
use std::cell::Cell;
use workflow_effects::{
    CallKind, EffectAttempt, EffectReceipt, EffectStatus, ManualOutcome, ManualResolution,
    Observation,
};
use workflow_worker::{Clock, EffectContract, FailureClass, Idempotency};
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
        &std::fs::read(base().join("examples/runs/effect-release.json")).unwrap(),
    )
    .unwrap()
}
fn lease(s: &mut SqliteRunStore, c: &dyn Clock, name: &str) -> Lease {
    s.acquire(
        &LeaseRequest {
            run_id: "demo-effect-release".into(),
            owner: name.into(),
            acquisition_id: format!("claim-{name}"),
            ttl_ms: 100,
        },
        c,
    )
    .unwrap()
}
fn call(s: &mut SqliteRunStore, l: &Lease, c: &dyn Clock) -> EffectAttempt {
    let EffectClaim::Call { attempt } = s.claim_effect(l, c).unwrap() else {
        panic!("expected effect call")
    };
    *attempt
}
fn applied(p: &EffectAttempt) -> Observation {
    Observation::Applied {
        receipt: EffectReceipt {
            release: None,
            operation_key: p.intent.operation_key.clone(),
            intent_digest: digest(&p.intent).unwrap(),
            target: p.intent.policy.target.clone(),
            resource_id: "release-1".into(),
            provider_receipt: "provider-confirmed-release-1".into(),
            outputs: [("release_id".into(), "release-1".into())].into(),
        },
    }
}
fn manual(outcome: ManualOutcome) -> ManualResolution {
    ManualResolution {
        resolution_id: "manual-1".into(),
        actor: "local-admin".into(),
        reason: "queried sandbox audit after stopping old workers".into(),
        evidence: "sandbox-audit:record-1".into(),
        outcome,
    }
}
#[test]
fn intent_and_receipt_bind_frozen_input_target_and_atomic_transition() {
    let db = Db::new();
    let mut s = db.store();
    let req = request();
    s.start(&req).unwrap();
    let clock = Time::new(1000);
    let l = lease(&mut s, &clock, "one");
    let p = call(&mut s, &l, &clock);
    assert_eq!(p.kind, CallKind::Write);
    assert_eq!(
        s.effects(&req.run_id, 0, 10).unwrap().items[0].status,
        EffectStatus::InFlight
    );
    assert_eq!(
        s.claim_effect(&l, &clock).unwrap_err().code,
        ErrorCode::AttemptInProgress
    );
    let snapshot = s.get(&req.run_id).unwrap();
    assert_eq!(
        s.apply(&event(
            &snapshot,
            "bypass",
            EventKind::TaskCompleted {
                instance_id: p.intent.instance_id,
                result: TaskResult::Succeeded {
                    outputs: [("release_id".into(), "fake".into())].into()
                }
            }
        ))
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );
    let entry = s.outbox(&req.run_id, 0, 1, true).unwrap().items.remove(0);
    assert_eq!(
        s.acknowledge(&DeliveryReceipt {
            run_id: req.run_id.clone(),
            command_id: entry.command_id,
            command_digest: entry.command_digest,
            delivery_id: "manual-bypass".into()
        })
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );
    let mut wrong = applied(&p);
    if let Observation::Applied { receipt } = &mut wrong {
        receipt.target.id = "other".into();
    }
    assert!(s.observe_effect(&l, &p.attempt_id, &wrong, &clock).is_err());
    assert_eq!(s.get(&req.run_id).unwrap(), snapshot);
    let outcome = applied(&p);
    let committed = s
        .observe_effect(&l, &p.attempt_id, &outcome, &clock)
        .unwrap();
    assert_eq!(committed.snapshot.status, RunStatus::Succeeded);
    drop(s);
    let mut s = SqliteRunStore::open(&db.path).unwrap();
    clock.set(1500);
    assert!(
        s.observe_effect(&l, &p.attempt_id, &outcome, &clock)
            .unwrap()
            .transition
            .duplicate
    );
    assert_eq!(
        s.observe_effect(&l, &p.attempt_id, &wrong, &clock)
            .unwrap_err()
            .code,
        ErrorCode::ReceiptConflict
    );
    assert_eq!(
        s.effects(&req.run_id, 0, 10).unwrap().items[0].calls.len(),
        1
    );
    s.verify(&req.run_id).unwrap();
    let mut changed = req;
    changed.run_id = "second-run".into();
    changed.bundle.effect_bindings[0].policy.retry.max_calls += 1;
    assert_eq!(
        s.start(&changed).unwrap_err().code,
        ErrorCode::BindingConflict
    );
}
#[test]
fn orphan_is_queried_after_restart_with_stable_key_and_old_owner_is_fenced() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let old = lease(&mut s, &c, "one");
    let p = call(&mut s, &old, &c);
    drop(s);
    let mut s = SqliteRunStore::open(&db.path).unwrap();
    c.set(100000);
    let current = lease(&mut s, &c, "two");
    let q = call(&mut s, &current, &c);
    assert_eq!(q.kind, CallKind::Query);
    assert_eq!(q.intent, p.intent);
    assert_ne!(q.attempt_id, p.attempt_id);
    assert_eq!(q.number, 2);
    assert!(c.0.get() > p.intent.write_deadline());
    assert_eq!(
        s.observe_effect(&old, &p.attempt_id, &applied(&p), &c)
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    assert_eq!(
        s.observe_effect(&current, &q.attempt_id, &applied(&q), &c)
            .unwrap()
            .snapshot
            .status,
        RunStatus::Succeeded
    );
    s.verify("demo-effect-release").unwrap();
}
#[test]
fn unknown_without_guarantees_and_query_absence_require_audited_manual_resolution() {
    for query in [false, true] {
        let db = Db::new();
        let mut s = db.store();
        let mut req = request();
        if let EffectContract::Write {
            idempotency,
            query: q,
            ..
        } = &mut req.bundle.capabilities[0].effects
        {
            *idempotency = Idempotency::None;
            if !query {
                *q = None;
            }
        }
        s.start(&req).unwrap();
        let c = Time::new(1000);
        let first = lease(&mut s, &c, "one");
        let p = call(&mut s, &first, &c);
        if query {
            c.set(1100);
            let current = lease(&mut s, &c, "two");
            let q = call(&mut s, &current, &c);
            assert_eq!(q.kind, CallKind::Query);
            s.observe_effect(&current, &q.attempt_id, &Observation::Absent, &c)
                .unwrap();
        } else {
            s.observe_effect(
                &first,
                &p.attempt_id,
                &Observation::Unknown {
                    reason: "connection lost".into(),
                },
                &c,
            )
            .unwrap();
        }
        c.set(1200);
        let current = lease(&mut s, &c, "three");
        assert!(matches!(
            s.claim_effect(&current, &c).unwrap(),
            EffectClaim::Manual { .. }
        ));
        assert!(matches!(
            s.get(&req.run_id).unwrap().frames[&1].nodes["publish"].state,
            workflow_kernel::NodeState::Reconciling
        ));
        let resolved = manual(ManualOutcome::ConfirmedNotApplied);
        s.resolve_effect(&current, &p.intent.operation_key, &resolved, &c)
            .unwrap();
        c.set(1400);
        assert!(
            s.resolve_effect(&current, &p.intent.operation_key, &resolved, &c)
                .unwrap()
                .transition
                .duplicate
        );
        let records = s.execution_history(&req.run_id, 0, 100).unwrap().items;
        assert!(records.iter().any(|r| matches!(&r.action,ExecutionAction::Effect { record,.. } if matches!(&record.change,workflow_effects::EffectChange::Resolved { resolution,.. } if resolution == &resolved))));
        s.verify(&req.run_id).unwrap();
    }
}
#[test]
fn permanent_failures_settle_once_and_transient_calls_keep_backoff_and_budget() {
    for (code, class) in [
        ("bad_input", FailureClass::InvalidInput),
        ("forbidden", FailureClass::PermissionDenied),
        ("rejected", FailureClass::BusinessRejected),
    ] {
        let db = Db::new();
        let mut s = db.store();
        s.start(&request()).unwrap();
        let c = Time::new(1000);
        let l = lease(&mut s, &c, "one");
        let p = call(&mut s, &l, &c);
        s.observe_effect(
            &l,
            &p.attempt_id,
            &Observation::NotApplied {
                code: code.into(),
                class,
                message: "provider refused before applying".into(),
            },
            &c,
        )
        .unwrap();
        assert_eq!(
            s.effects("demo-effect-release", 0, 10).unwrap().items[0].status,
            EffectStatus::Failed { code: code.into() }
        );
        assert!(matches!(s.claim_effect(&l, &c).unwrap(), EffectClaim::Idle));
    }
    let db = Db::new();
    let mut s = db.store();
    let mut req = request();
    req.bundle.effect_bindings[0].policy.retry.max_calls = 2;
    s.start(&req).unwrap();
    let c = Time::new(1000);
    let first = lease(&mut s, &c, "one");
    let p = call(&mut s, &first, &c);
    let fail = Observation::NotApplied {
        code: "retry".into(),
        class: FailureClass::Transient,
        message: "not applied".into(),
    };
    s.observe_effect(&first, &p.attempt_id, &fail, &c).unwrap();
    let EffectClaim::Waiting {
        not_before_unix_ms: next,
    } = s.claim_effect(&first, &c).unwrap()
    else {
        panic!("backoff")
    };
    assert!((1005..=1010).contains(&next));
    s.release(&first, &c).unwrap();
    let current = lease(&mut s, &c, "two");
    assert_eq!(
        s.claim_effect(&current, &c).unwrap(),
        EffectClaim::Waiting {
            not_before_unix_ms: next
        }
    );
    c.set(next);
    let q = call(&mut s, &current, &c);
    assert_eq!(q.intent.operation_key, p.intent.operation_key);
    assert_eq!(q.number, 2);
    s.observe_effect(&current, &q.attempt_id, &fail, &c)
        .unwrap();
    assert_eq!(
        s.effects(&req.run_id, 0, 10).unwrap().items[0].status,
        EffectStatus::Failed {
            code: "retry".into()
        }
    );
    s.verify(&req.run_id).unwrap();
}
#[test]
fn rejection_of_new_attempt_cannot_erase_an_older_unknown_write() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let first = lease(&mut s, &c, "one");
    let p = call(&mut s, &first, &c);
    c.set(1100);
    let current = lease(&mut s, &c, "two");
    let q = call(&mut s, &current, &c);
    s.observe_effect(&current, &q.attempt_id, &Observation::Absent, &c)
        .unwrap();
    c.set(1150);
    let retry = call(&mut s, &current, &c);
    assert_eq!(retry.kind, CallKind::Write);
    assert_eq!(retry.intent, p.intent);
    s.observe_effect(
        &current,
        &retry.attempt_id,
        &Observation::NotApplied {
            code: "forbidden".into(),
            class: FailureClass::PermissionDenied,
            message: "current credentials revoked".into(),
        },
        &c,
    )
    .unwrap();
    assert!(matches!(
        s.effects("demo-effect-release", 0, 10).unwrap().items[0].status,
        EffectStatus::Retry {
            kind: CallKind::Query,
            ..
        }
    ));
}
#[test]
fn pause_drains_real_receipts_and_cancel_recovers_without_reissuing_write() {
    for pause in [true, false] {
        let db = Db::new();
        let mut s = db.store();
        s.start(&request()).unwrap();
        let c = Time::new(1000);
        let first = lease(&mut s, &c, "one");
        let p = call(&mut s, &first, &c);
        let before = s.get("demo-effect-release").unwrap();
        s.apply(&event(
            &before,
            "control",
            if pause {
                EventKind::Pause {
                    reason: "maintenance".into(),
                }
            } else {
                EventKind::Cancel
            },
        ))
        .unwrap();
        c.set(1001);
        if pause {
            assert_eq!(s.claim_effect(&first, &c).unwrap(), EffectClaim::Idle);
            s.observe_effect(&first, &p.attempt_id, &applied(&p), &c)
                .unwrap();
        } else {
            c.set(1100);
            let current = lease(&mut s, &c, "two");
            let q = call(&mut s, &current, &c);
            assert_eq!(q.kind, CallKind::Query);
            s.observe_effect(&current, &q.attempt_id, &applied(&q), &c)
                .unwrap();
            s.claim_effect(&current, &c).unwrap();
        }
        let effects = s.effects("demo-effect-release", 0, 10).unwrap();
        assert!(matches!(
            effects.items[0].status,
            EffectStatus::Applied { .. }
        ));
        if !pause {
            assert_eq!(
                s.get("demo-effect-release").unwrap().status,
                RunStatus::Cancelled
            );
        }
        s.verify("demo-effect-release").unwrap();
    }
}

pub(super) fn process(
    s: &mut SqliteRunStore,
    dir: &std::path::Path,
    mode: &str,
    slot: &str,
    phase: &str,
) {
    let l: Lease =
        workflow_worker::parse_message(&std::fs::read(dir.join("lease.json")).unwrap()).unwrap();
    let c = Time::new(1000);
    let hook = |at: &str| {
        if at == phase {
            std::fs::write(dir.join(format!("ready-{slot}")), at).unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    if mode == "effect-crash-claim" {
        s.claim_effect_internal(&l, &c, hook).unwrap();
        return;
    }
    if mode == "effect-crash-settle" {
        let p: EffectAttempt =
            workflow_worker::parse_message(&std::fs::read(dir.join("attempt.json")).unwrap())
                .unwrap();
        s.observe_effect_internal(&l, &p.attempt_id, &applied(&p), &c, hook)
            .unwrap();
        return;
    }
    if mode == "effect-race-claim" {
        std::fs::write(dir.join(format!("ready-{slot}")), "ready").unwrap();
        let end = Instant::now() + Duration::from_secs(20);
        while !dir.join("go").exists() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(
            dir.join(format!("result-{slot}")),
            document(&s.claim_effect(&l, &c)).unwrap(),
        )
        .unwrap();
        return;
    }
    let p = call(s, &l, &c);
    let binding =
        workflow_worker::parse_message(&std::fs::read(dir.join("http.json")).unwrap()).unwrap();
    let adapter = workflow_effect_http::HttpEffect::new(binding).unwrap();
    let observation = adapter
        .execute_with_secret(&p, &c, "sandbox-only-test-token")
        .unwrap();
    assert!(matches!(observation, Observation::Applied { .. }));
    // The provider has committed, while the authority has only its intent.
    std::fs::write(dir.join("attempt.json"), document(&p).unwrap()).unwrap();
    hook("provider_committed");
    s.observe_effect(&l, &p.attempt_id, &observation, &c)
        .unwrap();
}
#[test]
fn two_process_claim_race_dispatches_only_one_durable_attempt() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let l = lease(&mut s, &c, "one");
    std::fs::write(db.dir.join("lease.json"), document(&l).unwrap()).unwrap();
    drop(s);
    let mut one = child(&db, "effect-race-claim", "one", "");
    let mut two = child(&db, "effect-race-claim", "two", "");
    wait_file(&db.dir.join("ready-one"), &mut one);
    wait_file(&db.dir.join("ready-two"), &mut two);
    std::fs::write(db.dir.join("go"), "go").unwrap();
    assert!(one.wait().unwrap().success());
    assert!(two.wait().unwrap().success());
    let results: Vec<Result<EffectClaim>> = ["one", "two"]
        .into_iter()
        .map(|name| {
            workflow_worker::parse_message(
                &std::fs::read(db.dir.join(format!("result-{name}"))).unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Ok(EffectClaim::Call { .. })))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(
                r,
                Err(Error {
                    code: ErrorCode::AttemptInProgress,
                    ..
                })
            ))
            .count(),
        1
    );
    let mut s = SqliteRunStore::open(&db.path).unwrap();
    assert_eq!(
        s.effects("demo-effect-release", 0, 10).unwrap().items[0]
            .calls
            .len(),
        1
    );
}
#[test]
fn killed_intent_and_receipt_transactions_recover_all_or_nothing() {
    for (mode, phases) in [
        (
            "effect-crash-claim",
            vec![
                "before_transaction",
                "intent_written",
                "before_commit",
                "after_commit",
            ],
        ),
        (
            "effect-crash-settle",
            vec![
                "before_transaction",
                "event_written",
                "state_written",
                "execution_written",
                "before_commit",
                "after_commit",
            ],
        ),
    ] {
        for phase in phases {
            let db = Db::new();
            let mut s = db.store();
            s.start(&request()).unwrap();
            let c = Time::new(1000);
            let l = lease(&mut s, &c, "one");
            std::fs::write(db.dir.join("lease.json"), document(&l).unwrap()).unwrap();
            if mode == "effect-crash-settle" {
                let p = call(&mut s, &l, &c);
                std::fs::write(db.dir.join("attempt.json"), document(&p).unwrap()).unwrap();
            }
            drop(s);
            let mut writer = child(&db, mode, "one", phase);
            wait_file(&db.dir.join("ready-one"), &mut writer);
            writer.kill().unwrap();
            writer.wait().unwrap();
            let mut s = SqliteRunStore::open(&db.path).unwrap();
            let e = s.effects("demo-effect-release", 0, 10).unwrap().items;
            if mode == "effect-crash-claim" {
                assert_eq!(e.len(), usize::from(phase == "after_commit"));
            } else {
                assert_eq!(
                    matches!(e[0].status, EffectStatus::Applied { .. }),
                    phase == "after_commit"
                );
                assert_eq!(
                    s.get("demo-effect-release").unwrap().revision,
                    if phase == "after_commit" { 2 } else { 1 }
                );
                assert_eq!(
                    s.outbox("demo-effect-release", 0, 10, true)
                        .unwrap()
                        .items
                        .len(),
                    usize::from(phase != "after_commit")
                );
            }
            s.verify("demo-effect-release").unwrap();
        }
    }
}
struct Gateway {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Gateway {
    fn new(path: PathBuf) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA synchronous=FULL;CREATE TABLE releases(operation_key TEXT PRIMARY KEY,intent_digest TEXT NOT NULL,receipt TEXT NOT NULL);CREATE TABLE calls(kind TEXT NOT NULL,operation_key TEXT NOT NULL);").unwrap();
        drop(c);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            let c = Connection::open(path).unwrap();
            c.pragma_update(None, "synchronous", "FULL").unwrap();
            while !stopping.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = vec![];
                let mut buffer = [0u8; 4096];
                let (header_end, len) = loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    assert!(bytes.len() < 2_100_000);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = std::str::from_utf8(&bytes[..pos]).unwrap();
                        let length = header
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|s| s.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (pos + 4, length);
                    }
                };
                while bytes.len() < header_end + len {
                    let n = socket.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
                assert!(
                    header
                        .to_ascii_lowercase()
                        .contains("authorization: bearer sandbox-only-test-token")
                );
                let p: EffectAttempt =
                    workflow_worker::parse_message(&bytes[header_end..header_end + len]).unwrap();
                assert!(header.contains(&p.intent.operation_key));
                let path = header.split_whitespace().nth(1).unwrap();
                let hash = digest(&p.intent).unwrap();
                c.execute(
                    "INSERT INTO calls VALUES(?1,?2)",
                    rusqlite::params![path, p.intent.operation_key],
                )
                .unwrap();
                let observation = if path == "/write" {
                    let value = applied(&p);
                    c.execute(
                        "INSERT OR IGNORE INTO releases VALUES(?1,?2,?3)",
                        rusqlite::params![p.intent.operation_key, hash, document(&value).unwrap()],
                    )
                    .unwrap();
                    let stored: (String, String) = c
                        .query_row(
                            "SELECT intent_digest,receipt FROM releases WHERE operation_key=?1",
                            [&p.intent.operation_key],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .unwrap();
                    assert_eq!(stored.0, hash);
                    workflow_worker::parse_message(stored.1.as_bytes()).unwrap()
                } else {
                    assert_eq!(path, "/query");
                    use rusqlite::OptionalExtension;
                    let stored:Option<String>=c.query_row("SELECT receipt FROM releases WHERE operation_key=?1 AND intent_digest=?2",rusqlite::params![p.intent.operation_key,hash],|r|r.get(0)).optional().unwrap();
                    stored.map_or(Observation::Absent, |s| {
                        workflow_worker::parse_message(s.as_bytes()).unwrap()
                    })
                };
                let body = document(&workflow_effects::EffectReply {
                    request_digest: digest(&p).unwrap(),
                    observation,
                })
                .unwrap();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}
#[test]
fn real_http_write_then_killed_worker_is_queried_and_duplicate_delivery_creates_one_release() {
    let db = Db::new();
    let mut s = db.store();
    let req = request();
    s.start(&req).unwrap();
    let c = Time::new(1000);
    let l = s
        .acquire(
            &LeaseRequest {
                run_id: req.run_id.clone(),
                owner: "one".into(),
                acquisition_id: "claim-one".into(),
                ttl_ms: 10000,
            },
            &c,
        )
        .unwrap();
    let gateway = Gateway::new(db.dir.join("provider.db"));
    let policy = &req.bundle.effect_bindings[0].policy;
    let binding = workflow_effect_http::HttpEffectBinding {
        workspace: None,
        schema_version: 1,
        target: policy.target.clone(),
        call_identity: policy.call_identity.clone(),
        capability: req.bundle.capabilities[0].clone(),
        endpoint: gateway.url.clone(),
        credential: None,
        api_key_env: "SANDBOX_TEST_TOKEN".into(),
        allow_loopback_http: true,
    };
    std::fs::write(db.dir.join("http.json"), document(&binding).unwrap()).unwrap();
    std::fs::write(db.dir.join("lease.json"), document(&l).unwrap()).unwrap();
    drop(s);
    let mut writer = child(&db, "effect-http-kill", "one", "provider_committed");
    wait_file(&db.dir.join("ready-one"), &mut writer);
    writer.kill().unwrap();
    writer.wait().unwrap();
    let original: EffectAttempt =
        workflow_worker::parse_message(&std::fs::read(db.dir.join("attempt.json")).unwrap())
            .unwrap();
    let mut s = SqliteRunStore::open(&db.path).unwrap();
    assert_eq!(s.get(&req.run_id).unwrap().revision, 1);
    // Simulate concurrent duplicate queue delivery at the gateway boundary.
    let mut deliveries = vec![];
    for _ in 0..2 {
        let b = binding.clone();
        let p = original.clone();
        deliveries.push(std::thread::spawn(move || {
            workflow_effect_http::HttpEffect::new(b)
                .unwrap()
                .execute_with_secret(&p, &Time::new(1001), "sandbox-only-test-token")
                .unwrap()
        }));
    }
    for d in deliveries {
        assert_eq!(d.join().unwrap(), applied(&original));
    }
    c.set(11000);
    let current = s
        .acquire(
            &LeaseRequest {
                run_id: req.run_id.clone(),
                owner: "two".into(),
                acquisition_id: "claim-two".into(),
                ttl_ms: 10000,
            },
            &c,
        )
        .unwrap();
    let q = call(&mut s, &current, &c);
    assert_eq!(q.kind, CallKind::Query);
    let result = workflow_effect_http::HttpEffect::new(binding)
        .unwrap()
        .execute_with_secret(&q, &c, "sandbox-only-test-token")
        .unwrap();
    assert_eq!(
        s.observe_effect(&current, &q.attempt_id, &result, &c)
            .unwrap()
            .snapshot
            .status,
        RunStatus::Succeeded
    );
    let provider = Connection::open(db.dir.join("provider.db")).unwrap();
    assert_eq!(
        provider
            .query_row("SELECT count(*) FROM releases", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        provider
            .query_row("SELECT count(*) FROM calls WHERE kind='/write'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
        3
    );
    assert_eq!(
        provider
            .query_row("SELECT count(*) FROM calls WHERE kind='/query'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
    s.verify(&req.run_id).unwrap();
}

#[test]
fn loop_instances_use_distinct_effect_keys_and_restart_preserves_each_receipt() {
    let db = Db::new();
    let mut s = db.store();
    let mut scenario = scenario("repair-third-round");
    for d in &mut scenario.bundle.capabilities {
        d.effects = EffectContract::Write {
            irreversible: false,
            idempotency: Idempotency::Key {
                scope: "loop-action".into(),
                retention_ms: 60000,
            },
            query: None,
            compensation: None,
        };
    }
    let policy = request().bundle.effect_bindings.remove(0).policy;
    for w in &scenario.bundle.workflows {
        for n in &w.nodes {
            if matches!(n.kind, workflow_ir::NodeKind::Task { .. }) {
                scenario
                    .bundle
                    .effect_bindings
                    .push(workflow_effects::EffectBinding {
                        release: None,
                        depends_on: vec![],
                        compensates: None,
                        workflow: workflow_ir::VersionRef {
                            id: w.id.clone(),
                            version: w.version.clone(),
                        },
                        node_id: n.id.clone(),
                        policy: policy.clone(),
                    });
            }
        }
    }
    let req = start(&scenario);
    s.start(&req).unwrap();
    let c = Time::new(1000);
    let l = s
        .acquire(
            &LeaseRequest {
                run_id: req.run_id.clone(),
                owner: "loop".into(),
                acquisition_id: "loop-owner".into(),
                ttl_ms: 10000,
            },
            &c,
        )
        .unwrap();
    let mut keys = std::collections::BTreeSet::new();
    for e in &scenario.events {
        c.set(e.at_unix_ms);
        let p = loop {
            match s.claim_effect(&l, &c).unwrap() {
                EffectClaim::Call { attempt } => break *attempt,
                EffectClaim::Idle => {
                    assert!(matches!(
                        s.claim_next(&l, &c).unwrap(),
                        Claimed::Handled { .. }
                    ));
                }
                other => panic!("{other:?}"),
            }
        };
        let EventKind::TaskCompleted {
            instance_id,
            result: TaskResult::Succeeded { outputs },
        } = &e.kind
        else {
            panic!("fixture task result")
        };
        assert_eq!(*instance_id, p.intent.instance_id);
        assert!(keys.insert(p.intent.operation_key.clone()));
        let receipt = EffectReceipt {
            release: None,
            operation_key: p.intent.operation_key.clone(),
            intent_digest: digest(&p.intent).unwrap(),
            target: p.intent.policy.target.clone(),
            resource_id: format!("iteration-{}", p.intent.instance_id),
            provider_receipt: format!("receipt-{}", p.intent.instance_id),
            outputs: outputs.clone(),
        };
        s.observe_effect(&l, &p.attempt_id, &Observation::Applied { receipt }, &c)
            .unwrap();
        drop(s);
        s = SqliteRunStore::open(&db.path).unwrap();
    }
    assert_eq!(keys.len(), 6);
    assert_eq!(s.get(&req.run_id).unwrap().status, RunStatus::Succeeded);
    let first = s.effects(&req.run_id, 0, 2).unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(
        s.effects(&req.run_id, first.next_cursor.unwrap(), 10)
            .unwrap()
            .items
            .len(),
        4
    );
    s.verify(&req.run_id).unwrap();
}
struct Jump(std::cell::RefCell<std::collections::VecDeque<u64>>);
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
fn expired_commit_rolls_back_intent_or_receipt_and_late_truth_can_settle_after_renewal() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let l = lease(&mut s, &c, "one");
    assert_eq!(
        s.claim_effect(&l, &Jump(std::cell::RefCell::new([1000, 1100].into())))
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    assert!(
        s.effects("demo-effect-release", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
    let p = call(&mut s, &l, &c);
    assert_eq!(
        s.observe_effect(
            &l,
            &p.attempt_id,
            &applied(&p),
            &Jump(std::cell::RefCell::new([1000, 1100].into()))
        )
        .unwrap_err()
        .code,
        ErrorCode::LeaseConflict
    );
    assert_eq!(s.get("demo-effect-release").unwrap().revision, 1);
    c.set(1050);
    let renewed = s.renew(&l, 100, &c).unwrap();
    c.set(1110);
    assert!(c.0.get() > p.deadline_unix_ms);
    assert_eq!(
        s.observe_effect(&renewed, &p.attempt_id, &applied(&p), &c)
            .unwrap()
            .snapshot
            .status,
        RunStatus::Succeeded
    );
    s.verify("demo-effect-release").unwrap();
}
#[test]
fn replay_rejects_a_receipt_and_transition_whose_effect_journal_proof_was_removed() {
    let db = Db::new();
    let mut s = db.store();
    s.start(&request()).unwrap();
    let c = Time::new(1000);
    let l = lease(&mut s, &c, "one");
    let p = call(&mut s, &l, &c);
    let head: (i64, String) = s
        .connection
        .query_row(
            "SELECT revision,chain_digest FROM execution_heads",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    s.observe_effect(&l, &p.attempt_id, &applied(&p), &c)
        .unwrap();
    s.connection
        .execute_batch(
            "DROP TRIGGER immutable_execution_delete;DROP TRIGGER monotonic_execution_head;",
        )
        .unwrap();
    s.connection
        .execute("DELETE FROM execution_events WHERE sequence>?1", [head.0])
        .unwrap();
    s.connection
        .execute(
            "UPDATE execution_heads SET revision=?1,chain_digest=?2",
            rusqlite::params![head.0, head.1],
        )
        .unwrap();
    assert_eq!(
        s.verify("demo-effect-release").unwrap_err().code,
        ErrorCode::CorruptStorage
    );
}

#[test]
fn due_workflow_deadlines_block_new_writes_and_expiry_during_intent_commit_rolls_back() {
    for during_commit in [false, true] {
        let db = Db::new();
        let mut s = db.store();
        let mut scenario = scenario("repair-third-round");
        for w in &mut scenario.bundle.workflows {
            for node in &mut w.nodes {
                if let workflow_ir::NodeKind::Loop { deadline_ms, .. } = &mut node.kind {
                    *deadline_ms = 10;
                }
            }
        }
        for d in &mut scenario.bundle.capabilities {
            d.effects = EffectContract::Write {
                irreversible: false,
                idempotency: Idempotency::Key {
                    scope: "bounded-loop".into(),
                    retention_ms: 60000,
                },
                query: None,
                compensation: None,
            };
        }
        let policy = request().bundle.effect_bindings.remove(0).policy;
        for w in &scenario.bundle.workflows {
            for n in &w.nodes {
                if matches!(n.kind, workflow_ir::NodeKind::Task { .. }) {
                    scenario
                        .bundle
                        .effect_bindings
                        .push(workflow_effects::EffectBinding {
                            release: None,
                            depends_on: vec![],
                            compensates: None,
                            workflow: workflow_ir::VersionRef {
                                id: w.id.clone(),
                                version: w.version.clone(),
                            },
                            node_id: n.id.clone(),
                            policy: policy.clone(),
                        });
                }
            }
        }
        let req = start(&scenario);
        s.start(&req).unwrap();
        let c = Time::new(1000);
        let l = s
            .acquire(
                &LeaseRequest {
                    run_id: req.run_id.clone(),
                    owner: "deadline".into(),
                    acquisition_id: "bounded-loop".into(),
                    ttl_ms: 100,
                },
                &c,
            )
            .unwrap();
        assert!(matches!(
            s.claim_next(&l, &c).unwrap(),
            Claimed::Handled { .. }
        ));
        let error = if during_commit {
            s.claim_effect(&l, &Jump(std::cell::RefCell::new([1000, 1010].into())))
                .unwrap_err()
        } else {
            c.set(1010);
            s.claim_effect(&l, &c).unwrap_err()
        };
        assert_eq!(
            error.code,
            if during_commit {
                ErrorCode::LeaseConflict
            } else {
                ErrorCode::TransitionRejected
            }
        );
        assert!(s.effects(&req.run_id, 0, 100).unwrap().items.is_empty());
        s.verify(&req.run_id).unwrap();
    }
}
