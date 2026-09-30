use super::*;
use workflow_kernel::{
    SignalDecision, SignalMessage, SignalRejection, SignalStatus, WaitKind, WaitPolicy,
    WaitPolicyBinding,
};

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn approval_policy_versions_are_immutable_and_unbound_waits_have_no_remote_bypass() {
    let mut f = Fixture::new();
    let author = f.credential("author", Role::DefinitionMaintainer);
    let runner = f.credential("starter", Role::Runner);
    let human = f.credential("reviewer", Role::Approver);
    let mut r = wait_request(WaitKind::HumanApproval, "unbound-review");
    f.service
        .publish(author.expose_secret(), &r.bundle)
        .unwrap();
    r.bundle.wait_policies[0]
        .policy
        .responders
        .insert("another-person".into());
    assert_eq!(
        f.service
            .publish(author.expose_secret(), &r.bundle)
            .unwrap_err()
            .code,
        ErrorCode::BindingConflict
    );
    r.bundle.wait_policies[0].policy.identity.version = "2.0.0".into();
    f.service
        .publish(author.expose_secret(), &r.bundle)
        .unwrap();
    r.bundle.wait_policies.clear();
    f.service
        .publish(author.expose_secret(), &r.bundle)
        .unwrap();
    let state = f
        .service
        .start(runner.expose_secret(), &r)
        .unwrap()
        .snapshot;
    let wait = f
        .service
        .waits(human.expose_secret(), &r.run_id, 0, 100)
        .unwrap()
        .items
        .remove(0);
    let signal = SignalSubmission {
        schema_version: 1,
        run_id: r.run_id.clone(),
        run_digest: state.run_digest.clone(),
        message: SignalMessage {
            schema_version: 1,
            message_id: "unbound".into(),
            correlation_id: wait.correlation_id,
            target: wait.target,
            source: "reviewer".into(),
            exception: None,
            decision: SignalDecision::Approve,
            reason: "cannot bypass frozen policy".into(),
            outputs: Values::new(),
            expires_at_unix_ms: wait.deadline_unix_ms,
        },
    };
    assert_eq!(
        f.service
            .approve(human.expose_secret(), &signal)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        f.service.get(runner.expose_secret(), &r.run_id).unwrap(),
        state
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn access_v1_migration_adds_signal_role_without_rewriting_credentials_or_audit() {
    let name = format!(
        "workflow_wait_migration_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut admin = client();
    admin
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .unwrap();
    let mut config: postgres::Config = std::env::var("WORKFLOW_TEST_POSTGRES")
        .unwrap()
        .parse()
        .unwrap();
    config.dbname(&name);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut c = config.connect(postgres::NoTls).unwrap();
        let credential =
            AuthenticatedService::bootstrap(&mut c, "tenant", "project", "admin", MAX_TTL).unwrap();
        c.batch_execute("ALTER TABLE workflow_access.credentials DROP CONSTRAINT credentials_role_check;
            ALTER TABLE workflow_access.credentials ADD CONSTRAINT credentials_role_check
            CHECK(role IN ('administrator','definition_maintainer','viewer','runner','approver','scheduler','worker','recovery'));
            UPDATE workflow_access.schema_version SET version=1").unwrap();
        let before: i64 = c
            .query_one("SELECT count(*) FROM workflow_access.audit", &[])
            .unwrap()
            .get(0);
        AuthenticatedService::migrate_access(&mut c).unwrap();
        AuthenticatedService::migrate_access(&mut c).unwrap();
        let version: i32 = c
            .query_one("SELECT version FROM workflow_access.schema_version", &[])
            .unwrap()
            .get(0);
        assert_eq!(version, 2);
        let after: i64 = c
            .query_one("SELECT count(*) FROM workflow_access.audit", &[])
            .unwrap()
            .get(0);
        assert_eq!(before, after);
        let mut service = AuthenticatedService::open(c).unwrap();
        service
            .issue(
                credential.expose_secret(),
                "ci",
                Role::SignalSource,
                &[],
                MAX_TTL,
            )
            .unwrap();
    }));
    admin
        .batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn wait_request(channel: WaitKind, id: &str) -> StartRun {
    let mut r = request(id);
    let root = &mut r.bundle.workflows[0];
    root.id = "scoped-review".into();
    root.entry = "review".into();
    root.nodes
        .retain(|n| ["review", "done", "declined", "expired"].contains(&n.id.as_str()));
    root.edges.retain(|e| e.from == "review");
    r.bundle.root.id = root.id.clone();
    r.bundle.wait_policies.clear();
    r.bundle.wait_policies.push(WaitPolicyBinding {
        workflow: r.bundle.root.clone(),
        node_id: "review".into(),
        policy: WaitPolicy {
            identity: workflow_ir::VersionRef {
                id: if channel == WaitKind::HumanApproval {
                    "human"
                } else {
                    "external"
                }
                .into(),
                version: "1.0.0".into(),
            },
            kind: channel,
            responders: [if channel == WaitKind::HumanApproval {
                "reviewer"
            } else {
                "ci"
            }
            .into()]
            .into(),
            subjects: [("review_digest".into(), workflow_kernel::SubjectKind::Digest)].into(),
            max_validity_ms: 86400000,
            exception: None,
        },
    });
    r
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn wait_policy_authenticates_channel_actor_expiry_and_durable_duplicate_receipts() {
    let mut f = Fixture::new();
    AuthenticatedService::migrate_access(&mut client()).unwrap();
    AuthenticatedService::migrate_access(&mut client()).unwrap();
    let runner = f.credential("starter", Role::Runner);
    let author = f.credential("author", Role::DefinitionMaintainer);
    let human = f.credential("reviewer", Role::Approver);
    let impostor = f.credential("impostor", Role::Approver);
    let ci = f.credential("ci", Role::SignalSource);
    let worker = f.credential("worker", Role::Worker);
    for (channel, id) in [
        (WaitKind::HumanApproval, "human-review"),
        (WaitKind::ExternalEvent, "ci-event"),
    ] {
        let r = wait_request(channel, id);
        f.service
            .publish(author.expose_secret(), &r.bundle)
            .unwrap();
        let state = f
            .service
            .start(runner.expose_secret(), &r)
            .unwrap()
            .snapshot;
        let wait = f
            .service
            .waits(runner.expose_secret(), id, 0, 100)
            .unwrap()
            .items
            .remove(0);
        assert_eq!(wait.policy, Some(r.bundle.wait_policies[0].policy.clone()));
        let now = workflow_worker::SystemClock.now_unix_ms().unwrap();
        let signal = SignalSubmission {
            schema_version: 1,
            run_id: id.into(),
            run_digest: state.run_digest,
            message: SignalMessage {
                exception: None,
                schema_version: 1,
                message_id: "review-response".into(),
                correlation_id: wait.correlation_id,
                target: wait.target,
                source: "forged-payload-actor".into(),
                decision: SignalDecision::Approve,
                reason: "explicit review of frozen inputs".into(),
                outputs: Values::new(),
                expires_at_unix_ms: now + 60000,
            },
        };
        assert_eq!(
            f.service
                .approve(worker.expose_secret(), &signal)
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
        assert_eq!(
            f.service
                .signal(human.expose_secret(), &signal)
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
        assert_eq!(
            f.service
                .approve(ci.expose_secret(), &signal)
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
        if channel == WaitKind::HumanApproval {
            assert_eq!(
                f.service
                    .signal(ci.expose_secret(), &signal)
                    .unwrap_err()
                    .code,
                ErrorCode::Unauthorized
            );
            let mut forged = signal.clone();
            forged.message.message_id = "wrong-actor".into();
            let denied = f
                .service
                .approve(impostor.expose_secret(), &forged)
                .unwrap();
            assert!(matches!(
                denied.entry.status,
                SignalStatus::Rejected {
                    reason: SignalRejection::ResponderNotAllowed,
                    ..
                }
            ));
            let mut expired = signal.clone();
            expired.message.message_id = "expired-review".into();
            expired.message.expires_at_unix_ms = now - 1;
            assert!(matches!(
                f.service
                    .approve(human.expose_secret(), &expired)
                    .unwrap()
                    .entry
                    .status,
                SignalStatus::Rejected {
                    reason: SignalRejection::Expired,
                    ..
                }
            ));
        } else {
            assert_eq!(
                f.service
                    .approve(human.expose_secret(), &signal)
                    .unwrap_err()
                    .code,
                ErrorCode::Unauthorized
            );
        }
        let receipt = match channel {
            WaitKind::HumanApproval => f.service.approve(human.expose_secret(), &signal),
            WaitKind::ExternalEvent => f.service.signal(ci.expose_secret(), &signal),
        }
        .unwrap();
        assert!(matches!(receipt.entry.status, SignalStatus::Applied { .. }));
        assert_eq!(
            receipt.entry.message.source,
            *r.bundle.wait_policies[0].policy.responders.first().unwrap()
        );
        let mut reopened = AuthenticatedService::open(client()).unwrap();
        let duplicate = match channel {
            WaitKind::HumanApproval => reopened.approve(human.expose_secret(), &signal),
            WaitKind::ExternalEvent => reopened.signal(ci.expose_secret(), &signal),
        }
        .unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.entry, receipt.entry);
        assert_eq!(
            reopened.get(runner.expose_secret(), id).unwrap().status,
            RunStatus::Succeeded
        );
    }
    let audit = f.service.audit(f.admin.expose_secret(), 0, 100).unwrap();
    assert!(
        audit
            .items
            .iter()
            .any(|a| a.operation == "signal" && a.actor == "ci" && a.outcome == "accepted")
    );
    assert!(
        audit
            .items
            .iter()
            .any(|a| a.operation == "approve" && a.actor == "reviewer" && a.outcome == "accepted")
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn early_events_survive_restart_and_lost_acknowledgements_in_receive_order() {
    let mut f = Fixture::new();
    let runner = f.credential("starter", Role::Runner);
    let author = f.credential("author", Role::DefinitionMaintainer);
    let ci = f.credential("ci", Role::SignalSource);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    let worker = f.credential("worker", Role::Worker);
    let mut r = request("early-event");
    r.bundle.wait_policies[0].policy.kind = WaitKind::ExternalEvent;
    r.bundle.wait_policies[0].policy.identity.id = "ci-notification".into();
    r.bundle.wait_policies[0].policy.responders = ["ci".into()].into();
    r.bundle.wait_policies[0].policy.exception = None;
    f.service
        .publish(author.expose_secret(), &r.bundle)
        .unwrap();
    let initial = f
        .service
        .start(runner.expose_secret(), &r)
        .unwrap()
        .snapshot;
    let target = workflow_kernel::WaitTarget {
        instance_id: initial.frames[&1].nodes["review"].instance_id,
        definition_digest: initial.frames[&1].definition_digest.clone(),
        input_digest: workflow_worker::digest(&r.inputs).unwrap(),
        event: "release-review".into(),
    };
    let mut callback = SignalSubmission {
        schema_version: 1,
        run_id: r.run_id.clone(),
        run_digest: initial.run_digest.clone(),
        message: SignalMessage {
            schema_version: 1,
            message_id: "z-first".into(),
            correlation_id: workflow_kernel::signal_correlation(&initial.run_digest, &target)
                .unwrap(),
            target,
            source: "untrusted-text".into(),
            exception: None,
            decision: SignalDecision::Approve,
            reason: "CI completed for frozen review digest".into(),
            outputs: Values::new(),
            expires_at_unix_ms: workflow_worker::SystemClock.now_unix_ms().unwrap() + 120000,
        },
    };
    // Derive the event subscription from the same immutable definition.
    if let workflow_ir::NodeKind::Wait { event, .. } = &r.bundle.workflows[0]
        .nodes
        .iter()
        .find(|n| n.id == "review")
        .unwrap()
        .kind
    {
        callback.message.target.event = event.clone();
        callback.message.correlation_id =
            workflow_kernel::signal_correlation(&initial.run_digest, &callback.message.target)
                .unwrap();
    }
    assert_eq!(
        f.service
            .signal(ci.expose_secret(), &callback)
            .unwrap()
            .entry
            .status,
        SignalStatus::Pending
    );
    let first = callback.clone();
    callback.message.message_id = "a-second".into();
    callback.message.decision = SignalDecision::Reject;
    assert_eq!(
        f.service
            .signal(ci.expose_secret(), &callback)
            .unwrap()
            .entry
            .status,
        SignalStatus::Pending
    );
    let mut reopened = AuthenticatedService::open(client()).unwrap();
    assert!(
        reopened
            .signal(ci.expose_secret(), &first)
            .unwrap()
            .duplicate
    );
    let lease = reopened
        .acquire(scheduler.expose_secret(), &r.run_id, "new-session", 120000)
        .unwrap();
    for _ in 0..100 {
        match reopened
            .dispatch(scheduler.expose_secret(), &lease, &worker.id)
            .unwrap()
        {
            Dispatch::Task { assignment_id } => {
                let task = reopened
                    .assignment(worker.expose_secret(), &assignment_id)
                    .unwrap();
                reopened
                    .finish(worker.expose_secret(), &assignment_id, &execute(&task))
                    .unwrap();
            }
            Dispatch::Handled => {}
            Dispatch::Idle => break,
        }
    }
    reopened.release(scheduler.expose_secret(), &lease).unwrap();
    let state = reopened.get(runner.expose_secret(), &r.run_id).unwrap();
    assert_eq!(state.status, RunStatus::Succeeded);
    assert!(matches!(
        state.inbox["z-first"].status,
        SignalStatus::Applied { .. }
    ));
    assert!(matches!(
        state.inbox["a-second"].status,
        SignalStatus::Rejected {
            reason: SignalRejection::AlreadySettled,
            ..
        }
    ));
    assert!(
        reopened
            .pending(worker.expose_secret(), "", 100)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        reopened
            .outstanding(f.admin.expose_secret(), "", 100)
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn timeout_cancel_and_response_serialize_to_one_transition_after_scheduler_restart() {
    let mut f = Fixture::new();
    let runner = f.credential("starter", Role::Runner);
    let author = f.credential("author", Role::DefinitionMaintainer);
    let human = f.credential("reviewer", Role::Approver);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    let mut r = wait_request(WaitKind::HumanApproval, "timeout-race");
    r.bundle.workflows[0].version = "2.0.0".into();
    r.bundle.root.version = "2.0.0".into();
    r.bundle.wait_policies[0].workflow = r.bundle.root.clone();
    if let workflow_ir::NodeKind::Wait { timeout_ms, .. } = &mut r.bundle.workflows[0].nodes[0].kind
    {
        *timeout_ms = 1500;
    }
    f.service
        .publish(author.expose_secret(), &r.bundle)
        .unwrap();
    let initial = f
        .service
        .start(runner.expose_secret(), &r)
        .unwrap()
        .snapshot;
    let wait = f
        .service
        .waits(human.expose_secret(), &r.run_id, 0, 100)
        .unwrap()
        .items
        .remove(0);
    let lease = f
        .service
        .acquire(
            scheduler.expose_secret(),
            &r.run_id,
            "restarted-scheduler",
            30000,
        )
        .unwrap();
    let signal = SignalSubmission {
        schema_version: 1,
        run_id: r.run_id.clone(),
        run_digest: initial.run_digest,
        message: SignalMessage {
            schema_version: 1,
            message_id: "racing-response".into(),
            correlation_id: wait.correlation_id,
            target: wait.target,
            source: "forged".into(),
            exception: None,
            decision: SignalDecision::Approve,
            reason: "review race".into(),
            outputs: Values::new(),
            expires_at_unix_ms: wait.deadline_unix_ms + 10000,
        },
    };
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let b = barrier.clone();
    let tick = std::thread::spawn(move || {
        let mut s = AuthenticatedService::open(client()).unwrap();
        b.wait();
        s.tick(scheduler.expose_secret(), &lease)
    });
    let b = barrier.clone();
    let response = std::thread::spawn(move || {
        let mut s = AuthenticatedService::open(client()).unwrap();
        b.wait();
        s.approve(human.expose_secret(), &signal).unwrap()
    });
    let remaining = wait
        .deadline_unix_ms
        .saturating_sub(workflow_worker::SystemClock.now_unix_ms().unwrap());
    std::thread::sleep(std::time::Duration::from_millis(remaining));
    barrier.wait();
    let cancelled = f.service.control(
        runner.expose_secret(),
        &RunControlRequest {
            run_id: r.run_id.clone(),
            event_id: "racing-cancel".into(),
            expected_revision: initial.revision,
            control: RunControl::Cancel,
        },
    );
    tick.join().unwrap().unwrap();
    let receipt = response.join().unwrap();
    assert!(matches!(
        receipt.entry.status,
        SignalStatus::Rejected {
            reason: SignalRejection::WaitExpired
                | SignalRejection::RunCancelled
                | SignalRejection::AlreadySettled,
            ..
        }
    ));
    let state = f.service.get(runner.expose_secret(), &r.run_id).unwrap();
    assert!(matches!(
        state.status,
        RunStatus::Failed | RunStatus::Cancelled
    ));
    if let Err(e) = cancelled {
        assert_eq!(e.code, ErrorCode::TransitionRejected);
    }
    assert!(
        state
            .inbox
            .values()
            .all(|e| !matches!(e.status, SignalStatus::Applied { .. }))
    );
}
