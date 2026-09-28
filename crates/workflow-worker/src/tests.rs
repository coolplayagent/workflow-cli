use super::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use workflow_ir::*;

fn descriptor() -> CapabilityDescriptor {
    let contract = BTreeMap::from([(
        "value".into(),
        Field {
            value_type: ValueType::Integer,
            required: true,
        },
    )]);
    CapabilityDescriptor {
        schema_version: 1,
        capability: VersionRef {
            id: "test.echo".into(),
            version: "1.0.0".into(),
        },
        inputs: contract.clone(),
        outputs: contract,
        timeout_ms: 1000,
        error_codes: BTreeMap::from([("unavailable".into(), FailureClass::Transient)]),
        effects: EffectContract::ReadOnly,
        usage: "Echo an integer without external effects".into(),
        skill: None,
    }
}
struct Echo {
    calls: Arc<AtomicUsize>,
    descriptor: CapabilityDescriptor,
    outcome: Option<AdapterOutcome>,
    panic: bool,
}
impl CapabilityAdapter for Echo {
    fn descriptor(&self) -> CapabilityDescriptor {
        self.descriptor.clone()
    }
    fn invoke(&self, invocation: Invocation<'_>) -> AdapterOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "simulated adapter panic");
        self.outcome
            .clone()
            .unwrap_or_else(|| AdapterOutcome::Succeeded {
                outputs: invocation.request.inputs.clone(),
                evidence: vec![],
            })
    }
}
fn worker(outcome: Option<AdapterOutcome>) -> (Worker, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut worker = Worker::default();
    worker
        .register(Echo {
            calls: calls.clone(),
            descriptor: descriptor(),
            outcome,
            panic: false,
        })
        .unwrap();
    (worker, calls)
}
fn request() -> WorkRequest {
    WorkRequest::standalone(
        &Capability::new(descriptor()).unwrap(),
        Values::from([("value".into(), json!(7))]),
        RequestContext {
            request_id: "r1".into(),
            trace_id: "t1".into(),
            issued_at_unix_ms: 90,
            deadline_unix_ms: 2000,
        },
    )
    .unwrap()
}
struct Times(Mutex<Vec<u64>>);
impl Times {
    fn new(times: Vec<u64>) -> Self {
        Self(Mutex::new(times.into_iter().rev().collect()))
    }
}
impl Clock for Times {
    fn now_unix_ms(&self) -> Result<u64> {
        Ok(self.0.lock().unwrap().pop().unwrap_or(101))
    }
}
fn result(worker: &Worker, request: &WorkRequest) -> Result<AcceptedResult> {
    worker.execute_with_clock(
        request,
        &ExecutionGrant::bind(request)?,
        &Times::new(vec![100, 100, 101, 101]),
    )
}

#[test]
fn descriptor_identity_is_immutable_and_contract_changes_change_digest() {
    let first = Capability::new(descriptor()).unwrap();
    let bytes = to_message(first.descriptor()).unwrap();
    let restored = Capability::new(parse_message(&bytes).unwrap()).unwrap();
    assert_eq!(first.digest(), restored.digest());
    let mut next = descriptor();
    next.timeout_ms += 1;
    assert_ne!(first.digest(), Capability::new(next).unwrap().digest());
    for bad in ["latest", "*", "main", "1.*", ""] {
        let mut d = descriptor();
        d.capability.version = bad.into();
        assert!(Capability::new(d).is_err(), "{bad}");
    }
    let mut d = descriptor();
    d.timeout_ms = 0;
    assert_eq!(
        Capability::new(d).unwrap_err().code,
        ErrorCode::InvalidDescriptor
    );
    let (mut w, _) = worker(None);
    assert_eq!(
        w.register(Echo {
            calls: Arc::default(),
            descriptor: descriptor(),
            outcome: None,
            panic: false
        })
        .unwrap_err()
        .code,
        ErrorCode::DuplicateCapability
    );
}

