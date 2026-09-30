use super::*;
mod artifacts;
mod effects;
mod migration;
mod restoration;
mod waits;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn client() -> Client {
    Client::connect(
        &std::env::var("WORKFLOW_TEST_POSTGRES").expect("disposable test database required"),
        postgres::NoTls,
    )
    .unwrap()
}
fn request(id: &str) -> StartRun {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    let mut r: StartRun = serde_json::from_slice(
        &std::fs::read(root.join("examples/approval/protected-start.json")).unwrap(),
    )
    .unwrap();
    r.run_id = id.into();
    r
}
struct Fixture {
    service: AuthenticatedService,
    admin: IssuedCredential,
    tenant: String,
}
impl Fixture {
    fn new() -> Self {
        let tenant = format!(
            "auth-{}-{}-{}",
            std::process::id(),
            workflow_worker::SystemClock.now_unix_ms().unwrap(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let admin = AuthenticatedService::bootstrap(
            &mut client(),
            &tenant,
            "project",
            "security-admin",
            MAX_TTL,
        )
        .unwrap();
        let mut service = AuthenticatedService::open(client()).unwrap();
        let maintainer = service
            .issue(
                admin.expose_secret(),
                "maintainer",
                Role::DefinitionMaintainer,
                &[],
                MAX_TTL,
            )
            .unwrap();
        service
            .publish(maintainer.expose_secret(), &request("x").bundle)
            .unwrap();
        Self {
            service,
            admin,
            tenant,
        }
    }
    fn credential(&mut self, actor: &str, role: Role) -> IssuedCredential {
        let caps = if role == Role::Worker {
            rules()
        } else {
            vec![]
        };
        self.service
            .issue(self.admin.expose_secret(), actor, role, &caps, MAX_TTL)
            .unwrap()
    }
}
fn rules() -> Vec<CapabilityRule> {
    request("x")
        .bundle
        .capabilities
        .into_iter()
        .map(|d| {
            let c = workflow_worker::Capability::new(d).unwrap();
            CapabilityRule {
                model_policy: None,
                effect: None,
                id: c.descriptor().capability.id.clone(),
                version: c.descriptor().capability.version.clone(),
                contract_digest: c.digest().into(),
                artifacts: None,
            }
        })
        .collect()
}
fn next(
    service: &mut AuthenticatedService,
    scheduler: &IssuedCredential,
    lease: &Lease,
    worker: &IssuedCredential,
) -> String {
    for _ in 0..50 {
        match service
            .dispatch(scheduler.expose_secret(), lease, &worker.id)
            .unwrap()
        {
            Dispatch::Task { assignment_id } => return assignment_id,
            Dispatch::Handled => {}
            Dispatch::Idle => panic!("expected task"),
        }
    }
    panic!("dispatch did not reach task")
}
fn execute(task: &PreparedTask) -> workflow_worker::WorkResult {
    workflow_builtin_capabilities::worker()
        .unwrap()
        .execute(&task.request, &task.grant)
        .unwrap()
        .into_result()
}
#[test]
fn bearer_shape_and_debug_do_not_disclose_secret() {
    for value in [
        "",
        "token",
        "wf1_é",
        &"x".repeat(68),
        &format!("wf1_{}", "Z".repeat(64)),
    ] {
        assert_eq!(
            token_digest(value).unwrap_err().code,
            ErrorCode::Unauthorized
        );
    }
    let secret = random("wf1_").unwrap();
    assert!(token_digest(&secret).is_ok());
    let issued = IssuedCredential {
        id: "id".into(),
        secret,
        expires_at_unix_ms: 1,
    };
    assert!(!format!("{issued:?}").contains(issued.expose_secret()));
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn execution_expiry_at_commit_is_retryable_without_expiring_the_credential() {
    let mut f = Fixture::new();
    let runner = f.credential("runner", Role::Runner);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    f.service
        .start(runner.expose_secret(), &request("deadline"))
        .unwrap();
    let prepared = std::cell::Cell::new(false);
    let result = f.service.transact(
        scheduler.expose_secret(),
        &[Role::Scheduler],
        "acquire",
        "deadline",
        |tx, who| {
            who.change(tx, "deadline", false, |s, c| {
                s.acquire(
                    &LeaseRequest {
                        run_id: "deadline".into(),
                        owner: who.id.clone(),
                        acquisition_id: "delayed-commit".into(),
                        ttl_ms: 2000,
                    },
                    c,
                )
            })?;
            prepared.set(true);
            tx.query_one("SELECT pg_sleep(2.05)", &[])
                .map_err(storage)?;
            Ok(())
        },
    );
    assert!(prepared.get());
    assert_eq!(result.unwrap_err().code, ErrorCode::LeaseConflict);
    // The speculative lease was rolled back. The same credential still works.
    let lease = f
        .service
        .acquire(scheduler.expose_secret(), "deadline", "retry", 120000)
        .unwrap();
    assert_eq!(lease.epoch, 1);
    assert!(f.service.get(runner.expose_secret(), "deadline").is_ok());
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn authenticated_scope_roles_dispatch_result_and_approval_contract() {
    let mut f = Fixture::new();
    let mut b = Fixture::new();
    let runner = f.credential("starter", Role::Runner);
    let viewer = f.credential("reader", Role::Viewer);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    let worker = f.credential("worker", Role::Worker);
    let another = f.credential("other-worker", Role::Worker);
    let approver = f.credential("reviewer", Role::Approver);
    let other = b.credential("other-tenant", Role::Viewer);
    let other_worker = b.credential("other-worker", Role::Worker);
    let r = request("private-run");
    for token in [
        f.admin.expose_secret(),
        viewer.expose_secret(),
        worker.expose_secret(),
        approver.expose_secret(),
    ] {
        assert_eq!(
            f.service.start(token, &r).unwrap_err().code,
            ErrorCode::Unauthorized
        );
    }
    assert!(
        f.service
            .publish(runner.expose_secret(), &r.bundle)
            .is_err()
    );
    let maintainer = f.credential("author", Role::DefinitionMaintainer);
    assert!(f.service.start(maintainer.expose_secret(), &r).is_err());
    let mut unpublished = r.clone();
    unpublished.bundle.capabilities[0].usage.push_str(" edited");
    assert_eq!(
        f.service
            .start(runner.expose_secret(), &unpublished)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let other_project = AuthenticatedService::bootstrap(
        &mut client(),
        &f.tenant,
        "other-project",
        "security",
        MAX_TTL,
    )
    .unwrap();
    let project_viewer = f
        .service
        .issue(
            other_project.expose_secret(),
            "reader",
            Role::Viewer,
            &[],
            MAX_TTL,
        )
        .unwrap();
    let started = f.service.start(runner.expose_secret(), &r).unwrap();
    assert_eq!(
        f.service
            .get(project_viewer.expose_secret(), &r.run_id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(
        f.service
            .start(runner.expose_secret(), &r)
            .unwrap()
            .transition
            .duplicate
    );
    assert_eq!(
        f.service.get(viewer.expose_secret(), &r.run_id).unwrap(),
        started.snapshot
    );
    assert!(
        f.service
            .get(
                "wf1_0000000000000000000000000000000000000000000000000000000000000000",
                &r.run_id
            )
            .is_err()
    );
    assert_eq!(
        f.service
            .get(other.expose_secret(), &r.run_id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(
        f.service
            .history(other.expose_secret(), &r.run_id, 0, 100)
            .is_err()
    );
    assert!(
        f.service
            .inbox(other.expose_secret(), &r.run_id, 0, 100)
            .is_err()
    );
    assert_eq!(
        f.service
            .get(worker.expose_secret(), &r.run_id)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert!(
        f.service
            .issue(
                runner.expose_secret(),
                "escalated",
                Role::Administrator,
                &[],
                MAX_TTL
            )
            .is_err()
    );
    assert!(
        f.service
            .revoke(b.admin.expose_secret(), &worker.id)
            .is_err()
    );
    assert!(
        AuthenticatedService::bootstrap(&mut client(), &f.tenant, "project", "attacker", MAX_TTL)
            .is_err()
    );
    let lease = f
        .service
        .acquire(scheduler.expose_secret(), &r.run_id, "acquire", 120000)
        .unwrap();
    assert_eq!(lease.owner, scheduler.id);
    assert!(
        f.service
            .dispatch(scheduler.expose_secret(), &lease, &other_worker.id)
            .is_err()
    );
    let mut forged = lease.clone();
    forged.owner = "caller-authority".into();
    assert!(
        f.service
            .dispatch(scheduler.expose_secret(), &forged, &worker.id)
            .is_err()
    );
    // Wrong capability policy rolls the prepared attempt back with its outbox.
    let mut wrong_rules = rules();
    wrong_rules[0].contract_digest = format!("sha256:{}", "0".repeat(64));
    let wrong = f
        .service
        .issue(
            f.admin.expose_secret(),
            "wrong-contract",
            Role::Worker,
            &wrong_rules,
            MAX_TTL,
        )
        .unwrap();
    let mut rejected = false;
    for _ in 0..50 {
        let before = f.service.get(viewer.expose_secret(), &r.run_id).unwrap();
        match f
            .service
            .dispatch(scheduler.expose_secret(), &lease, &wrong.id)
        {
            Ok(Dispatch::Handled) => continue,
            Err(e) => {
                assert_eq!(e.code, ErrorCode::Unauthorized);
                assert_eq!(
                    before,
                    f.service.get(viewer.expose_secret(), &r.run_id).unwrap()
                );
                rejected = true;
                break;
            }
            other => panic!("expected task authorization denial: {other:?}"),
        }
    }
    assert!(rejected);
    let assignment = next(&mut f.service, &scheduler, &lease, &worker);
    for token in [
        another.expose_secret(),
        other_worker.expose_secret(),
        runner.expose_secret(),
        scheduler.expose_secret(),
    ] {
        assert!(f.service.assignment(token, &assignment).is_err());
    }
    let task = f
        .service
        .assignment(worker.expose_secret(), &assignment)
        .unwrap();
    let result = execute(&task);
    let mut forged = result.clone();
    forged.request_digest = format!("sha256:{}", "0".repeat(64));
    assert!(
        f.service
            .finish(worker.expose_secret(), &assignment, &forged)
            .is_err()
    );
    assert!(
        f.service
            .finish(other_worker.expose_secret(), &assignment, &result)
            .is_err()
    );
    let mut extra = result.clone();
    if let workflow_worker::AdapterOutcome::Succeeded { outputs, .. } = &mut extra.outcome {
        outputs.insert("grant_admin".into(), true.into());
    }
    assert!(
        f.service
            .finish(worker.expose_secret(), &assignment, &extra)
            .is_err()
    );
    assert!(
        !f.service
            .finish(worker.expose_secret(), &assignment, &result)
            .unwrap()
            .duplicate
    );
    assert!(
        f.service
            .finish(worker.expose_secret(), &assignment, &result)
            .unwrap()
            .duplicate
    );
    assert_eq!(
        f.service
            .assignment(worker.expose_secret(), &assignment)
            .unwrap_err()
            .code,
        ErrorCode::ReceiptConflict
    );
    // Complete the real builtin graph, then approve with server-stamped identity.
    for _ in 0..100 {
        match f
            .service
            .dispatch(scheduler.expose_secret(), &lease, &worker.id)
            .unwrap()
        {
            Dispatch::Task { assignment_id } => {
                let task = f
                    .service
                    .assignment(worker.expose_secret(), &assignment_id)
                    .unwrap();
                f.service
                    .finish(worker.expose_secret(), &assignment_id, &execute(&task))
                    .unwrap();
            }
            Dispatch::Handled => {}
            Dispatch::Idle => break,
        }
    }
    let target = f
        .service
        .waits(approver.expose_secret(), &r.run_id, 0, 100)
        .unwrap()
        .items
        .remove(0);
    let mut signal = SignalSubmission {
        schema_version: 1,
        run_id: r.run_id.clone(),
        run_digest: started.snapshot.run_digest,
        message: workflow_kernel::SignalMessage {
            exception: None,
            schema_version: 1,
            message_id: "review".into(),
            correlation_id: target.correlation_id,
            target: target.target,
            source: "forged-administrator".into(),
            decision: workflow_kernel::SignalDecision::Approve,
            reason: "review complete".into(),
            outputs: Default::default(),
            expires_at_unix_ms: target.deadline_unix_ms,
        },
    };
    assert!(f.service.approve(worker.expose_secret(), &signal).is_err());
    assert!(f.service.approve(runner.expose_secret(), &signal).is_err());
    let receipt = f
        .service
        .approve(approver.expose_secret(), &signal)
        .unwrap();
    assert!(
        serde_json::to_string(&receipt)
            .unwrap()
            .contains("reviewer")
    );
    assert!(
        !serde_json::to_string(&receipt)
            .unwrap()
            .contains("forged-administrator")
    );
    signal.message.source = "different-forged-source".into();
    assert!(
        f.service
            .approve(approver.expose_secret(), &signal)
            .unwrap()
            .duplicate
    );
    assert_eq!(
        f.service
            .get(viewer.expose_secret(), &r.run_id)
            .unwrap()
            .status,
        RunStatus::Succeeded
    );
    assert!(f.service.audit(worker.expose_secret(), 0, 100).is_err());
    let audit = f.service.audit(f.admin.expose_secret(), 0, 100).unwrap();
    assert!(audit.items.iter().any(|a| a.outcome == "denied"));
    let serialized = serde_json::to_string(&audit).unwrap();
    for c in [&f.admin, &runner, &worker, &scheduler] {
        assert!(!serialized.contains(c.expose_secret()));
    }
    // Verify secrets are absent from every persisted run, assignment and audit.
    let rows = client()
        .query(
            "SELECT image FROM workflow_authority.runs WHERE tenant=$1",
            &[&f.tenant],
        )
        .unwrap();
    for row in rows {
        let bytes: Vec<u8> = row.get(0);
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains(worker.expose_secret())
        );
    }
    let rows = client()
        .query(
            "SELECT row_to_json(a)::text FROM workflow_access.assignments a WHERE tenant=$1",
            &[&f.tenant],
        )
        .unwrap();
    for row in rows {
        assert!(!row.get::<_, String>(0).contains(worker.expose_secret()));
    }
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn worker_revocation_rotation_old_lease_and_expiry_reject_results() {
    let mut f = Fixture::new();
    let runner = f.credential("starter", Role::Runner);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    let worker = f.credential("worker", Role::Worker);
    f.service
        .start(runner.expose_secret(), &request("revoke"))
        .unwrap();
    let lease = f
        .service
        .acquire(scheduler.expose_secret(), "revoke", "first", 120000)
        .unwrap();
    let assignment = next(&mut f.service, &scheduler, &lease, &worker);
    let task = f
        .service
        .assignment(worker.expose_secret(), &assignment)
        .unwrap();
    let result = execute(&task);
    let replacement = f
        .service
        .rotate(f.admin.expose_secret(), &worker.id, MAX_TTL)
        .unwrap();
    assert_ne!(replacement.expose_secret(), worker.expose_secret());
    assert!(
        f.service
            .finish(worker.expose_secret(), &assignment, &result)
            .is_err()
    );
    assert!(
        f.service
            .finish(replacement.expose_secret(), &assignment, &result)
            .is_err()
    );
    let outstanding = f
        .service
        .outstanding(f.admin.expose_secret(), "", 100)
        .unwrap();
    assert!(
        outstanding
            .items
            .iter()
            .any(|a| a.assignment_id == assignment && a.worker_revoked)
    );
    f.service
        .release(scheduler.expose_secret(), &lease)
        .unwrap();
    let fresh = f
        .service
        .acquire(scheduler.expose_secret(), "revoke", "second", 120000)
        .unwrap();
    assert!(fresh.epoch > lease.epoch);
    let next_assignment = next(&mut f.service, &scheduler, &fresh, &replacement);
    let task = f
        .service
        .assignment(replacement.expose_secret(), &next_assignment)
        .unwrap();
    let result = execute(&task);
    f.service
        .release(scheduler.expose_secret(), &fresh)
        .unwrap();
    assert_eq!(
        f.service
            .finish(replacement.expose_secret(), &next_assignment, &result)
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    // A live credential at admission can expire after a speculative start. The
    // final authority fence must discard both state and successful response.
    let short = f
        .service
        .issue(f.admin.expose_secret(), "brief", Role::Runner, &[], 2000)
        .unwrap();
    let result = f.service.transact(
        short.expose_secret(),
        &[Role::Runner],
        "start",
        "expired",
        |tx, who| {
            who.change(tx, "expired", true, |s, c| {
                let mut r = request("expired");
                r.started_at_unix_ms = c.now_unix_ms()?;
                s.start(&r)
            })?;
            std::thread::sleep(std::time::Duration::from_millis(2050));
            Ok(())
        },
    );
    assert_eq!(result.unwrap_err().code, ErrorCode::Unauthorized);
    assert_eq!(
        f.service
            .get(runner.expose_secret(), "expired")
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(f.service.get(short.expose_secret(), "revoke").is_err());
    f.service
        .revoke(f.admin.expose_secret(), &replacement.id)
        .unwrap();
    assert!(
        f.service
            .assignment(replacement.expose_secret(), &next_assignment)
            .is_err()
    );
}

fn wait_lock(pid: i32) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut c = client();
    loop {
        if c.query_one(
            "SELECT coalesce(wait_event_type='Lock',false) FROM pg_stat_activity WHERE pid=$1",
            &[&pid],
        )
        .unwrap()
        .get::<_, bool>(0)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "expected real row-lock wait"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn revocation_and_submission_have_a_database_lock_order() {
    let mut f = Fixture::new();
    let runner = f.credential("starter", Role::Runner);
    let runner_secret = runner.expose_secret().to_owned();
    let admin_secret = f.admin.expose_secret().to_owned();
    let id = runner.id.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let op = std::thread::spawn(move || {
        let mut s = AuthenticatedService::open(client()).unwrap();
        s.transact(
            &runner_secret,
            &[Role::Runner],
            "start",
            "ordered",
            |tx, who| {
                who.change(tx, "ordered", true, |s, c| {
                    let mut r = request("ordered");
                    r.started_at_unix_ms = c.now_unix_ms()?;
                    s.start(&r)
                })?;
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(15))
                    .unwrap();
                Ok(())
            },
        )
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .unwrap();
    let mut c = client();
    let pid: i32 = c.query_one("SELECT pg_backend_pid()", &[]).unwrap().get(0);
    let revoke = std::thread::spawn(move || {
        AuthenticatedService::open(c)
            .unwrap()
            .revoke(&admin_secret, &id)
    });
    wait_lock(pid);
    release_tx.send(()).unwrap();
    op.join().unwrap().unwrap();
    revoke.join().unwrap().unwrap();
    assert_eq!(
        f.service
            .get(runner.expose_secret(), "ordered")
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let viewer = f.credential("viewer", Role::Viewer);
    assert!(f.service.get(viewer.expose_secret(), "ordered").is_ok());
    // Reverse order: authentication blocks behind an uncommitted revocation and
    // must examine the updated row after it wakes (READ COMMITTED + FOR SHARE).
    let runner = f.credential("second", Role::Runner);
    let mut c = client();
    let mut revoke = c.transaction().unwrap();
    revoke
        .execute(
            "UPDATE workflow_access.credentials SET revoked=true WHERE id=$1",
            &[&runner.id],
        )
        .unwrap();
    let mut c = client();
    let pid: i32 = c.query_one("SELECT pg_backend_pid()", &[]).unwrap().get(0);
    let secret = runner.expose_secret().to_owned();
    let op = std::thread::spawn(move || {
        AuthenticatedService::open(c)
            .unwrap()
            .start(&secret, &request("must-not-appear"))
    });
    wait_lock(pid);
    revoke.commit().unwrap();
    assert_eq!(
        op.join().unwrap().unwrap_err().code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        f.service
            .get(viewer.expose_secret(), "must-not-appear")
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}
