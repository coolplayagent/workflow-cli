use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use workflow_models::{
    Action, ModelAdapter, ModelCall, ModelEvent, ModelFailure, ModelIdentity, ModelReply, Policy,
    Proposal, Reply, ToolReply, Usage, execute,
};
use workflow_worker::{AdapterOutcome, Clock, WorkResult};
struct Time(u64);
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0)
    }
}
struct Planner {
    calls: AtomicUsize,
    unavailable: bool,
}
impl ModelAdapter for Planner {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            adapter: workflow_ir::VersionRef {
                id: "fixture.model".into(),
                version: "1.0.0".into(),
            },
            model: "deterministic-fixture".into(),
            binding_digest: digest(&"fixture").unwrap(),
        }
    }
    fn complete(&self, call: &ModelCall) -> Reply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.unavailable {
            return Reply::Failed {
                failure: ModelFailure::Unavailable,
            };
        }
        let action = match call.events.last() {
            Some(ModelEvent::ToolFinished {
                response: ToolReply::Received { result },
                ..
            }) => {
                let AdapterOutcome::Succeeded { outputs, .. } = &result.outcome else {
                    panic!("tool failure")
                };
                Action::Complete {
                    outputs: outputs.clone(),
                    summary: "Observed tool outputs".into(),
                }
            }
            _ => Action::Call {
                capability: call.policy.tools[0].capability.clone(),
                inputs: call.inputs.clone(),
                summary: "Validate document".into(),
            },
        };
        Reply::Received {
            reply: ModelReply {
                proposal: Proposal {
                    protocol_version: 1,
                    action,
                },
                resolved_model: "fixture-v1".into(),
                response_id: "fixture-response".into(),
                usage: Usage {
                    input_tokens: None,
                    output_tokens: None,
                },
            },
        }
    }
}
fn request() -> StartRun {
    workflow_worker::parse_message(
        &std::fs::read(base().join("examples/models/start.json")).unwrap(),
    )
    .unwrap()
}
fn prepare(store: &mut SqliteRunStore, start: &StartRun) -> (Lease, PreparedTask) {
    store.start(start).unwrap();
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: start.run_id.clone(),
                owner: "model-host".into(),
                acquisition_id: "acquisition".into(),
                ttl_ms: 10000,
            },
            &Time(1000),
        )
        .unwrap();
    let Claimed::Task { attempt } = store.claim_next(&lease, &Time(1000)).unwrap() else {
        panic!("task")
    };
    (lease, *attempt)
}
fn result(start: &StartRun, p: &PreparedTask, model: &Planner) -> WorkResult {
    let policy = Policy::new(start.bundle.model_policies[0].clone()).unwrap();
    let record = execute(
        &policy,
        &p.request,
        model,
        &workflow_builtin_capabilities::worker().unwrap(),
        &Time(1001),
        p.request.deadline_unix_ms,
    )
    .unwrap();
    WorkResult {
        protocol_version: 2,
        request_digest: digest(&p.request).unwrap(),
        completed_at_unix_ms: 1001,
        outcome: record.outcome.clone(),
        model_record: Some(serde_json::to_value(record).unwrap()),
    }
}
#[test]
fn model_completion_is_fenced_recorded_and_recovers_without_resampling() {
    for unavailable in [false, true] {
        let db = Db::new();
        let mut store = db.store();
        let start = request();
        let (lease, p) = prepare(&mut store, &start);
        assert_eq!(p.request.protocol_version, 2);
        assert_eq!(
            store.bundle(&start.run_id).unwrap().model_policies,
            start.bundle.model_policies
        );
        let model = Planner {
            calls: AtomicUsize::new(0),
            unavailable,
        };
        let result = result(&start, &p, &model);
        let before = store.get(&start.run_id).unwrap();
        let instance_id = before.frames[&1].nodes["inspect"].instance_id;
        assert!(
            store
                .apply(&event(
                    &before,
                    "raw-success",
                    EventKind::TaskCompleted {
                        instance_id,
                        result: TaskResult::Succeeded {
                            outputs: Values::new()
                        }
                    }
                ))
                .is_err()
        );
        for mode in 0..4 {
            let mut bad = result.clone();
            match mode {
                0 => bad.model_record = None,
                1 => {
                    bad.model_record.as_mut().unwrap()["policy_digest"] =
                        serde_json::json!(digest(&"foreign").unwrap())
                }
                2 => bad.completed_at_unix_ms = 1000,
                _ => {
                    bad.model_record.as_mut().unwrap()["events"][0]["call_digest"] =
                        serde_json::json!(digest(&"another").unwrap());
                }
            }
            assert!(
                store
                    .finish_task(&lease, &p.attempt_id, &bad, &Time(1001))
                    .is_err()
            );
            assert_eq!(store.get(&start.run_id).unwrap(), before);
        }
        let mut old = lease.clone();
        old.epoch += 1;
        assert_eq!(
            store
                .finish_task(&old, &p.attempt_id, &result, &Time(1001))
                .unwrap_err()
                .code,
            ErrorCode::LeaseConflict
        );
        let committed = store
            .finish_task(&lease, &p.attempt_id, &result, &Time(1001))
            .unwrap();
        assert_eq!(
            committed.snapshot.status,
            if unavailable {
                RunStatus::Failed
            } else {
                RunStatus::Succeeded
            }
        );
        drop(store);
        let mut recovered = SqliteRunStore::open(&db.path).unwrap();
        assert_eq!(recovered.get(&start.run_id).unwrap(), committed.snapshot);
        recovered.verify(&start.run_id).unwrap();
        assert!(
            recovered
                .finish_task(&lease, &p.attempt_id, &result, &Time(1002))
                .unwrap()
                .transition
                .duplicate
        );
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            if unavailable { 1 } else { 2 }
        );
    }
}
#[test]
fn policy_versions_are_locked_and_missing_tool_contracts_reject_start() {
    let db = Db::new();
    let mut store = db.store();
    let start = request();
    store.start(&start).unwrap();
    let mut changed = start.clone();
    changed.run_id = "second".into();
    changed.bundle.model_policies[0].goal = "Different instructions".into();
    assert_eq!(
        store.start(&changed).unwrap_err().code,
        ErrorCode::BindingConflict
    );
    let mut changed = start;
    changed.run_id = "third".into();
    changed.bundle.model_policies[0].tools[0].timeout_ms += 1;
    assert!(store.start(&changed).is_err());
}

