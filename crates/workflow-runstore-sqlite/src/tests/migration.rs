use super::*;
use workflow_worker::Clock;
struct Time(u64);
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0)
    }
}
fn request(old: &StartRun) -> MigrationRequest {
    let mut target = old.bundle.clone();
    target.root.version = "2.0.0".into();
    target.workflows[0].version = "2.0.0".into();
    target.workflows[0].nodes.retain(|n| n.id != "implement");
    target.workflows[0].edges.retain(|e| e.from != "implement");
    for edge in &mut target.workflows[0].edges {
        if edge.to == "implement" {
            edge.to = "done".into();
        }
    }
    let wait = target.workflows[0]
        .nodes
        .iter_mut()
        .find(|n| n.id == "review")
        .unwrap();
    wait.kind = workflow_ir::NodeKind::Wait {
        event: "design-review".into(),
        timeout_ms: 10000,
    };
    target.capabilities.clear();
    MigrationRequest {
        migration_id: "reviewed-upgrade".into(),
        target_bundle: target,
        target_inputs: old.inputs.clone(),
        execution_policy: workflow_kernel::MigrationExecutionPolicy::RestartWithFreshEvidence,
        timer_policy: workflow_kernel::MigrationTimerPolicy::CancelAndRearmOnResume,
        node_mapping: vec![],
        decision_summary: "Remove obsolete implementation and require new approval".into(),
    }
}
fn prepared(db: &Db) -> (StartRun, Snapshot, Lease, MigrationPlan) {
    let start = start(&scenario("review-approved"));
    let mut store = db.store();
    let initial = store.start(&start).unwrap().snapshot;
    let paused = store
        .apply(&event(
            &initial,
            "pause-migration",
            EventKind::Pause {
                reason: "review upgrade".into(),
            },
        ))
        .unwrap()
        .snapshot;
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: start.run_id.clone(),
                owner: "operator".into(),
                acquisition_id: "migration-owner".into(),
                ttl_ms: 10000,
            },
            &Time(1002),
        )
        .unwrap();
    let plan = store
        .plan_migration(&start.run_id, &request(&start))
        .unwrap();
    (start, paused, lease, plan)
}

#[test]
fn migration_commits_target_and_authority_atomically_preserves_history_and_fences_old_owner() {
    let db = Db::new();
    let (start, old, lease, plan) = prepared(&db);
    let mut store = db.store();
    assert!(
        store
            .apply(&event(
                &old,
                "raw-migration",
                EventKind::MigrateDefinition {
                    plan: Box::new(plan.clone())
                }
            ))
            .is_err()
    );
    let mut tampered = plan.clone();
    tampered.invalidated_instances.clear();
    assert!(
        store
            .migrate_definition(&lease, &tampered, "operator", &Time(1002))
            .is_err()
    );
    assert_eq!(store.get(&start.run_id).unwrap(), old);
    let migrated = store
        .migrate_definition(&lease, &plan, "operator", &Time(1002))
        .unwrap();
    assert_eq!(migrated.snapshot.bundle_digest, plan.target_bundle_digest);
    assert!(migrated.snapshot.pause.is_some());
    assert_eq!(
        store
            .historical_snapshot(&start.run_id, old.revision)
            .unwrap(),
        old
    );
    assert!(
        store
            .migrate_definition(&lease, &plan, "operator", &Time(20000))
            .unwrap()
            .transition
            .duplicate
    );
    assert!(
        store
            .migrate_definition(&lease, &plan, "another-operator", &Time(1002))
            .is_err()
    );
    assert!(store.claim_next(&lease, &Time(1003)).is_err());
    drop(store);
    let mut reopened = db.store();
    assert_eq!(reopened.get(&start.run_id).unwrap(), migrated.snapshot);
    assert_eq!(
        reopened.bundle(&start.run_id).unwrap().root.version,
        "2.0.0"
    );
    let latest = reopened
        .acquire(
            &LeaseRequest {
                acquisition_id: "new-generation".into(),
                ..LeaseRequest {
                    run_id: start.run_id.clone(),
                    owner: "operator".into(),
                    acquisition_id: String::new(),
                    ttl_ms: 10000,
                }
            },
            &Time(1003),
        )
        .unwrap();
    assert_ne!(latest.generation, lease.generation);
    let resumed = reopened
        .apply(&event(
            &migrated.snapshot,
            "resume-v2",
            EventKind::Resume {
                reason: "accept new version".into(),
            },
        ))
        .unwrap()
        .snapshot;
    assert_eq!(
        resumed.frames[&1].nodes["review"].state,
        workflow_kernel::NodeState::Waiting {
            deadline_unix_ms: resumed.now_unix_ms + 10000
        }
    );
    let approved = reopened
        .apply(&event(
            &resumed,
            "approve-v2",
            EventKind::Signal {
                instance_id: resumed.frames[&1].nodes["review"].instance_id,
                event: "design-review".into(),
                accepted: true,
                outputs: Values::new(),
            },
        ))
        .unwrap();
    assert_eq!(approved.snapshot.status, RunStatus::Succeeded);
    reopened.verify(&start.run_id).unwrap();
    assert_eq!(
        reopened
            .historical_snapshot(&start.run_id, old.revision)
            .unwrap(),
        old
    );
    assert_eq!(
        reopened
            .connection
            .query_row("SELECT count(*) FROM bundles", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
}

#[test]
fn failed_definition_migration_rolls_back_new_bindings_state_receipts_and_execution() {
    let db = Db::new();
    let (start, old, lease, plan) = prepared(&db);
    let mut store = db.store();
    let history = store.execution_history(&start.run_id, 0, 100).unwrap();
    let before = store.outbox(&start.run_id, 0, 100, false).unwrap();
    struct Expiring(std::cell::Cell<u32>);
    impl Clock for Expiring {
        fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
            let count = self.0.get();
            self.0.set(count + 1);
            Ok(if count == 0 { 1002 } else { 50000 })
        }
    }
    assert!(
        store
            .migrate_definition(
                &lease,
                &plan,
                "operator",
                &Expiring(std::cell::Cell::new(0))
            )
            .is_err()
    );
    assert_eq!(store.get(&start.run_id).unwrap(), old);
    assert_eq!(
        store.execution_history(&start.run_id, 0, 100).unwrap(),
        history
    );
    assert_eq!(store.outbox(&start.run_id, 0, 100, false).unwrap(), before);
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM bundles", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM binding_locks WHERE version='2.0.0'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
    assert!(
        !store
            .migrate_definition(&lease, &plan, "operator", &Time(1002))
            .unwrap()
            .transition
            .duplicate
    );
}
