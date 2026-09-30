use super::*;
use workflow_effects::{
    CallKind, EffectAttempt, EffectReceipt, EffectStatus, ManualOutcome, ManualResolution,
    Observation,
};

pub(super) fn effect_request() -> StartRun {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    serde_json::from_slice(&std::fs::read(root.join("examples/runs/effect-release.json")).unwrap())
        .unwrap()
}
fn rule(start: &StartRun) -> CapabilityRule {
    let cap = workflow_worker::Capability::new(start.bundle.capabilities[0].clone()).unwrap();
    CapabilityRule {
        model_policy: None,
        id: cap.descriptor().capability.id.clone(),
        version: cap.descriptor().capability.version.clone(),
        contract_digest: cap.digest().into(),
        artifacts: None,
        effect: Some(EffectRule {
            policy: start.bundle.effect_bindings[0].policy.clone(),
        }),
    }
}
pub(super) fn setup(start: &StartRun) -> (Fixture, IssuedCredential, IssuedCredential, Lease) {
    let mut f = Fixture::new();
    let maintainer = f.credential("maintainer", Role::DefinitionMaintainer);
    let runner = f.credential("runner", Role::Runner);
    let scheduler = f.credential("scheduler", Role::Scheduler);
    let worker = f
        .service
        .issue(
            f.admin.expose_secret(),
            "effect-worker",
            Role::Worker,
            &[rule(start)],
            MAX_TTL,
        )
        .unwrap();
    f.service
        .publish(maintainer.expose_secret(), &start.bundle)
        .unwrap();
    f.service.start(runner.expose_secret(), start).unwrap();
    let lease = f
        .service
        .acquire(scheduler.expose_secret(), &start.run_id, "effects", 60000)
        .unwrap();
    (f, scheduler, worker, lease)
}
pub(super) fn dispatch(
    f: &mut Fixture,
    scheduler: &IssuedCredential,
    worker: &IssuedCredential,
    lease: &Lease,
) -> String {
    let EffectDispatch::Call { assignment_id } = f
        .service
        .dispatch_effect(scheduler.expose_secret(), lease, &worker.id)
        .unwrap()
    else {
        panic!("call required")
    };
    assignment_id
}
// A synthetic provider receipt for authority tests; the HTTPS fault fixture uses
// actual independently committed provider state instead.
pub(super) fn receipt(a: &EffectAttempt) -> Observation {
    Observation::Applied {
        receipt: EffectReceipt {
            release: None,
            operation_key: a.intent.operation_key.clone(),
            intent_digest: workflow_effects::digest(&a.intent).unwrap(),
            target: a.intent.policy.target.clone(),
            resource_id: "test-release".into(),
            provider_receipt: "test-provider-receipt".into(),
            outputs: [("release_id".into(), "test-release".into())].into(),
        },
    }
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn effect_authority_policy_rollback_and_single_delivery_race() {
    let start = effect_request();
    let (mut f, scheduler, worker, lease) = setup(&start);
    for change in 0..5 {
        let mut r = rule(&start);
        match change {
            0 => r.effect = None,
            1 => r.effect.as_mut().unwrap().policy.target.id = "wrong-target".into(),
            2 => r.effect.as_mut().unwrap().policy.call_identity.id = "wrong-principal".into(),
            3 => r.effect.as_mut().unwrap().policy.retry.max_calls += 1,
            _ => r.contract_digest = workflow_worker::digest(&"wrong").unwrap(),
        }
        let wrong = f
            .service
            .issue(
                f.admin.expose_secret(),
                "wrong-worker",
                Role::Worker,
                &[r],
                MAX_TTL,
            )
            .unwrap();
        assert_eq!(
            f.service
                .dispatch_effect(scheduler.expose_secret(), &lease, &wrong.id)
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
        assert!(
            f.service
                .effects(scheduler.expose_secret(), &start.run_id, 0, 100)
                .unwrap()
                .items
                .is_empty()
        );
    }
    let mut foreign = Fixture::new();
    let foreign_worker = foreign
        .service
        .issue(
            foreign.admin.expose_secret(),
            "foreign-worker",
            Role::Worker,
            &[rule(&start)],
            MAX_TTL,
        )
        .unwrap();
    assert_eq!(
        f.service
            .dispatch_effect(scheduler.expose_secret(), &lease, &foreign_worker.id)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    let id = dispatch(&mut f, &scheduler, &worker, &lease);
    assert_eq!(
        f.service
            .effect_assignment(foreign_worker.expose_secret(), &id)
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        f.service
            .pending_effects(worker.expose_secret(), "", 100)
            .unwrap()
            .items,
        vec![id.clone()]
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut threads = vec![];
    for _ in 0..2 {
        let token = worker.expose_secret().to_owned();
        let id = id.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut service = AuthenticatedService::open(client()).unwrap();
            barrier.wait();
            service.effect_assignment(&token, &id)
        }));
    }
    let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes.iter().find_map(|r| r.as_ref().err()).unwrap().code,
        ErrorCode::ReceiptConflict
    );
    let a = outcomes.into_iter().find_map(|r| r.ok()).unwrap();
    assert!(
        f.service
            .pending_effects(worker.expose_secret(), "", 100)
            .unwrap()
            .items
            .is_empty()
    );
    let mut bad = receipt(&a);
    if let Observation::Applied { receipt } = &mut bad {
        receipt.operation_key = "wrong".into();
    }
    assert!(
        f.service
            .observe_assigned_effect(worker.expose_secret(), &id, &bad)
            .is_err()
    );
    f.service
        .observe_assigned_effect(worker.expose_secret(), &id, &receipt(&a))
        .unwrap();
    assert!(
        f.service
            .observe_assigned_effect(worker.expose_secret(), &id, &receipt(&a))
            .unwrap()
            .duplicate
    );
    assert!(
        f.service
            .outstanding_effects(f.admin.expose_secret(), "", 100)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(matches!(
        f.service
            .effects(scheduler.expose_secret(), &start.run_id, 0, 100)
            .unwrap()
            .items[0]
            .status,
        EffectStatus::Applied { .. }
    ));
    assert!(
        f.service
            .effects(worker.expose_secret(), &start.run_id, 0, 100)
            .is_err()
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn effect_revocation_takeover_requires_query_and_retains_operation_identity() {
    let start = effect_request();
    let (mut f, scheduler, worker, lease) = setup(&start);
    AuthenticatedService::configure_scheduling(
        &mut client(),
        &f.tenant,
        None,
        &scheduling::policy(),
    )
    .unwrap();
    f.service
        .worker_heartbeat(worker.expose_secret(), "1.0.0", false)
        .unwrap();
    let id = dispatch(&mut f, &scheduler, &worker, &lease);
    let a = f
        .service
        .effect_assignment(worker.expose_secret(), &id)
        .unwrap();
    f.service
        .revoke(f.admin.expose_secret(), &worker.id)
        .unwrap();
    assert_eq!(
        f.service
            .observe_assigned_effect(worker.expose_secret(), &id, &receipt(&a))
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    assert!(
        f.service
            .outstanding_effects(f.admin.expose_secret(), "", 100)
            .unwrap()
            .items[0]
            .worker_revoked
    );
    f.service
        .release(scheduler.expose_secret(), &lease)
        .unwrap();
    let next = f
        .service
        .acquire(scheduler.expose_secret(), &start.run_id, "next", 60000)
        .unwrap();
    let worker = f
        .service
        .issue(
            f.admin.expose_secret(),
            "replacement",
            Role::Worker,
            &[rule(&start)],
            MAX_TTL,
        )
        .unwrap();
    f.service
        .worker_heartbeat(worker.expose_secret(), "2.0.0", false)
        .unwrap();
    let id = dispatch(&mut f, &scheduler, &worker, &next);
    let b = f
        .service
        .effect_assignment(worker.expose_secret(), &id)
        .unwrap();
    assert_eq!(b.kind, CallKind::Query);
    assert_eq!(b.intent, a.intent);
    assert!(b.epoch > a.epoch);
    f.service
        .observe_assigned_effect(worker.expose_secret(), &id, &receipt(&a))
        .unwrap();
    f.service.release(scheduler.expose_secret(), &next).unwrap();
    assert_eq!(
        f.service
            .observe_assigned_effect(worker.expose_secret(), &id, &receipt(&a))
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn effect_unknown_without_lookup_requires_authenticated_manual_resolution() {
    let mut start = effect_request();
    start.bundle.capabilities[0].effects = workflow_worker::EffectContract::Write {
        idempotency: workflow_worker::Idempotency::None,
        query: None,
        compensation: None,
        irreversible: false,
    };
    let (mut f, scheduler, worker, lease) = setup(&start);
    let id = dispatch(&mut f, &scheduler, &worker, &lease);
    let a = f
        .service
        .effect_assignment(worker.expose_secret(), &id)
        .unwrap();
    let unknown = Observation::Unknown {
        reason: "SECRET-provider-body".into(),
    };
    f.service
        .observe_assigned_effect(worker.expose_secret(), &id, &unknown)
        .unwrap();
    assert!(matches!(
        f.service
            .dispatch_effect(scheduler.expose_secret(), &lease, &worker.id)
            .unwrap(),
        EffectDispatch::Manual { .. }
    ));
    let recovery = f.credential("human-reconciler", Role::Recovery);
    let resolution = ManualResolution {
        resolution_id: "checked-provider".into(),
        actor: "forged-actor".into(),
        reason: "provider inspected; all old writers stopped".into(),
        evidence: "sandbox:operator-check".into(),
        outcome: ManualOutcome::ConfirmedNotApplied,
    };
    assert!(
        f.service
            .resolve_effect(
                scheduler.expose_secret(),
                &lease,
                &a.intent.operation_key,
                &resolution
            )
            .is_err()
    );
    assert!(
        f.service
            .resolve_effect(
                recovery.expose_secret(),
                &lease,
                &a.intent.operation_key,
                &resolution
            )
            .is_err()
    );
    f.service
        .release(scheduler.expose_secret(), &lease)
        .unwrap();
    let lease = f
        .service
        .acquire(
            recovery.expose_secret(),
            &start.run_id,
            "reconciliation",
            60000,
        )
        .unwrap();
    f.service
        .resolve_effect(
            recovery.expose_secret(),
            &lease,
            &a.intent.operation_key,
            &resolution,
        )
        .unwrap();
    assert!(
        f.service
            .resolve_effect(
                recovery.expose_secret(),
                &lease,
                &a.intent.operation_key,
                &resolution
            )
            .unwrap()
            .duplicate
    );
    let rows=client().query("SELECT image FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2", &[&f.tenant,&start.run_id]).unwrap();
    let image = String::from_utf8(rows[0].get(0)).unwrap();
    assert!(image.contains("human-reconciler"));
    assert!(!image.contains("forged-actor"));
    assert!(!image.contains("SECRET-provider-body"));
    f.service.release(recovery.expose_secret(), &lease).unwrap();
    assert_eq!(
        f.service
            .resolve_effect(
                recovery.expose_secret(),
                &lease,
                &a.intent.operation_key,
                &resolution
            )
            .unwrap_err()
            .code,
        ErrorCode::LeaseConflict
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn effect_control_cancel_and_late_receipt_preserve_truth() {
    for delivered in [false, true] {
        let start = effect_request();
        let (mut f, scheduler, worker, lease) = setup(&start);
        let runner = f.credential("controller", Role::Runner);
        let id = dispatch(&mut f, &scheduler, &worker, &lease);
        let a = if delivered {
            Some(
                f.service
                    .effect_assignment(worker.expose_secret(), &id)
                    .unwrap(),
            )
        } else {
            None
        };
        let control = RunControlRequest {
            run_id: start.run_id.clone(),
            event_id: "cancel-request".into(),
            expected_revision: f
                .service
                .get(runner.expose_secret(), &start.run_id)
                .unwrap()
                .revision,
            control: RunControl::Cancel,
        };
        assert!(f.service.control(worker.expose_secret(), &control).is_err());
        f.service.control(runner.expose_secret(), &control).unwrap();
        assert!(
            f.service
                .control(runner.expose_secret(), &control)
                .unwrap()
                .transition
                .duplicate
        );
        let mut changed = control.clone();
        changed.control = RunControl::Pause {
            reason: "changed".into(),
        };
        assert!(f.service.control(runner.expose_secret(), &changed).is_err());
        if let Some(a) = a {
            // Cancellation cannot erase a write that the provider applied.
            f.service
                .observe_assigned_effect(worker.expose_secret(), &id, &receipt(&a))
                .unwrap();
            assert!(matches!(
                f.service
                    .effects(runner.expose_secret(), &start.run_id, 0, 100)
                    .unwrap()
                    .items[0]
                    .status,
                EffectStatus::Applied { .. }
            ));
        } else {
            assert_eq!(
                f.service
                    .effect_assignment(worker.expose_secret(), &id)
                    .unwrap_err()
                    .code,
                ErrorCode::TransitionRejected
            );
        }
    }
    let mut start = effect_request();
    start.bundle.capabilities[0].timeout_ms = 1000;
    let (mut f, scheduler, worker, lease) = setup(&start);
    let id = dispatch(&mut f, &scheduler, &worker, &lease);
    let a = f
        .service
        .effect_assignment(worker.expose_secret(), &id)
        .unwrap();
    while workflow_worker::SystemClock.now_unix_ms().unwrap() <= a.deadline_unix_ms {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The call deadline stops new I/O; a live lease can retain a late receipt.
    f.service
        .observe_assigned_effect(worker.expose_secret(), &id, &receipt(&a))
        .unwrap();
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn effect_schema_unknown_version_is_never_adopted() {
    let _f = Fixture::new();
    let mut db = client();
    let mut tx = db.transaction().unwrap();
    tx.execute(
        "UPDATE workflow_effect_dispatch.schema_version SET version=99",
        &[],
    )
    .unwrap();
    assert_eq!(
        super::super::effects::initialize(&mut tx).unwrap_err().code,
        ErrorCode::UnsupportedStorage
    );
    tx.rollback().unwrap();
    AuthenticatedService::initialize_effects(&mut db).unwrap();
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn effect_shared_retry_budget_and_permanent_errors_survive_reopen() {
    for transient in [false, true] {
        let mut start = effect_request();
        start.bundle.effect_bindings[0].policy.retry.max_calls = 2;
        let (mut f, scheduler, worker, lease) = setup(&start);
        let mut calls = 0;
        for _ in 0..50 {
            match f
                .service
                .dispatch_effect(scheduler.expose_secret(), &lease, &worker.id)
                .unwrap()
            {
                EffectDispatch::Call { assignment_id } => {
                    calls += 1;
                    f.service
                        .effect_assignment(worker.expose_secret(), &assignment_id)
                        .unwrap();
                    f.service
                        .observe_assigned_effect(
                            worker.expose_secret(),
                            &assignment_id,
                            &Observation::NotApplied {
                                code: if transient { "retry" } else { "forbidden" }.into(),
                                class: if transient {
                                    workflow_worker::FailureClass::Transient
                                } else {
                                    workflow_worker::FailureClass::PermissionDenied
                                },
                                message: "SECRET-exception".into(),
                            },
                        )
                        .unwrap();
                    f.service = AuthenticatedService::open(client()).unwrap();
                }
                EffectDispatch::Waiting { .. } => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                EffectDispatch::Idle | EffectDispatch::Handled => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(calls, if transient { 2 } else { 1 });
        let ledger = f
            .service
            .effects(scheduler.expose_secret(), &start.run_id, 0, 100)
            .unwrap();
        assert!(matches!(
            ledger.items[0].status,
            EffectStatus::Failed { .. }
        ));
        assert!(
            !serde_json::to_string(&ledger)
                .unwrap()
                .contains("SECRET-exception")
        );
    }
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn effect_shared_compensation_order_and_manual_takeover() {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    let start: StartRun = serde_json::from_slice(
        &std::fs::read(root.join("examples/runs/effect-compensation.json")).unwrap(),
    )
    .unwrap();
    let (mut f, scheduler, _, mut lease) = setup(&start);
    let policy = start.bundle.effect_bindings[0].policy.clone();
    let rules: Vec<_> = start
        .bundle
        .capabilities
        .iter()
        .filter(|c| c.effects != workflow_worker::EffectContract::ReadOnly)
        .map(|d| {
            let cap = workflow_worker::Capability::new(d.clone()).unwrap();
            CapabilityRule {
                model_policy: None,
                id: d.capability.id.clone(),
                version: d.capability.version.clone(),
                contract_digest: cap.digest().into(),
                artifacts: None,
                effect: Some(EffectRule {
                    policy: policy.clone(),
                }),
            }
        })
        .collect();
    let worker = f
        .service
        .issue(
            f.admin.expose_secret(),
            "compensator",
            Role::Worker,
            &rules,
            MAX_TTL,
        )
        .unwrap();
    let mut order = vec![];
    let mut failed = None;
    let began = std::time::Instant::now();
    while failed.is_none() {
        assert!(began.elapsed() < std::time::Duration::from_secs(30));
        f.service.tick(scheduler.expose_secret(), &lease).unwrap();
        match f
            .service
            .dispatch_effect(scheduler.expose_secret(), &lease, &worker.id)
            .unwrap()
        {
            EffectDispatch::Call { assignment_id } => {
                let a = f
                    .service
                    .effect_assignment(worker.expose_secret(), &assignment_id)
                    .unwrap();
                order.push(a.intent.node_id.clone());
                let observation = if a.intent.node_id == "undo_environment" {
                    failed = Some(a.clone());
                    Observation::NotApplied {
                        code: "forbidden".into(),
                        class: workflow_worker::FailureClass::PermissionDenied,
                        message: "manual provider cleanup required".into(),
                    }
                } else {
                    let outputs = a
                        .intent
                        .capability
                        .outputs
                        .keys()
                        .map(|k| (k.clone(), serde_json::json!(format!("resource-{k}"))))
                        .collect();
                    Observation::Applied {
                        receipt: EffectReceipt {
                            release: None,
                            operation_key: a.intent.operation_key.clone(),
                            intent_digest: workflow_effects::digest(&a.intent).unwrap(),
                            target: a.intent.policy.target.clone(),
                            resource_id: format!("resource-{}", a.intent.node_id),
                            provider_receipt: format!("receipt-{}", a.intent.node_id),
                            outputs,
                        },
                    }
                };
                f.service
                    .observe_assigned_effect(worker.expose_secret(), &assignment_id, &observation)
                    .unwrap();
                // Reopening between every effect replays durable receipts only.
                f.service = AuthenticatedService::open(client()).unwrap();
            }
            EffectDispatch::Idle => {
                f.service
                    .dispatch(scheduler.expose_secret(), &lease, &worker.id)
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            EffectDispatch::Handled => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        order,
        vec!["environment", "deploy", "undo_deploy", "undo_environment"]
    );
    let a = failed.unwrap();
    assert!(matches!(
        f.service
            .dispatch_effect(scheduler.expose_secret(), &lease, &worker.id)
            .unwrap(),
        EffectDispatch::Manual { .. }
    ));
    f.service
        .release(scheduler.expose_secret(), &lease)
        .unwrap();
    let recovery = f.credential("operator", Role::Recovery);
    let repair_lease = f
        .service
        .acquire(recovery.expose_secret(), &start.run_id, "repair", 60000)
        .unwrap();
    let resolution = ManualResolution {
        resolution_id: "manual-cleanup".into(),
        actor: "untrusted-actor".into(),
        reason: "sandbox cleanup performed".into(),
        evidence: "sandbox:cleanup-receipt".into(),
        outcome: ManualOutcome::Applied {
            receipt: EffectReceipt {
                release: None,
                operation_key: a.intent.operation_key.clone(),
                intent_digest: workflow_effects::digest(&a.intent).unwrap(),
                target: a.intent.policy.target.clone(),
                resource_id: "cleaned-environment".into(),
                provider_receipt: "sandbox-cleanup-1".into(),
                outputs: Default::default(),
            },
        },
    };
    f.service
        .resolve_effect(
            recovery.expose_secret(),
            &repair_lease,
            &a.intent.operation_key,
            &resolution,
        )
        .unwrap();
    f.service
        .release(recovery.expose_secret(), &repair_lease)
        .unwrap();
    lease = f
        .service
        .acquire(
            scheduler.expose_secret(),
            &start.run_id,
            "finish-compensation",
            60000,
        )
        .unwrap();
    for _ in 0..20 {
        if f.service
            .get(scheduler.expose_secret(), &start.run_id)
            .unwrap()
            .status
            == RunStatus::Cancelled
        {
            break;
        }
        assert!(!matches!(
            f.service
                .dispatch_effect(scheduler.expose_secret(), &lease, &worker.id)
                .unwrap(),
            EffectDispatch::Call { .. }
        ));
        f.service
            .dispatch(scheduler.expose_secret(), &lease, &worker.id)
            .unwrap();
    }
    assert_eq!(
        f.service
            .get(scheduler.expose_secret(), &start.run_id)
            .unwrap()
            .status,
        RunStatus::Cancelled
    );
    let ledger = f
        .service
        .effects(scheduler.expose_secret(), &start.run_id, 0, 100)
        .unwrap();
    assert_eq!(ledger.items.len(), 4);
    assert!(ledger.items.iter().all(|e| e.calls.len() == 1));
    assert_eq!(
        ledger
            .items
            .iter()
            .filter(|e| e.compensated_by.is_some())
            .count(),
        2
    );
}