#[test]
fn typed_model_success_cannot_skip_a_frozen_postcondition() {
    let db = Db::new();
    let mut store = db.store();
    let mut start = request();
    let gated: StartRun = workflow_worker::parse_message(
        &std::fs::read(base().join("examples/gates/guarded-start.json")).unwrap(),
    )
    .unwrap();
    let mut gate = gated.bundle.postconditions[0].clone();
    gate.workflow = start.bundle.root.clone();
    // The model's Boolean output is a proposal, never the required checker.
    // Execute a separate direct task; its missing report must still block the
    // model node even after both tasks return correctly typed success outputs.
    let mut checker = gated.bundle.workflows[0]
        .nodes
        .iter()
        .find(|n| n.id == "inspect")
        .unwrap()
        .clone();
    checker.id = "independent-check".into();
    let workflow = &mut start.bundle.workflows[0];
    workflow.entry = checker.id.clone();
    workflow.nodes.push(checker);
    workflow.edges.push(
        serde_json::from_value(serde_json::json!({
            "id":"checked", "from":"independent-check", "to":"inspect", "route":{"type":"next"}
        }))
        .unwrap(),
    );
    gate.policy.requirements[0].node_id = "independent-check".into();
    start.bundle.postconditions.push(gate);
    let (lease, checker) = prepare(&mut store, &start);
    let checked = workflow_builtin_capabilities::worker()
        .unwrap()
        .execute_with_clock(&checker.request, &checker.grant, &Time(1000))
        .unwrap()
        .into_result();
    store
        .finish_task(&lease, &checker.attempt_id, &checked, &Time(1000))
        .unwrap();
    let Claimed::Task { attempt: p } = store.claim_next(&lease, &Time(1000)).unwrap() else {
        panic!("model task")
    };
    let model = Planner {
        calls: AtomicUsize::new(0),
        unavailable: false,
    };
    let result = result(&start, &p, &model);
    let committed = store
        .finish_task(&lease, &p.attempt_id, &result, &Time(1001))
        .unwrap();
    assert_ne!(committed.snapshot.status, RunStatus::Succeeded);
    assert!(matches!(
        store.claim_next(&lease, &Time(1002)).unwrap(),
        Claimed::Handled { .. }
    ));
    assert!(matches!(
        store.claim_next(&lease, &Time(1003)).unwrap(),
        Claimed::Idle
    ));
    let waiting = store.get(&start.run_id).unwrap();
    assert_eq!(
        waiting.frames[&1].nodes["inspect"]
            .gate_decision
            .as_ref()
            .unwrap()
            .verdict,
        workflow_gates::Verdict::Unknown
    );
    drop(store);
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    assert_eq!(store.get(&start.run_id).unwrap(), waiting);
    store.verify(&start.run_id).unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
