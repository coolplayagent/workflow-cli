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

#[test]
fn repeated_definition_migrations_preserve_each_historical_bundle_and_old_idempotency() {
    let db = Db::new();
    let (start, old, lease, first) = prepared(&db);
    let mut store = db.store();
    let v2 = store
        .migrate_definition(&lease, &first, "operator", &Time(1002))
        .unwrap()
        .snapshot;
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: start.run_id.clone(),
                owner: "operator".into(),
                acquisition_id: "second".into(),
                ttl_ms: 10000,
            },
            &Time(1003),
        )
        .unwrap();
    let mut req = first.request.clone();
    req.migration_id = "third-version".into();
    req.target_bundle.root.version = "3.0.0".into();
    req.target_bundle.workflows[0].version = "3.0.0".into();
    let second = store.plan_migration(&start.run_id, &req).unwrap();
    let v3 = store
        .migrate_definition(&lease, &second, "operator", &Time(1003))
        .unwrap()
        .snapshot;
    assert_ne!(v2.bundle_digest, v3.bundle_digest);
    assert_ne!(
        v2.frames[&1].nodes["review"].instance_id,
        v3.frames[&1].nodes["review"].instance_id
    );
    drop(store);
    let mut restored = db.store();
    assert_eq!(
        restored
            .historical_snapshot(&start.run_id, old.revision)
            .unwrap(),
        old
    );
    assert_eq!(
        restored
            .historical_snapshot(&start.run_id, v2.revision)
            .unwrap(),
        v2
    );
    assert_eq!(restored.get(&start.run_id).unwrap(), v3);
    assert!(
        restored
            .migrate_definition(&lease, &first, "operator", &Time(20000))
            .unwrap()
            .transition
            .duplicate
    );
    restored.verify(&start.run_id).unwrap();
}