#[test]
fn in_process_and_json_calls_produce_the_same_checked_observation() {
    let (w, calls) = worker(None);
    let r = request();
    let local = result(&w, &r).unwrap().into_result();
    let restored: WorkRequest = parse_message(&to_message(&r).unwrap()).unwrap();
    let wire = result(&w, &restored).unwrap().into_result();
    assert_eq!(local, wire);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let output: WorkResult = parse_message(&to_message(&wire).unwrap()).unwrap();
    assert_eq!(
        accept_result(
            &r,
            &ExecutionGrant::bind(&r).unwrap(),
            &Capability::new(descriptor()).unwrap(),
            output,
            101
        )
        .unwrap()
        .result(),
        &local
    );
}

#[test]
fn protocol_input_and_authority_errors_are_rejected_before_adapter_calls() {
    let (w, calls) = worker(None);
    let original = request();
    let grant = ExecutionGrant::bind(&original).unwrap();
    for version in [0, 2] {
        let mut r = original.clone();
        r.protocol_version = version;
        assert_eq!(
            w.execute_with_clock(&r, &grant, &Times::new(vec![100]))
                .unwrap_err()
                .code,
            ErrorCode::UnsupportedProtocol
        );
    }
    let mut r = original.clone();
    r.inputs.insert("value".into(), json!(8));
    assert_eq!(
        w.execute_with_clock(&r, &grant, &Times::new(vec![100]))
            .unwrap_err()
            .code,
        ErrorCode::DigestMismatch
    );
    r.input_digest = digest(&r.inputs).unwrap();
    assert_eq!(
        w.execute_with_clock(&r, &grant, &Times::new(vec![100]))
            .unwrap_err()
            .code,
        ErrorCode::Unauthorized
    );
    for value in [json!("7"), json!(null)] {
        let mut r = original.clone();
        r.inputs.insert("value".into(), value);
        r.input_digest = digest(&r.inputs).unwrap();
        assert_eq!(result(&w, &r).unwrap_err().code, ErrorCode::InvalidInput);
    }
    let mut r = original.clone();
    r.inputs.insert("unknown".into(), json!(1));
    r.input_digest = digest(&r.inputs).unwrap();
    assert_eq!(result(&w, &r).unwrap_err().code, ErrorCode::InvalidInput);
    let mut r = original.clone();
    r.contract_digest = digest(&"different contract").unwrap();
    assert_eq!(result(&w, &r).unwrap_err().code, ErrorCode::DigestMismatch);
    let mut r = original.clone();
    r.capability.version = "2".into();
    assert_eq!(
        result(&w, &r).unwrap_err().code,
        ErrorCode::MissingCapability
    );
    for now in [89, 2000, 2001] {
        assert_eq!(
            w.execute_with_clock(&original, &grant, &Times::new(vec![now]))
                .unwrap_err()
                .code,
            ErrorCode::Expired
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn strict_wire_decoder_rejects_duplicate_keys_unknown_fields_trailing_and_deep_data() {
    assert!(parse_message::<Values>(br#"{"value":1,"value":2}"#).is_err());
    assert!(parse_message::<Values>(br#"{"nested":{"x":1,"x":2}}"#).is_err());
    let mut r = serde_json::to_value(request()).unwrap();
    r["grant"] = json!({"request_digest":"self-authorized"});
    assert!(parse_message::<WorkRequest>(&serde_json::to_vec(&r).unwrap()).is_err());
    assert!(parse_message::<Values>(br#"{} {}"#).is_err());
    let deep = format!("{}0{}", "[".repeat(130), "]".repeat(130));
    assert!(parse_message::<serde_json::Value>(deep.as_bytes()).is_err());
    assert!(parse_message::<Values>(&vec![b' '; MAX_MESSAGE_BYTES + 1]).is_err());
    assert_eq!(
        digest(&json!({"b":2,"a":1})).unwrap(),
        digest(&json!({"a":1,"b":2})).unwrap()
    );
}

#[test]
fn invalid_outputs_failure_codes_and_evidence_never_become_accepted_results() {
    for outcome in [
        AdapterOutcome::Succeeded {
            outputs: Values::new(),
            evidence: vec![],
        },
        AdapterOutcome::Succeeded {
            outputs: Values::from([("value".into(), json!("wrong"))]),
            evidence: vec![],
        },
        AdapterOutcome::Failed {
            code: "unknown".into(),
            class: FailureClass::Transient,
            message: "failed".into(),
            evidence: vec![],
        },
        AdapterOutcome::Failed {
            code: "unavailable".into(),
            class: FailureClass::Permanent,
            message: "failed".into(),
            evidence: vec![],
        },
        AdapterOutcome::Succeeded {
            outputs: request().inputs,
            evidence: vec![EvidenceRef {
                artifact_id: "artifact".into(),
                digest: "no-digest".into(),
            }],
        },
    ] {
        let (w, calls) = worker(Some(outcome));
        assert!(result(&w, &request()).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    let (w, _) = worker(Some(AdapterOutcome::Failed {
        code: "unavailable".into(),
        class: FailureClass::Transient,
        message: "service unavailable".into(),
        evidence: vec![],
    }));
    assert!(matches!(
        result(&w, &request()).unwrap().result().outcome,
        AdapterOutcome::Failed { .. }
    ));
}

#[test]
fn result_cannot_cross_requests_attempts_epochs_or_expired_grants() {
    let (w, _) = worker(None);
    let r = request();
    let output = result(&w, &r).unwrap().into_result();
    let capability = Capability::new(descriptor()).unwrap();
    let mut next = r.clone();
    next.request_id = "r2".into();
    assert_eq!(
        accept_result(
            &next,
            &ExecutionGrant::bind(&next).unwrap(),
            &capability,
            output.clone(),
            101
        )
        .unwrap_err()
        .code,
        ErrorCode::DigestMismatch
    );
    assert_eq!(
        accept_result(
            &r,
            &ExecutionGrant::bind(&r).unwrap(),
            &capability,
            output.clone(),
            2000
        )
        .unwrap_err()
        .code,
        ErrorCode::Expired
    );
    let mut output = output;
    output.completed_at_unix_ms = 102;
    assert_eq!(
        accept_result(
            &r,
            &ExecutionGrant::bind(&r).unwrap(),
            &capability,
            output,
            101
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidResult
    );
    let mut scoped = r;
    scoped.scope = InvocationScope::Workflow {
        definition_digest: digest(&"definition").unwrap(),
        run_id: "run".into(),
        node_id: "node".into(),
        node_instance_id: "node-1".into(),
        attempt_id: "attempt-1".into(),
        lease_epoch: 1,
    };
    let grant = ExecutionGrant::bind(&scoped).unwrap();
    for (attempt, epoch) in [("attempt-2", 1), ("attempt-1", 2)] {
        let mut changed = scoped.clone();
        if let InvocationScope::Workflow {
            attempt_id,
            lease_epoch,
            ..
        } = &mut changed.scope
        {
            *attempt_id = attempt.into();
            *lease_epoch = epoch;
        }
        assert_eq!(
            w.execute_with_clock(&changed, &grant, &Times::new(vec![100]))
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
    }
}

#[test]
fn deadlines_clock_rollback_panics_and_writes_fail_explicitly() {
    let (w, _) = worker(None);
    let r = request();
    let grant = ExecutionGrant::bind(&r).unwrap();
    assert_eq!(
        w.execute_with_clock(&r, &grant, &Times::new(vec![100, 100, 1100]))
            .unwrap_err()
            .code,
        ErrorCode::DeadlineExceeded
    );
    assert_eq!(
        w.execute_with_clock(&r, &grant, &Times::new(vec![100, 99]))
            .unwrap_err()
            .code,
        ErrorCode::ClockError
    );
    let mut w = Worker::default();
    w.register(Echo {
        calls: Arc::default(),
        descriptor: descriptor(),
        outcome: None,
        panic: true,
    })
    .unwrap();
    assert_eq!(result(&w, &r).unwrap_err().code, ErrorCode::AdapterPanicked);
    let mut d = descriptor();
    d.effects = EffectContract::Write {
        irreversible: false,
        idempotency: Idempotency::None,
        query: None,
        compensation: None,
    };
    let c = Capability::new(d.clone()).unwrap();
    let mut r = r;
    r.contract_digest = c.digest().into();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut w = Worker::default();
    w.register(Echo {
        calls: calls.clone(),
        descriptor: d,
        outcome: None,
        panic: false,
    })
    .unwrap();
    assert_eq!(
        result(&w, &r).unwrap_err().code,
        ErrorCode::UnsupportedEffect
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

fn workflow() -> Workflow {
    let d = descriptor();
    WorkflowBuilder::new("echo-flow", "1", "echo")
        .input("value", d.inputs["value"].clone())
        .node(Node {
            id: "echo".into(),
            kind: NodeKind::Task {
                capability: d.capability,
                policy: None,
            },
            inputs: d.inputs,
            outputs: d.outputs,
            bindings: BTreeMap::from([(
                "value".into(),
                Binding::WorkflowInput {
                    field: "value".into(),
                },
            )]),
            preconditions: vec![Condition::Eq {
                field: "value".into(),
                value: json!(7),
            }],
        })
        .node(Node {
            id: "done".into(),
            kind: NodeKind::Terminal {
                outcome: TerminalOutcome::Succeeded,
            },
            inputs: Contract::new(),
            outputs: Contract::new(),
            bindings: BTreeMap::new(),
            preconditions: vec![],
        })
        .edge(Edge {
            id: "finish".into(),
            from: "echo".into(),
            to: "done".into(),
            route: Route::Next,
        })
        .build()
}
fn node_request(w: &Workflow, inputs: Values) -> Result<WorkRequest> {
    WorkRequest::for_node(
        w,
        "echo",
        &Capability::new(descriptor()).unwrap(),
        inputs,
        RequestContext {
            request_id: "r1".into(),
            trace_id: "t1".into(),
            issued_at_unix_ms: 90,
            deadline_unix_ms: 2000,
        },
        NodeAttempt {
            run_id: "run".into(),
            node_instance_id: "echo-1".into(),
            attempt_id: "a1".into(),
            lease_epoch: 1,
        },
    )
}
#[test]
fn workflow_and_standalone_use_the_same_capability_contract_and_results() {
    let (worker, _) = worker(None);
    let workflow = workflow();
    assert!(workflow_validator::validate(&workflow, "test").is_empty());
    let standalone = request();
    let node = node_request(&workflow, standalone.inputs.clone()).unwrap();
    assert_eq!(node.capability, standalone.capability);
    assert_eq!(node.contract_digest, standalone.contract_digest);
    assert_eq!(
        result(&worker, &node).unwrap().result().outcome,
        result(&worker, &standalone).unwrap().result().outcome
    );
    assert_eq!(
        node_request(&workflow, Values::from([("value".into(), json!(8))]))
            .unwrap_err()
            .code,
        ErrorCode::PreconditionFailed
    );
    let mut mismatch = workflow.clone();
    mismatch.nodes[0].outputs.clear();
    assert_eq!(
        node_request(&mismatch, standalone.inputs.clone())
            .unwrap_err()
            .code,
        ErrorCode::InvalidBinding
    );
    let mut policy = workflow;
    if let NodeKind::Task { policy, .. } = &mut policy.nodes[0].kind {
        *policy = Some(VersionRef {
            id: "model-policy".into(),
            version: "1".into(),
        });
    }
    assert_eq!(
        node_request(&policy, standalone.inputs).unwrap_err().code,
        ErrorCode::InvalidBinding
    );
}

#[test]
fn wire_result_cannot_propose_a_transition_or_overwrite_state() {
    let (w, _) = worker(None);
    let raw = result(&w, &request()).unwrap().into_result();
    let mut value = serde_json::to_value(raw).unwrap();
    value["outcome"]["next_node"] = json!("skip-gate");
    assert!(parse_message::<WorkResult>(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn time_spent_validating_cannot_start_expired_work_or_return_a_late_result() {
    let (w, calls) = worker(None);
    let r = request();
    let grant = ExecutionGrant::bind(&r).unwrap();
    assert_eq!(
        w.execute_with_clock(&r, &grant, &Times::new(vec![100, 2000]))
            .unwrap_err()
            .code,
        ErrorCode::Expired
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        w.execute_with_clock(&r, &grant, &Times::new(vec![100, 100, 101, 1100]))
            .unwrap_err()
            .code,
        ErrorCode::DeadlineExceeded
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        w.execute_with_clock(&r, &grant, &Times::new(vec![100, 100, 101, 100]))
            .unwrap_err()
            .code,
        ErrorCode::ClockError
    );
}
