use super::*;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use workflow_runstore::{RunStore, StartRun, Values};
use workflow_runstore_sqlite::SqliteRunStore;
use workflow_worker::{AdapterOutcome, CapabilityAdapter, CapabilityDescriptor, Invocation};
static NEXT: AtomicU64 = AtomicU64::new(0);
#[test]
fn paused_driver_performs_no_capability_calls_and_resume_reuses_pending_intent() {
    let path =
        std::env::temp_dir().join(format!("workflow-runtime-pause-{}.db", std::process::id()));
    let mut store = SqliteRunStore::create(&path).unwrap();
    let snapshot = store.start(&request(true)).unwrap().snapshot;
    store
        .apply(&workflow_kernel::Event {
            event_id: "pause".into(),
            run_id: snapshot.run_id.clone(),
            run_digest: snapshot.run_digest.clone(),
            expected_revision: 1,
            at_unix_ms: 1001,
            kind: workflow_kernel::EventKind::Pause {
                reason: "maintenance".into(),
            },
        })
        .unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let mut worker = Worker::default();
    worker
        .register(Counted {
            calls: calls.clone(),
        })
        .unwrap();
    let mut options = DriveOptions {
        owner: "local".into(),
        acquisition_id: "paused".into(),
        lease_ms: 1000,
        max_commands: 10,
    };
    let report = drive(&mut store, &worker, "inspect", &options, &Time(1001)).unwrap();
    assert_eq!(report.stop_reason, "paused");
    assert_eq!(report.executed_tasks, 0);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(report.snapshot.revision, 2);
    store
        .apply(&workflow_kernel::Event {
            event_id: "resume".into(),
            run_id: snapshot.run_id,
            run_digest: snapshot.run_digest,
            expected_revision: 2,
            at_unix_ms: 1002,
            kind: workflow_kernel::EventKind::Resume {
                reason: "ready".into(),
            },
        })
        .unwrap();
    options.acquisition_id = "resumed".into();
    let report = drive(&mut store, &worker, "inspect", &options, &Time(1002)).unwrap();
    assert_eq!(report.executed_tasks, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        report.snapshot.status,
        workflow_kernel::RunStatus::Succeeded
    );
    store.verify("inspect").unwrap();
    drop(store);
    std::fs::remove_file(path).unwrap();
}
struct Time(u64);
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0)
    }
}
struct Counted {
    calls: Arc<AtomicU64>,
}
impl CapabilityAdapter for Counted {
    fn descriptor(&self) -> CapabilityDescriptor {
        workflow_builtin_capabilities::ValidateDefinition.descriptor()
    }
    fn invoke(&self, i: Invocation<'_>) -> AdapterOutcome {
        self.calls.fetch_add(1, Ordering::Relaxed);
        workflow_builtin_capabilities::ValidateDefinition.invoke(i)
    }
}
fn base() -> PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn request(valid: bool) -> StartRun {
    let w = workflow_kernel::BundleSpec {
        postconditions: vec![],
        model_policies: vec![],
        effect_bindings: vec![],
        schema_version: 1,
        root: serde_json::from_value(
            serde_json::json!({"id":"inspect-definition","version":"1.0.0"}),
        )
        .unwrap(),
        workflows: vec![
            workflow_worker::parse_message(
                &std::fs::read(base().join("examples/worker/inspect-definition.json")).unwrap(),
            )
            .unwrap(),
        ],
        capabilities: vec![workflow_builtin_capabilities::ValidateDefinition.descriptor()],
    };
    StartRun {
        schema_version: 1,
        bundle: w,
        run_id: "inspect".into(),
        inputs: Values::from([
            ("format".into(), serde_json::json!("json")),
            (
                "document".into(),
                serde_json::json!(if valid {
                    std::fs::read_to_string(base().join("examples/review.json")).unwrap()
                } else {
                    "{}".into()
                }),
            ),
        ]),
        started_at_unix_ms: 1000,
        limits: Default::default(),
    }
}
#[test]
fn actual_builtin_results_drive_the_declared_business_branch_and_never_rerun_committed_work() {
    for valid in [false, true] {
        let path = std::env::temp_dir().join(format!(
            "workflow-runtime-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut store = SqliteRunStore::create(&path).unwrap();
        store.start(&request(valid)).unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let mut worker = Worker::default();
        worker
            .register(Counted {
                calls: calls.clone(),
            })
            .unwrap();
        let options = DriveOptions {
            owner: "local".into(),
            acquisition_id: "first".into(),
            lease_ms: 1000,
            max_commands: 10,
        };
        let report = drive(&mut store, &worker, "inspect", &options, &Time(1001)).unwrap();
        assert_eq!(report.executed_tasks, 1);
        assert_eq!(
            report.snapshot.status,
            if valid {
                workflow_kernel::RunStatus::Succeeded
            } else {
                workflow_kernel::RunStatus::Failed
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        drop(store);
        let mut store = SqliteRunStore::open(&path).unwrap();
        let mut options = options;
        options.acquisition_id = "second".into();
        let again = drive(&mut store, &worker, "inspect", &options, &Time(1002)).unwrap();
        assert_eq!(again.executed_tasks, 0);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        store.verify("inspect").unwrap();
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn storage_rejection_prevents_invocation_and_worker_errors_remain_retryable_observations() {
    let path =
        std::env::temp_dir().join(format!("workflow-runtime-errors-{}.db", std::process::id()));
    let mut store = SqliteRunStore::create(&path).unwrap();
    let before = store.start(&request(true)).unwrap().snapshot;
    drop(store);
    let calls = Arc::new(AtomicU64::new(0));
    let mut worker = Worker::default();
    worker
        .register(Counted {
            calls: calls.clone(),
        })
        .unwrap();
    let options = DriveOptions {
        owner: "local".into(),
        acquisition_id: "blocked".into(),
        lease_ms: 1000,
        max_commands: 10,
    };
    let mut readonly = SqliteRunStore::open_readonly(&path).unwrap();
    let error = drive(&mut readonly, &worker, "inspect", &options, &Time(1001)).unwrap_err();
    assert!(error.storage.is_some());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    drop(readonly);
    let mut store = SqliteRunStore::open(&path).unwrap();
    let error = drive(
        &mut store,
        &Worker::default(),
        "inspect",
        &options,
        &Time(1001),
    )
    .unwrap_err();
    assert_eq!(
        error.worker.unwrap().code,
        workflow_worker::ErrorCode::MissingCapability
    );
    assert!(error.release_error.is_none());
    assert_eq!(store.get("inspect").unwrap(), before);
    assert!(store.history("inspect", 0, 100).unwrap().items.is_empty());
    let records = store.execution_history("inspect", 0, 100).unwrap().items;
    assert!(
        records
            .iter()
            .any(|r| matches!(r.action, workflow_runstore::ExecutionAction::Failed { .. }))
    );
    let options = DriveOptions {
        acquisition_id: "retry".into(),
        ..options
    };
    let report = drive(&mut store, &worker, "inspect", &options, &Time(1002)).unwrap();
    assert_eq!(report.executed_tasks, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        report.snapshot.status,
        workflow_kernel::RunStatus::Succeeded
    );
    store.verify("inspect").unwrap();
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn effect_driver_records_unknown_then_queries_without_repeating_the_write() {
    use workflow_effects::{CallKind, EffectAdapter, EffectAttempt, EffectReceipt, Observation};
    struct Provider {
        writes: AtomicU64,
        queries: AtomicU64,
    }
    impl EffectAdapter for Provider {
        fn execute(
            &self,
            p: &EffectAttempt,
            _: &dyn Clock,
        ) -> workflow_worker::Result<Observation> {
            match p.kind {
                CallKind::Write => {
                    self.writes.fetch_add(1, Ordering::Relaxed);
                    Err(workflow_worker::Error::new(
                        workflow_worker::ErrorCode::DeadlineExceeded,
                        "response lost after provider commit",
                    ))
                }
                CallKind::Query => {
                    self.queries.fetch_add(1, Ordering::Relaxed);
                    Ok(Observation::Applied {
                        receipt: EffectReceipt {
                            operation_key: p.intent.operation_key.clone(),
                            intent_digest: workflow_effects::digest(&p.intent)?,
                            target: p.intent.policy.target.clone(),
                            resource_id: "release-1".into(),
                            provider_receipt: "provider-query-receipt".into(),
                            outputs: [("release_id".into(), "release-1".into())].into(),
                        },
                    })
                }
            }
        }
    }
    let path = std::env::temp_dir().join(format!(
        "workflow-runtime-effects-{}.db",
        std::process::id()
    ));
    let mut store = SqliteRunStore::create(&path).unwrap();
    let req: StartRun = workflow_worker::parse_message(
        &std::fs::read(base().join("examples/runs/effect-release.json")).unwrap(),
    )
    .unwrap();
    store.start(&req).unwrap();
    let provider = Provider {
        writes: AtomicU64::new(0),
        queries: AtomicU64::new(0),
    };
    let mut options = DriveOptions {
        owner: "runtime".into(),
        acquisition_id: "first".into(),
        lease_ms: 1000,
        max_commands: 10,
    };
    let first = drive_with_effects(
        &mut store,
        &Worker::default(),
        &provider,
        &req.run_id,
        &options,
        &Time(1000),
    )
    .unwrap();
    assert_eq!(first.effect_calls, 1);
    assert_eq!(first.stop_reason, "effect_backoff");
    drop(store);
    let mut store = SqliteRunStore::open(&path).unwrap();
    options.acquisition_id = "second".into();
    let recovered = drive_with_effects(
        &mut store,
        &Worker::default(),
        &provider,
        &req.run_id,
        &options,
        &Time(1010),
    )
    .unwrap();
    assert_eq!(
        recovered.snapshot.status,
        workflow_kernel::RunStatus::Succeeded
    );
    assert_eq!(recovered.effect_calls, 1);
    options.acquisition_id = "third".into();
    assert_eq!(
        drive_with_effects(
            &mut store,
            &Worker::default(),
            &provider,
            &req.run_id,
            &options,
            &Time(1020)
        )
        .unwrap()
        .effect_calls,
        0
    );
    assert_eq!(provider.writes.load(Ordering::Relaxed), 1);
    assert_eq!(provider.queries.load(Ordering::Relaxed), 1);
    store.verify(&req.run_id).unwrap();
    drop(store);
    std::fs::remove_file(path).unwrap();
}