#[test]
fn migration_process() {
    let Ok(path) = std::env::var("WORKFLOW_MIGRATION_DB") else {
        return;
    };
    let path = PathBuf::from(path);
    let dir = path.parent().unwrap();
    let phase = std::env::var("WORKFLOW_MIGRATION_PHASE").unwrap();
    let slot = std::env::var("WORKFLOW_MIGRATION_SLOT").unwrap();
    let hook = |at: &str| {
        if phase == at {
            std::fs::write(dir.join(format!("ready-{slot}")), []).unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    if std::env::var("WORKFLOW_MIGRATION_STORAGE").is_ok() {
        let mut c = connect(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        c.pragma_update(None, "cache_size", 1).unwrap();
        crate::storage_upgrade::upgrade_connection(&mut c, None, None, hook).unwrap();
        return;
    }
    let lease: Lease =
        workflow_worker::parse_message(&std::fs::read(dir.join("lease.json")).unwrap()).unwrap();
    let plan: MigrationPlan =
        workflow_worker::parse_message(&std::fs::read(dir.join("plan.json")).unwrap()).unwrap();
    let mut store = SqliteRunStore::open(&path).unwrap();
    store
        .connection
        .pragma_update(None, "cache_size", 1)
        .unwrap();
    if phase == "race" {
        std::fs::write(dir.join(format!("ready-{slot}")), []).unwrap();
        while !dir.join("go").exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let result = store
        .migrate_definition_internal(&lease, &plan, "operator", &Time(1002), hook)
        .unwrap();
    std::fs::write(
        dir.join(format!("result-{slot}")),
        result.transition.duplicate.to_string(),
    )
    .unwrap();
}

fn process(db: &Db, phase: &str, slot: &str, storage: bool) -> std::process::Child {
    let mut child = Process::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "tests::migration::migration_process",
            "--nocapture",
        ])
        .env("WORKFLOW_MIGRATION_DB", &db.path)
        .env("WORKFLOW_MIGRATION_PHASE", phase)
        .env("WORKFLOW_MIGRATION_SLOT", slot)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if storage {
        child.env("WORKFLOW_MIGRATION_STORAGE", "1");
    }
    child.spawn().unwrap()
}

#[test]
fn definition_migration_process_crashes_are_atomic_and_concurrent_duplicate_is_exact() {
    for phase in [
        "before_transaction",
        "target_retained",
        "event_written",
        "state_written",
        "commands_retired",
        "execution_written",
        "before_commit",
        "after_commit",
        "race",
    ] {
        let db = Db::new();
        let (start, old, lease, plan) = prepared(&db);
        std::fs::write(
            db.dir.join("lease.json"),
            workflow_worker::to_message(&lease).unwrap(),
        )
        .unwrap();
        std::fs::write(
            db.dir.join("plan.json"),
            workflow_worker::to_message(&plan).unwrap(),
        )
        .unwrap();
        let mut child = process(&db, phase, "one", false);
        wait_file(&db.dir.join("ready-one"), &mut child);
        if phase == "race" {
            let mut other = process(&db, phase, "two", false);
            wait_file(&db.dir.join("ready-two"), &mut other);
            std::fs::write(db.dir.join("go"), []).unwrap();
            assert!(child.wait().unwrap().success());
            assert!(other.wait().unwrap().success());
            let mut replies = ["one", "two"]
                .map(|n| std::fs::read_to_string(db.dir.join(format!("result-{n}"))).unwrap());
            replies.sort();
            assert_eq!(replies, ["false", "true"]);
        } else {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let mut store = db.store();
        let state = store.get(&start.run_id).unwrap();
        if !matches!(phase, "after_commit" | "race") {
            assert_eq!(state, old, "{phase}");
        } else {
            assert_eq!(state.bundle_digest, plan.target_bundle_digest, "{phase}");
        }
        let receipt = store
            .migrate_definition(&lease, &plan, "operator", &Time(1002))
            .unwrap();
        assert_eq!(
            receipt.transition.duplicate,
            matches!(phase, "after_commit" | "race")
        );
        assert_eq!(
            store
                .historical_snapshot(&start.run_id, old.revision)
                .unwrap(),
            old
        );
        store.verify(&start.run_id).unwrap();
        assert_eq!(
            store
                .execution_history(&start.run_id, 0, 100)
                .unwrap()
                .items
                .iter()
                .filter(|r| matches!(r.action, ExecutionAction::Migrated { .. }))
                .count(),
            1
        );
    }
}

fn downgrade_fixture(db: &Db) -> (StartRun, Snapshot) {
    let mut store = db.store();
    let r = start(&scenario("review-approved"));
    let state = store.start(&r).unwrap().snapshot;
    store
        .connection
        .execute_batch("DROP TABLE storage_migrations; PRAGMA user_version=10")
        .unwrap();
    (r, state)
}

#[test]
fn definition_migration_waits_for_live_attempts_and_refuses_recorded_business_effects() {
    let db = Db::new();
    let mut store = db.store();
    let start = start(&scenario("parallel-all"));
    let initial = store.start(&start).unwrap().snapshot;
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: start.run_id.clone(),
                owner: "operator".into(),
                acquisition_id: "migration-owner".into(),
                ttl_ms: 10000,
            },
            &Time(1000),
        )
        .unwrap();
    let Claimed::Task { attempt } = store.claim_next(&lease, &Time(1000)).unwrap() else {
        panic!("task");
    };
    store
        .apply(&event(
            &initial,
            "pause",
            EventKind::Pause {
                reason: "upgrade".into(),
            },
        ))
        .unwrap();
    let mut target = start.bundle.clone();
    target.root.version = "2.0.0".into();
    target.workflows[0].version = "2.0.0".into();
    let request = MigrationRequest {
        migration_id: "drain".into(),
        target_bundle: target,
        target_inputs: start.inputs.clone(),
        execution_policy: workflow_kernel::MigrationExecutionPolicy::RestartWithFreshEvidence,
        timer_policy: workflow_kernel::MigrationTimerPolicy::CancelAndRearmOnResume,
        node_mapping: vec![],
        decision_summary: "Drain existing execution".into(),
    };
    let plan = store.plan_migration(&start.run_id, &request).unwrap();
    assert_eq!(
        store
            .migrate_definition(&lease, &plan, "operator", &Time(1001))
            .unwrap_err()
            .code,
        ErrorCode::AttemptInProgress
    );
    let result = workflow_worker::WorkResult {
        protocol_version: 1,
        request_digest: digest(&attempt.request).unwrap(),
        completed_at_unix_ms: 1002,
        outcome: workflow_worker::AdapterOutcome::Succeeded {
            outputs: Values::new(),
            evidence: vec![],
        },
        model_record: None,
    };
    store
        .finish_task(&lease, &attempt.attempt_id, &result, &Time(1002))
        .unwrap();
    assert!(
        store
            .migrate_definition(&lease, &plan, "operator", &Time(1002))
            .is_err()
    );
    let fresh = store.plan_migration(&start.run_id, &request).unwrap();
    store
        .migrate_definition(&lease, &fresh, "operator", &Time(1002))
        .unwrap();
    store.verify(&start.run_id).unwrap();

    let db = Db::new();
    let mut store = db.store();
    let write: StartRun = workflow_worker::parse_message(
        &std::fs::read(base().join("examples/runs/effect-release.json")).unwrap(),
    )
    .unwrap();
    let old = store.start(&write).unwrap().snapshot;
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: write.run_id.clone(),
                owner: "operator".into(),
                acquisition_id: "write".into(),
                ttl_ms: 10000,
            },
            &Time(1000),
        )
        .unwrap();
    assert!(matches!(
        store.claim_effect(&lease, &Time(1000)).unwrap(),
        EffectClaim::Call { .. }
    ));
    store
        .apply(&event(
            &old,
            "pause",
            EventKind::Pause {
                reason: "external write may have happened".into(),
            },
        ))
        .unwrap();
    assert_eq!(
        store
            .plan_migration(&write.run_id, &request)
            .unwrap_err()
            .code,
        ErrorCode::MigrationBlocked
    );
}

