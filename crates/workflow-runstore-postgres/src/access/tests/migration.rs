use super::*;
use workflow_kernel::{SignalDecision, SignalMessage};

fn versioned(id: &str, version: &str) -> StartRun {
    let mut r = request(id);
    let inspect = r.bundle.workflows[1]
        .nodes
        .iter()
        .find(|n| n.id == "inspect")
        .unwrap()
        .clone();
    let w = &mut r.bundle.workflows[0];
    w.id = "versioned-review".into();
    w.version = version.into();
    w.entry = "review".into();
    w.nodes
        .retain(|n| ["review", "done", "declined", "expired"].contains(&n.id.as_str()));
    w.nodes.push(inspect);
    w.edges.retain(|e| e.from == "review");
    w.edges.iter_mut().find(|e| e.id == "approved").unwrap().to = "inspect".into();
    w.edges.push(serde_json::from_value(serde_json::json!({"id":"inspected","from":"inspect","to":"done","route":{"type":"next"}})).unwrap());
    r.bundle.root.id = w.id.clone();
    r.bundle.root.version = version.into();
    r.bundle.workflows.truncate(1);
    r.bundle.wait_policies[0].workflow = r.bundle.root.clone();
    r
}
fn approval(
    f: &mut Fixture,
    runner: &IssuedCredential,
    r: &StartRun,
    id: &str,
) -> SignalSubmission {
    let state = f.service.get(runner.expose_secret(), &r.run_id).unwrap();
    let wait = f
        .service
        .waits(runner.expose_secret(), &r.run_id, 0, 100)
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    SignalSubmission {
        schema_version: 1,
        run_id: r.run_id.clone(),
        run_digest: state.run_digest,
        message: SignalMessage {
            schema_version: 1,
            message_id: id.into(),
            correlation_id: wait.correlation_id,
            target: wait.target,
            source: "ignored-client-actor".into(),
            decision: SignalDecision::Approve,
            reason: "reviewed frozen inputs".into(),
            outputs: Values::new(),
            expires_at_unix_ms: wait.deadline_unix_ms,
            exception: None,
        },
    }
}
fn migration_request(r: &StartRun) -> MigrationRequest {
    let mut target = versioned(&r.run_id, "2.0.0");
    let root = &mut target.bundle.workflows[0];
    root.nodes.retain(|n| n.id != "inspect");
    root.edges.retain(|e| e.from != "inspect");
    root.edges
        .iter_mut()
        .find(|e| e.id == "approved")
        .unwrap()
        .to = "done".into();
    root.nodes
        .iter_mut()
        .find(|n| n.id == "review")
        .unwrap()
        .kind = workflow_ir::NodeKind::Wait {
        event: "design-review".into(),
        timeout_ms: 120000,
    };
    target.bundle.capabilities.clear();
    target.inputs.insert(
        "review_digest".into(),
        format!("sha256:{}", "b".repeat(64)).into(),
    );
    MigrationRequest {
        migration_id: "review-upgrade".into(),
        target_bundle: target.bundle,
        target_inputs: target.inputs,
        execution_policy: workflow_kernel::MigrationExecutionPolicy::RestartWithFreshEvidence,
        timer_policy: workflow_kernel::MigrationTimerPolicy::CancelAndRearmOnResume,
        node_mapping: vec![],
        decision_summary: "Remove inspection, replace reviewed subject and rearm approval timeout"
            .into(),
    }
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn published_v2_does_not_move_waiting_v1_and_workers_route_by_exact_contract() {
    let mut f = Fixture::new();
    let author = f.credential("author", Role::DefinitionMaintainer);
    let runner = f.credential("runner", Role::Runner);
    let human = f.credential("reviewer", Role::Approver);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    let worker = f.credential("old-worker", Role::Worker);
    let v1 = versioned("waiting-v1", "1.0.0");
    f.service
        .publish(author.expose_secret(), &v1.bundle)
        .unwrap();
    let old = f
        .service
        .start(runner.expose_secret(), &v1)
        .unwrap()
        .snapshot;
    let mut v2 = versioned("new-v2", "2.0.0");
    v2.bundle.capabilities[0].capability.version = "2.0.0".into();
    if let workflow_ir::NodeKind::Task { capability, .. } = &mut v2.bundle.workflows[0]
        .nodes
        .iter_mut()
        .find(|n| n.id == "inspect")
        .unwrap()
        .kind
    {
        capability.version = "2.0.0".into();
    }
    f.service
        .publish(author.expose_secret(), &v2.bundle)
        .unwrap();
    assert_eq!(
        f.service.get(runner.expose_secret(), &v1.run_id).unwrap(),
        old
    );
    let new = f
        .service
        .start(runner.expose_secret(), &v2)
        .unwrap()
        .snapshot;
    assert_ne!(new.bundle_digest, old.bundle_digest);
    let c = workflow_worker::Capability::new(v2.bundle.capabilities[0].clone()).unwrap();
    let new_worker = f
        .service
        .issue(
            f.admin.expose_secret(),
            "new-worker",
            Role::Worker,
            &[CapabilityRule {
                id: c.descriptor().capability.id.clone(),
                version: "2.0.0".into(),
                contract_digest: c.digest().into(),
                artifacts: None,
                effect: None,
                model_policy: None,
            }],
            MAX_TTL,
        )
        .unwrap();
    let message = approval(&mut f, &runner, &v1, "approve-v1");
    f.service.approve(human.expose_secret(), &message).unwrap();
    let lease = f
        .service
        .acquire(scheduler.expose_secret(), &v1.run_id, "v1-owner", 120000)
        .unwrap();
    // Drain non-task intents; dispatching an incompatible worker must leave the
    // actual task unclaimed so a compatible worker can execute it afterwards.
    loop {
        match f
            .service
            .dispatch(scheduler.expose_secret(), &lease, &new_worker.id)
        {
            Ok(Dispatch::Handled) => {}
            Err(e) => {
                assert_eq!(e.code, ErrorCode::Unauthorized);
                break;
            }
            other => panic!("unexpected routing: {other:?}"),
        }
    }
    let id = next(&mut f.service, &scheduler, &lease, &worker);
    let task = f.service.assignment(worker.expose_secret(), &id).unwrap();
    assert_eq!(task.request.capability.version, "1.0.0");
    let result = execute(&task);
    f.service
        .finish(worker.expose_secret(), &id, &result)
        .unwrap();
    assert_eq!(
        f.service
            .get(runner.expose_secret(), &v1.run_id)
            .unwrap()
            .status,
        RunStatus::Succeeded
    );
    let message = approval(&mut f, &runner, &v2, "approve-v2");
    f.service.approve(human.expose_secret(), &message).unwrap();
    let lease = f
        .service
        .acquire(scheduler.expose_secret(), &v2.run_id, "v2-owner", 120000)
        .unwrap();
    let id = next(&mut f.service, &scheduler, &lease, &new_worker);
    assert_eq!(
        f.service
            .assignment(new_worker.expose_secret(), &id)
            .unwrap()
            .request
            .capability
            .version,
        "2.0.0"
    );
    assert_eq!(
        f.service
            .historical_snapshot(runner.expose_secret(), &v1.run_id, old.revision)
            .unwrap(),
        old
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn definition_migration_is_admin_scoped_cas_audited_and_invalidates_old_approval() {
    let mut f = Fixture::new();
    let author = f.credential("author", Role::DefinitionMaintainer);
    let runner = f.credential("runner", Role::Runner);
    let human = f.credential("reviewer", Role::Approver);
    let other = Fixture::new();
    let r = versioned("migrate", "1.0.0");
    f.service
        .publish(author.expose_secret(), &r.bundle)
        .unwrap();
    let old = f
        .service
        .start(runner.expose_secret(), &r)
        .unwrap()
        .snapshot;
    let old_message = approval(&mut f, &runner, &r, "old-approval");
    let paused = f
        .service
        .control(
            runner.expose_secret(),
            &RunControlRequest {
                run_id: r.run_id.clone(),
                event_id: "pause".into(),
                expected_revision: old.revision,
                control: RunControl::Pause {
                    reason: "review migration".into(),
                },
            },
        )
        .unwrap()
        .snapshot;
    let request = migration_request(&r);
    assert!(
        f.service
            .plan_migration(f.admin.expose_secret(), &r.run_id, &request)
            .is_err()
    );
    f.service
        .publish(author.expose_secret(), &request.target_bundle)
        .unwrap();
    for token in [
        runner.expose_secret(),
        human.expose_secret(),
        author.expose_secret(),
        other.admin.expose_secret(),
    ] {
        assert!(
            f.service
                .plan_migration(token, &r.run_id, &request)
                .is_err()
        );
    }
    let plan = f
        .service
        .plan_migration(f.admin.expose_secret(), &r.run_id, &request)
        .unwrap();
    assert!(
        plan.inputs_changed
            && plan.nodes.iter().any(|n| n.target.is_none())
            && !plan.timers.is_empty()
    );
    let lease = f
        .service
        .acquire(
            f.admin.expose_secret(),
            &r.run_id,
            "admin-migration",
            120000,
        )
        .unwrap();
    let mut tampered = plan.clone();
    tampered.invalidated_instances.clear();
    assert!(
        f.service
            .migrate_definition(f.admin.expose_secret(), &lease, &tampered)
            .is_err()
    );
    assert!(
        f.service
            .migrate_definition(runner.expose_secret(), &lease, &plan)
            .is_err()
    );
    assert_eq!(
        f.service.get(runner.expose_secret(), &r.run_id).unwrap(),
        paused
    );
    let migrated = f
        .service
        .migrate_definition(f.admin.expose_secret(), &lease, &plan)
        .unwrap();
    assert_eq!(migrated.snapshot.bundle_digest, plan.target_bundle_digest);
    assert!(
        f.service
            .migrate_definition(f.admin.expose_secret(), &lease, &plan)
            .unwrap()
            .transition
            .duplicate
    );
    assert!(f.service.release(f.admin.expose_secret(), &lease).is_err());
    let resumed = f
        .service
        .control(
            runner.expose_secret(),
            &RunControlRequest {
                run_id: r.run_id.clone(),
                event_id: "resume".into(),
                expected_revision: migrated.snapshot.revision,
                control: RunControl::Resume {
                    reason: "approved explicit plan".into(),
                },
            },
        )
        .unwrap();
    assert_eq!(
        f.service
            .approve(human.expose_secret(), &old_message)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let new_message = approval(&mut f, &runner, &r, "new-approval");
    assert_ne!(new_message.message.target, old_message.message.target);
    f.service
        .approve(human.expose_secret(), &new_message)
        .unwrap();
    assert_eq!(
        f.service
            .get(runner.expose_secret(), &r.run_id)
            .unwrap()
            .status,
        RunStatus::Succeeded
    );
    assert_eq!(
        f.service
            .historical_snapshot(f.admin.expose_secret(), &r.run_id, paused.revision)
            .unwrap(),
        paused
    );
    assert!(resumed.snapshot.pause.is_none());
    let audit = f.service.audit(f.admin.expose_secret(), 0, 100).unwrap();
    assert!(
        audit
            .items
            .iter()
            .any(|e| e.operation == "migrate_definition"
                && e.outcome == "accepted"
                && e.actor == "security-admin")
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn shared_image_storage_upgrade_requires_reviewed_source_and_preserves_frozen_history() {
    let mut f = Fixture::new();
    let runner = f.credential("runner", Role::Runner);
    let r = request("storage-upgrade");
    let old = f
        .service
        .start(runner.expose_secret(), &r)
        .unwrap()
        .snapshot;
    let mut c = client();
    let row=c.query_one("SELECT image FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2",&[&f.tenant,&r.run_id]).unwrap();
    let bytes: Vec<u8> = row.get(0);
    let mut image: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    image["schema_version"] = 10.into();
    image["tables"].as_array_mut().unwrap().pop();
    let legacy = serde_json::to_vec(&image).unwrap();
    c.execute("UPDATE workflow_authority.runs SET image=$3,image_digest=$4 WHERE tenant=$1 AND project='project' AND run_id=$2",&[&f.tenant,&r.run_id,&legacy,&hash(&legacy)]).unwrap();
    assert_eq!(
        f.service
            .get(runner.expose_secret(), &r.run_id)
            .unwrap_err()
            .code,
        ErrorCode::UnsupportedStorage
    );
    assert!(
        f.service
            .plan_storage_upgrade(runner.expose_secret(), &r.run_id)
            .is_err()
    );
    let plan = f
        .service
        .plan_storage_upgrade(f.admin.expose_secret(), &r.run_id)
        .unwrap();
    let mut wrong = plan.clone();
    wrong.verified_runs += 1;
    assert!(
        f.service
            .upgrade_storage(f.admin.expose_secret(), &r.run_id, &wrong)
            .is_err()
    );
    let untouched:Vec<u8>=c.query_one("SELECT image FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2",&[&f.tenant,&r.run_id]).unwrap().get(0);
    assert_eq!(untouched, legacy);
    assert_eq!(
        f.service
            .upgrade_storage(f.admin.expose_secret(), &r.run_id, &plan)
            .unwrap(),
        plan
    );
    assert_eq!(
        f.service
            .upgrade_storage(f.admin.expose_secret(), &r.run_id, &plan)
            .unwrap(),
        plan
    );
    assert_eq!(
        f.service.get(runner.expose_secret(), &r.run_id).unwrap(),
        old
    );
}
