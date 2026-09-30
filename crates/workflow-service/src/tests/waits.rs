use super::*;
use workflow_kernel::{SignalDecision, SignalMessage, SignalStatus, WaitKind};
use workflow_runstore_postgres::access::{RunControl, RunControlRequest};

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn tls_wait_survives_service_restart_and_resumes_only_from_the_authorized_current_response() {
    let tenant = format!(
        "wait-tls-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let author = credential(&mut service, &admin, "author", Role::DefinitionMaintainer);
    let human = credential(&mut service, &admin, "approver", Role::Approver);
    let ci = credential(&mut service, &admin, "ci", Role::SignalSource);
    let worker = credential(&mut service, &admin, "worker", Role::Worker);
    for (kind, id) in [
        (WaitKind::HumanApproval, "human"),
        (WaitKind::ExternalEvent, "ci"),
    ] {
        let mut request = fixture(id);
        let root = &mut request.bundle.workflows[0];
        root.id = "durable-wait-demo".into();
        root.entry = "review".into();
        root.nodes
            .retain(|n| ["review", "done", "declined", "expired"].contains(&n.id.as_str()));
        root.edges.retain(|e| e.from == "review");
        request.bundle.root.id = root.id.clone();
        request.bundle.wait_policies[0].workflow = request.bundle.root.clone();
        let policy = &mut request.bundle.wait_policies[0].policy;
        policy.kind = kind;
        policy.identity.id = id.into();
        if kind == WaitKind::ExternalEvent {
            policy.responders = ["ci".into()].into();
        }
        let h = Harness::new(true);
        let (_, author_client) = h.client(&author);
        let (_, runner_client) = h.client(&runner);
        call(
            &author_client,
            Operation::Publish {
                bundle: Box::new(request.bundle.clone()),
            },
        )
        .unwrap();
        let Response::Committed(started) = call(
            &runner_client,
            Operation::Start {
                request: Box::new(request.clone()),
            },
        )
        .unwrap() else {
            panic!()
        };
        let Response::Waits(waits) = call(
            &runner_client,
            Operation::Waits {
                run_id: id.into(),
                after: 0,
                limit: 100,
            },
        )
        .unwrap() else {
            panic!()
        };
        let wait = &waits.items[0];
        assert_eq!(wait.routes["timed_out"], "expired");
        assert_eq!(
            wait.subjects["review_digest"],
            request.inputs["review_digest"]
        );
        call(
            &runner_client,
            Operation::Control {
                request: RunControlRequest {
                    run_id: id.into(),
                    event_id: "pause".into(),
                    expected_revision: started.snapshot.revision,
                    control: RunControl::Pause {
                        reason: "deployment maintenance".into(),
                    },
                },
            },
        )
        .unwrap();
        // Kill the actual TLS server and drop every client session. All callbacks
        // during downtime must be retried by the upstream durable sender.
        drop(h);
        let h = Harness::new(true);
        let (_, runner_client) = h.client(&runner);
        let (_, human_client) = h.client(&human);
        let (_, ci_client) = h.client(&ci);
        let (_, worker_client) = h.client(&worker);
        let Response::Waits(recovered) = call(
            &runner_client,
            Operation::Waits {
                run_id: id.into(),
                after: 0,
                limit: 100,
            },
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(recovered.items[0].target, wait.target);
        assert_eq!(recovered.items[0].deadline_unix_ms, wait.deadline_unix_ms);
        assert!(recovered.items[0].paused);
        let response = SignalSubmission {
            schema_version: 1,
            run_id: id.into(),
            run_digest: started.snapshot.run_digest.clone(),
            message: SignalMessage {
                schema_version: 1,
                message_id: "response".into(),
                correlation_id: wait.correlation_id.clone(),
                target: wait.target.clone(),
                source: "model-text-is-not-identity".into(),
                exception: None,
                decision: SignalDecision::Approve,
                reason: "verified current design".into(),
                outputs: Values::new(),
                expires_at_unix_ms: wait.deadline_unix_ms,
            },
        };
        let operation = if kind == WaitKind::HumanApproval {
            Operation::Approve {
                request: Box::new(response.clone()),
            }
        } else {
            Operation::Signal {
                request: Box::new(response.clone()),
            }
        };
        assert_eq!(
            call(&worker_client, operation.clone()).unwrap_err().code,
            ErrorCode::Unauthorized
        );
        let (authorized, wrong_role) = if kind == WaitKind::HumanApproval {
            (&human_client, &ci_client)
        } else {
            (&ci_client, &human_client)
        };
        assert_eq!(
            call(wrong_role, operation.clone()).unwrap_err().code,
            ErrorCode::Unauthorized
        );
        let result = call(authorized, operation.clone()).unwrap();
        let receipt = match result {
            Response::Approved(r) | Response::SignalReceived(r) => r,
            _ => panic!(),
        };
        assert_eq!(receipt.entry.status, SignalStatus::Pending);
        let Response::Pending(pending) = call(
            &worker_client,
            Operation::Pending {
                after: "".into(),
                limit: 100,
            },
        )
        .unwrap() else {
            panic!()
        };
        assert!(pending.items.is_empty());
        call(
            &runner_client,
            Operation::Control {
                request: RunControlRequest {
                    run_id: id.into(),
                    event_id: "resume".into(),
                    expected_revision: receipt.run_revision,
                    control: RunControl::Resume {
                        reason: "maintenance completed".into(),
                    },
                },
            },
        )
        .unwrap();
        let result = call(authorized, operation).unwrap();
        let receipt = match result {
            Response::Approved(r) | Response::SignalReceived(r) => r,
            _ => panic!(),
        };
        assert!(receipt.duplicate);
        assert!(matches!(receipt.entry.status, SignalStatus::Applied { .. }));
        assert_eq!(
            receipt.entry.message.source,
            if kind == WaitKind::HumanApproval {
                "approver"
            } else {
                "ci"
            }
        );
        let Response::Snapshot(state) =
            call(&runner_client, Operation::Get { run_id: id.into() }).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, RunStatus::Succeeded);
        assert_eq!(state.inbox.len(), 1);
    }
}