#[test]
fn storage_upgrade_preflight_backup_restore_and_crash_rollback_preserve_locked_history() {
    for phase in [
        "schema_written",
        "verified",
        "before_commit",
        "after_commit",
    ] {
        let db = Db::new();
        let (r, old) = downgrade_fixture(&db);
        let plan = SqliteRunStore::plan_storage_upgrade(&db.path, None)
            .unwrap()
            .unwrap();
        assert_eq!(plan.source_version, 10);
        assert_eq!(plan.target_version, 11);
        let mut child = process(&db, phase, "upgrade", true);
        wait_file(&db.dir.join("ready-upgrade"), &mut child);
        child.kill().unwrap();
        child.wait().unwrap();
        let c = Connection::open(&db.path).unwrap();
        let version: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, if phase == "after_commit" { 11 } else { 10 });
        drop(c);
        let mut migrated = SqliteRunStore::migrate(&db.path).unwrap();
        assert_eq!(migrated.get(&r.run_id).unwrap(), old);
        assert_eq!(migrated.storage_history().unwrap(), [plan]);
    }
    let db = Db::new();
    let (r, old) = downgrade_fixture(&db);
    let plan = SqliteRunStore::plan_storage_upgrade(&db.path, None)
        .unwrap()
        .unwrap();
    let backup = db.dir.join("before.sqlite");
    let (mut upgraded, report) =
        SqliteRunStore::upgrade_with_backup(&db.path, &backup, None).unwrap();
    assert_eq!(report, plan);
    assert_eq!(upgraded.get(&r.run_id).unwrap(), old);
    assert_eq!(
        upgraded.storage_history().unwrap(),
        std::slice::from_ref(&report)
    );
    let restored = db.dir.join("restored.sqlite");
    SqliteRunStore::restore_storage_backup(&backup, &restored, &report, None).unwrap();
    assert!(SqliteRunStore::open(&restored).is_err()); // retained v10 binary is required
    assert_eq!(
        SqliteRunStore::migrate(&restored)
            .unwrap()
            .get(&r.run_id)
            .unwrap(),
        old
    );
    assert!(SqliteRunStore::restore_storage_backup(&backup, &restored, &report, None).is_err());
    let c = Connection::open(&backup).unwrap();
    c.execute_batch("UPDATE heads SET revision=revision+1")
        .unwrap();
    drop(c);
    assert!(SqliteRunStore::plan_storage_upgrade(&backup, None).is_err());
    assert!(SqliteRunStore::migrate(&backup).is_err());
    let c = Connection::open(&backup).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        10
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='storage_migrations'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
}
