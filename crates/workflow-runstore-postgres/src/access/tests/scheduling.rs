use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Barrier};
use std::time::Instant;

pub(super) fn policy() -> SchedulingPolicy {
    let limit = AdmissionLimit {
        concurrent: 16,
        per_minute: 10000,
    };
    SchedulingPolicy {
        schema_version: 1,
        tenant: limit.clone(),
        project: limit.clone(),
        capability: limit.clone(),
        model: limit.clone(),
        worker: limit,
        project_limits: BTreeMap::new(),
        capability_limits: BTreeMap::new(),
        model_limits: BTreeMap::new(),
        model_pools: BTreeMap::new(),
        allowed_worker_versions: BTreeSet::from(["1.0.0".into(), "2.0.0".into()]),
        heartbeat_ms: 300000,
        priority_step_ms: 10,
        max_active_runs_per_project: 100,
    }
}
fn simple(id: &str) -> StartRun {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = if let Ok(dir) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(dir).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        root
    };
    let mut r: StartRun = serde_json::from_slice(
        &std::fs::read(root.join("examples/execution/valid-start.json")).unwrap(),
    )
    .unwrap();
    r.run_id = id.into();
    r
}
struct Cluster {
    f: Fixture,
    runner: IssuedCredential,
    scheduler: IssuedCredential,
    recovery: IssuedCredential,
    workers: Vec<IssuedCredential>,
}
impl Cluster {
    fn new(p: &SchedulingPolicy) -> Self {
        let mut f = Fixture::new();
        AuthenticatedService::configure_scheduling(&mut client(), &f.tenant, None, p).unwrap();
        let author = f.credential("cluster-author", Role::DefinitionMaintainer);
        f.service
            .publish(author.expose_secret(), &simple("x").bundle)
            .unwrap();
        let runner = f.credential("runner", Role::Runner);
        let scheduler = f.credential("scheduler", Role::Scheduler);
        let recovery = f.credential("recovery", Role::Recovery);
        let workers = (0..3)
            .map(|i| f.credential(&format!("worker-{i}"), Role::Worker))
            .collect::<Vec<_>>();
        for w in &workers {
            f.service
                .worker_heartbeat(w.expose_secret(), "1.0.0", false)
                .unwrap();
        }
        Self {
            f,
            runner,
            scheduler,
            recovery,
            workers,
        }
    }
    fn start(&mut self, id: &str) -> Lease {
        self.f
            .service
            .start(self.runner.expose_secret(), &simple(id))
            .unwrap();
        self.f
            .service
            .acquire(self.scheduler.expose_secret(), id, "owner", 300000)
            .unwrap()
    }
    fn dispatch(&mut self, l: &Lease, worker: usize) -> RoutedDispatch {
        self.f
            .service
            .dispatch_routed(
                self.scheduler.expose_secret(),
                l,
                &[self.workers[worker].id.clone()],
                false,
            )
            .unwrap()
    }
    fn finish(&mut self, id: &str, worker: usize) {
        let task = self
            .f
            .service
            .assignment(self.workers[worker].expose_secret(), id)
            .unwrap();
        let result = execute(&task);
        self.f
            .service
            .finish(self.workers[worker].expose_secret(), id, &result)
            .unwrap();
        assert!(
            self.f
                .service
                .finish(self.workers[worker].expose_secret(), id, &result)
                .unwrap()
                .duplicate
        );
    }
}
#[test]
fn scheduling_policy_rejects_unbounded_ambiguous_or_unpinned_limits() {
    let p = policy();
    p.validate().unwrap();
    let mut bad = p.clone();
    bad.tenant.concurrent = 0;
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.allowed_worker_versions = BTreeSet::from(["latest".into()]);
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.capability_limits
        .insert("task".into(), p.tenant.clone());
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.heartbeat_ms = u64::MAX;
    assert!(bad.validate().is_err());
    let mut bad = p;
    bad.priority_step_ms = u64::MAX;
    assert!(bad.validate().is_err());
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn concurrent_schedulers_reserve_one_slot_atomically_and_completion_does_not_refund_rate() {
    let mut p = policy();
    p.tenant.concurrent = 1;
    let mut c = Cluster::new(&p);
    let leases = [c.start("one"), c.start("two")];
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = vec![];
    for (i, lease) in leases.iter().cloned().enumerate() {
        let barrier = barrier.clone();
        let token = c.scheduler.expose_secret().to_owned();
        let worker = c.workers[i].id.clone();
        threads.push(std::thread::spawn(move || {
            let mut service = AuthenticatedService::open(client()).unwrap();
            barrier.wait();
            service
                .dispatch_routed(&token, &lease, &[worker], false)
                .unwrap()
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, RoutedDispatch::Task { .. }))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, RoutedDispatch::Deferred))
            .count(),
        1
    );
    let winner = results
        .iter()
        .position(|r| matches!(r, RoutedDispatch::Task { .. }))
        .unwrap();
    let RoutedDispatch::Task { assignment_id } = &results[winner] else {
        unreachable!()
    };
    c.finish(assignment_id, winner);
    let loser = 1 - winner;
    let RoutedDispatch::Task { assignment_id } = c.dispatch(&leases[loser], loser) else {
        panic!("released slot was not available")
    };
    c.finish(&assignment_id, loser);
    let rows: i64 = client()
        .query_one(
            "SELECT count(*) FROM workflow_scheduling.admissions WHERE tenant=$1",
            &[&c.f.tenant],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        rows, 2,
        "deferred transaction must not leave an admission or speculative attempt"
    );
    p.tenant.per_minute = 2;
    assert_eq!(
        AuthenticatedService::configure_scheduling(&mut client(), &c.f.tenant, Some(1), &p)
            .unwrap(),
        2
    );
    assert_eq!(
        AuthenticatedService::configure_scheduling(&mut client(), &c.f.tenant, Some(1), &p)
            .unwrap_err()
            .code,
        ErrorCode::BindingConflict
    );
    let l = c.start("rate-limited");
    assert!(matches!(c.dispatch(&l, 0), RoutedDispatch::Deferred));
    // Another tenant has its own admission row and is unaffected by this quota.
    let mut other = Cluster::new(&p);
    let l = other.start("independent-tenant");
    assert!(matches!(other.dispatch(&l, 0), RoutedDispatch::Task { .. }));
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn exact_routing_drain_renewal_and_scoped_dead_letter_replay_preserve_execution_authority() {
    let mut c = Cluster::new(&policy());
    let l = c.start("rolling");
    let mut wrong_rule = rules()[0].clone();
    wrong_rule.version = "2.0.0".into();
    let wrong =
        c.f.service
            .issue(
                c.f.admin.expose_secret(),
                "wrong",
                Role::Worker,
                &[wrong_rule],
                MAX_TTL,
            )
            .unwrap();
    c.f.service
        .worker_heartbeat(wrong.expose_secret(), "1.0.0", false)
        .unwrap();
    let result =
        c.f.service
            .dispatch_routed(
                c.scheduler.expose_secret(),
                &l,
                &[wrong.id.clone(), c.workers[0].id.clone()],
                false,
            )
            .unwrap();
    let RoutedDispatch::Task { assignment_id } = result else {
        panic!("exact compatible worker not selected")
    };
    let task =
        c.f.service
            .assignment(c.workers[0].expose_secret(), &assignment_id)
            .unwrap();
    let renewed =
        c.f.service
            .renew(c.scheduler.expose_secret(), &l, 300000)
            .unwrap();
    assert_eq!(renewed.epoch, l.epoch);
    assert!(renewed.expires_at_unix_ms > l.expires_at_unix_ms);
    assert_eq!(
        task,
        c.f.service
            .assignment(c.workers[0].expose_secret(), &assignment_id)
            .unwrap(),
        "renewal must not alter the task grant/deadline"
    );
    assert_eq!(
        c.f.service
            .tick(c.scheduler.expose_secret(), &l)
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
    let drained =
        c.f.service
            .worker_heartbeat(c.workers[0].expose_secret(), "1.0.0", true)
            .unwrap();
    assert!(drained.draining);
    assert_eq!(drained.active_assignments, 1);
    assert!(
        c.f.service
            .worker_heartbeat(c.workers[0].expose_secret(), "1.0.0", false)
            .unwrap()
            .draining
    );
    c.finish(&assignment_id, 0);
    let next = c.start("next-version");
    assert!(matches!(c.dispatch(&next, 0), RoutedDispatch::Deferred));
    c.f.service
        .worker_heartbeat(c.workers[1].expose_secret(), "2.0.0", false)
        .unwrap();
    let RoutedDispatch::Task { assignment_id } = c.dispatch(&next, 1) else {
        panic!("new version not selected")
    };
    c.finish(&assignment_id, 1);
    assert_eq!(
        c.f.service
            .worker_status(c.scheduler.expose_secret(), &c.workers[0].id)
            .unwrap()
            .active_assignments,
        0
    );
    let l = c.start("dead-letter");
    let RoutedDispatch::Parked { letter } =
        c.f.service
            .dispatch_routed(c.scheduler.expose_secret(), &l, &[wrong.id], false)
            .unwrap()
    else {
        panic!("missing route not diagnosed")
    };
    assert_eq!(letter.reason, "no_compatible_worker");
    assert!(matches!(c.dispatch(&l, 1), RoutedDispatch::Parked { .. }));
    let mut resolution = DeadLetterResolution {
        id: letter.id,
        expected_revision: letter.revision,
        expected_snapshot_digest: letter.snapshot_digest,
        retry: true,
        reason: "installed reviewed compatible worker".into(),
    };
    assert_eq!(
        c.f.service
            .resolve_dead_letter(c.runner.expose_secret(), &resolution)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    resolution.expected_revision += 1;
    assert!(
        c.f.service
            .resolve_dead_letter(c.recovery.expose_secret(), &resolution)
            .is_err()
    );
    resolution.expected_revision -= 1;
    c.f.service
        .resolve_dead_letter(c.recovery.expose_secret(), &resolution)
        .unwrap();
    let receipt =
        c.f.service
            .dead_letter(c.recovery.expose_secret(), &resolution.id)
            .unwrap()
            .resolution
            .unwrap();
    assert_eq!(receipt.actor, "recovery");
    assert_eq!(receipt.review.reason, resolution.reason);
    c.f.service
        .resolve_dead_letter(c.recovery.expose_secret(), &resolution)
        .unwrap();
    let RoutedDispatch::Task { assignment_id } = c.dispatch(&l, 1) else {
        panic!("reviewed replay did not resume current task")
    };
    c.finish(&assignment_id, 1);
    let events =
        c.f.service
            .audit(c.f.admin.expose_secret(), 0, 100)
            .unwrap();
    assert!(
        events
            .items
            .iter()
            .any(|a| a.operation == "dead_letter_replayed")
    );
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn priority_rotation_prevents_low_priority_starvation_and_queue_limit_is_transactional() {
    let mut p = policy();
    p.max_active_runs_per_project = 3;
    p.priority_step_ms = 1;
    let mut c = Cluster::new(&p);
    for id in ["high-a", "high-b", "low"] {
        c.start(id);
    }
    c.f.service
        .set_priority(c.runner.expose_secret(), "high-a", 9)
        .unwrap();
    c.f.service
        .set_priority(c.runner.expose_secret(), "high-b", 9)
        .unwrap();
    assert_eq!(
        c.f.service
            .start(c.runner.expose_secret(), &simple("overflow"))
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    assert_eq!(
        c.f.service
            .get(c.runner.expose_secret(), "overflow")
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let started = Instant::now();
    let mut seen = BTreeSet::new();
    for _ in 0..6 {
        for run in
            c.f.service
                .schedule_candidates(c.scheduler.expose_secret(), 1)
                .unwrap()
        {
            seen.insert(run.run_id);
        }
        if seen.len() == 3 {
            break;
        }
    }
    assert_eq!(seen.len(), 3);
    assert!(seen.contains("low"));
    eprintln!(
        "cluster_priority_rotation: runs=3 page=1 low_priority_selected=true elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn project_capability_and_worker_limits_cannot_be_bypassed_by_legacy_dispatch() {
    for scope in ["project", "capability", "worker"] {
        let mut p = policy();
        let limit = AdmissionLimit {
            concurrent: 1,
            per_minute: 10000,
        };
        match scope {
            "project" => {
                p.project_limits.insert("project".into(), limit);
            }
            "capability" => {
                p.capability_limits
                    .insert("workflow.validate@1.0.0".into(), limit);
            }
            _ => p.worker = limit,
        }
        let mut c = Cluster::new(&p);
        let first = c.start("first");
        let second = c.start("second");
        let RoutedDispatch::Task { assignment_id } = c.dispatch(&first, 0) else {
            panic!("first task");
        };
        let worker = if scope == "worker" { 0 } else { 1 };
        assert_eq!(
            c.f.service
                .dispatch(c.scheduler.expose_secret(), &second, &c.workers[worker].id)
                .unwrap_err()
                .code,
            ErrorCode::Busy,
            "{scope}"
        );
        c.finish(&assignment_id, 0);
        assert!(
            matches!(c.dispatch(&second, worker), RoutedDispatch::Task { .. }),
            "{scope}"
        );
    }
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn expired_heartbeat_readonly_database_and_reviewed_execution_failure_fail_closed() {
    let mut c = Cluster::new(&policy());
    let lease = c.start("heartbeat");
    // Move only this fixture heartbeat outside its window; run/task clocks and
    // lease epochs remain real database time.
    client().execute("UPDATE workflow_scheduling.workers SET heartbeat_at=heartbeat_at-300001 WHERE worker_id=$1", &[&c.workers[0].id]).unwrap();
    assert!(matches!(c.dispatch(&lease, 0), RoutedDispatch::Deferred));
    c.f.service
        .worker_heartbeat(c.workers[0].expose_secret(), "1.0.0", false)
        .unwrap();
    let mut read_only = client();
    read_only
        .batch_execute("SET default_transaction_read_only=on")
        .unwrap();
    let mut unavailable = AuthenticatedService::open(read_only).unwrap();
    assert!(
        unavailable
            .dispatch_routed(
                c.scheduler.expose_secret(),
                &lease,
                &[c.workers[0].id.clone()],
                false
            )
            .is_err()
    );
    let count: i64 = client()
        .query_one(
            "SELECT count(*) FROM workflow_scheduling.admissions WHERE tenant=$1",
            &[&c.f.tenant],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        count, 0,
        "readonly failure cannot leave a grant or acknowledge dispatch"
    );
    let RoutedDispatch::Task { assignment_id } = c.dispatch(&lease, 0) else {
        panic!("database recovery");
    };
    c.f.service
        .fail(
            c.workers[0].expose_secret(),
            &assignment_id,
            &workflow_worker::Error::new(
                workflow_worker::ErrorCode::InvalidRequest,
                "private adapter detail",
            ),
        )
        .unwrap();
    let letters =
        c.f.service
            .dead_letters(c.recovery.expose_secret(), "", 10)
            .unwrap();
    assert_eq!(letters.items.len(), 1);
    let letter = &letters.items[0];
    assert_eq!(letter.reason, "worker_execution_failed");
    let mut resolution = DeadLetterResolution {
        id: letter.id.clone(),
        expected_revision: letter.revision,
        expected_snapshot_digest: letter.snapshot_digest.clone(),
        retry: false,
        reason: "reviewed adapter failure and repaired worker".into(),
    };
    assert!(
        c.f.service
            .resolve_dead_letter(c.recovery.expose_secret(), &resolution)
            .is_err(),
        "archiving an active run must not silently resume it"
    );
    assert!(matches!(
        c.dispatch(&lease, 0),
        RoutedDispatch::Parked { .. }
    ));
    resolution.retry = true;
    c.f.service
        .resolve_dead_letter(c.recovery.expose_secret(), &resolution)
        .unwrap();
    assert!(
        c.f.service
            .dead_letters(c.recovery.expose_secret(), "", 10)
            .unwrap()
            .items
            .is_empty()
    );
    let RoutedDispatch::Task { assignment_id } = c.dispatch(&lease, 0) else {
        panic!("reviewed retry");
    };
    c.finish(&assignment_id, 0);
    // Advance the remaining decision/terminal commands after the recovered task.
    for _ in 0..4 {
        let _ = c.dispatch(&lease, 0);
    }
    assert_eq!(
        c.f.service
            .get(c.runner.expose_secret(), "heartbeat")
            .unwrap()
            .status,
        RunStatus::Succeeded
    );
    assert_eq!(
        AuthenticatedService::access_schema_version(&mut client()).unwrap(),
        3
    );
    AuthenticatedService::migrate_access(&mut client()).unwrap();
    assert_eq!(
        AuthenticatedService::access_schema_version(&mut client()).unwrap(),
        3
    );
}
