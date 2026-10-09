use super::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use workflow_ir::{Contract, Field, ValueType, VersionRef};
use workflow_worker::*;
fn reference(id: &str) -> VersionRef {
    VersionRef {
        id: id.into(),
        version: "1.0.0".into(),
    }
}
fn descriptor(id: &str) -> CapabilityDescriptor {
    let contract = Contract::from([(
        "value".into(),
        Field {
            required: true,
            value_type: ValueType::Integer,
        },
    )]);
    CapabilityDescriptor {
        schema_version: 1,
        capability: reference(id),
        inputs: contract.clone(),
        outputs: contract,
        timeout_ms: 10000,
        error_codes: BTreeMap::new(),
        effects: EffectContract::ReadOnly,
        usage: "Return an integer".into(),
        skill: None,
    }
}
fn policy() -> Policy {
    let mut task = descriptor("sop.echo");
    for code in [
        "model_unavailable",
        "model_invalid_response",
        "model_refused",
        "model_budget",
        "model_deadline",
    ] {
        task.error_codes
            .insert(code.into(), FailureClass::Permanent);
    }
    Policy::new(PolicySpec {
        retry: None,
        schema_version: 1,
        policy: reference("sop.echo-policy"),
        task,
        goal: "Use echo, then return its observed value".into(),
        tools: vec![descriptor("tool.echo")],
        budget: Budget {
            model_calls: 3,
            tool_calls: 2,
            context_bytes: 32768,
            response_bytes: 8192,
            output_tokens_per_call: 1024,
        },
    })
    .unwrap()
}
fn request(p: &Policy) -> WorkRequest {
    WorkRequest::standalone(
        &Capability::new(p.spec().task.clone()).unwrap(),
        Values::from([("value".into(), json!(7))]),
        RequestContext {
            request_id: "request".into(),
            trace_id: "trace".into(),
            issued_at_unix_ms: 90,
            deadline_unix_ms: 1000,
        },
    )
    .unwrap()
    .with_model_policy(p.binding().clone())
    .unwrap()
}
struct Time;
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(100)
    }
}
struct Echo(Arc<AtomicUsize>);
impl CapabilityAdapter for Echo {
    fn descriptor(&self) -> CapabilityDescriptor {
        descriptor("tool.echo")
    }
    fn invoke(&self, i: Invocation<'_>) -> AdapterOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        AdapterOutcome::Succeeded {
            outputs: i.request.inputs.clone(),
            evidence: vec![],
        }
    }
}
struct Planner {
    calls: Arc<AtomicUsize>,
    action: Option<Action>,
}
impl ModelAdapter for Planner {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            adapter: reference("test.planner"),
            model: "fixture-model".into(),
            binding_digest: digest(&"fixture").unwrap(),
        }
    }
    fn complete(&self, call: &ModelCall) -> Reply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let action = self.action.clone().unwrap_or_else(|| {
            if let Some(ModelEvent::ToolFinished {
                response: ToolReply::Received { result },
                ..
            }) = call.events.last()
            {
                let AdapterOutcome::Succeeded { outputs, .. } = &result.outcome else {
                    panic!()
                };
                Action::Complete {
                    outputs: outputs.clone(),
                    summary: "Return observed tool value".into(),
                }
            } else {
                Action::Call {
                    capability: reference("tool.echo"),
                    inputs: call.inputs.clone(),
                    summary: "Inspect using the declared tool".into(),
                }
            }
        });
        Reply::Received {
            reply: ModelReply {
                proposal: Proposal {
                    protocol_version: 1,
                    action,
                },
                resolved_model: "fixture-model-v1".into(),
                response_id: "reply-1".into(),
                usage: Usage {
                    input_tokens: Some(100),
                    output_tokens: Some(30),
                },
            },
        }
    }
}
fn tools(calls: Arc<AtomicUsize>) -> Worker {
    let mut w = Worker::default();
    w.register(Echo(calls)).unwrap();
    w
}
#[test]
fn bounded_model_tool_loop_replays_without_resampling_or_reinvoking() {
    let p = policy();
    let r = request(&p);
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let model = Planner {
        calls: model_calls.clone(),
        action: None,
    };
    let result = execute(&p, &r, &model, &tools(tool_calls.clone()), &Time, 900).unwrap();
    assert_eq!(
        result.outcome,
        AdapterOutcome::Succeeded {
            outputs: r.inputs.clone(),
            evidence: vec![]
        }
    );
    assert_eq!(result.events.len(), 3);
    let restored: ModelRecord = parse_message(&to_message(&result).unwrap()).unwrap();
    verify_record(&p, &r, &restored).unwrap();
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    let mut forged = result.clone();
    forged.outcome = AdapterOutcome::Succeeded {
        outputs: Values::from([("value".into(), json!(99))]),
        evidence: vec![],
    };
    assert!(verify_record(&p, &r, &forged).is_err());
    let mut altered = r.clone();
    altered.trace_id = "another".into();
    assert!(verify_record(&p, &altered, &result).is_err());
    let mut forged = result;
    if let ModelEvent::ToolFinished { request, .. } = &mut forged.events[1] {
        request.inputs.insert("value".into(), json!(0));
    }
    assert!(verify_record(&p, &r, &forged).is_err());
}
#[test]
fn undeclared_tools_invalid_outputs_and_budget_exhaustion_never_succeed() {
    let p = policy();
    let r = request(&p);
    let cases = [
        (
            Action::Call {
                capability: reference("forbidden.write"),
                inputs: Values::new(),
                summary: "Call".into(),
            },
            0,
        ),
        (
            Action::Complete {
                outputs: Values::from([("value".into(), json!("bad"))]),
                summary: "Done".into(),
            },
            0,
        ),
        (
            Action::Call {
                capability: reference("tool.echo"),
                inputs: r.inputs.clone(),
                summary: "Repeat".into(),
            },
            2,
        ),
    ];
    for (action, expected) in cases {
        let count = Arc::new(AtomicUsize::new(0));
        let model = Planner {
            calls: Arc::new(AtomicUsize::new(0)),
            action: Some(action),
        };
        let record = execute(&p, &r, &model, &tools(count.clone()), &Time, 900).unwrap();
        assert!(matches!(record.outcome, AdapterOutcome::Failed { .. }));
        verify_record(&p, &r, &record).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), expected);
    }
    assert!(parse_message::<Proposal>(br#"{"protocol_version":1,"action":{"type":"complete","outputs":{"value":7},"summary":"done","transition":"succeeded"}}"#).is_err());
}
#[test]
fn missing_or_changed_tools_are_detected_before_model_invocation() {
    let p = policy();
    let count = Arc::new(AtomicUsize::new(0));
    let model = Planner {
        calls: count.clone(),
        action: None,
    };
    assert!(execute(&p, &request(&p), &model, &Worker::default(), &Time, 900).is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let mut spec = p.spec().clone();
    spec.tools[0].effects = EffectContract::Write {
        irreversible: false,
        idempotency: Idempotency::None,
        query: None,
        compensation: None,
    };
    assert!(Policy::new(spec).is_err());
}
#[test]
fn incomplete_extra_or_time_forged_history_cannot_be_a_completion() {
    let p = policy();
    let r = request(&p);
    let model = Planner {
        calls: Arc::new(AtomicUsize::new(0)),
        action: None,
    };
    let record = execute(
        &p,
        &r,
        &model,
        &tools(Arc::new(AtomicUsize::new(0))),
        &Time,
        900,
    )
    .unwrap();
    for mode in 0..4 {
        let mut altered = record.clone();
        match mode {
            0 => {
                altered.events.pop();
            }
            1 => altered.events.push(altered.events[0].clone()),
            2 => {
                if let ModelEvent::Replied { at_unix_ms, .. } = &mut altered.events[0] {
                    *at_unix_ms = 900;
                }
            }
            _ => altered.policy_digest = digest(&"foreign").unwrap(),
        }
        assert!(verify_record(&p, &r, &altered).is_err());
    }
}

#[test]
fn worker_selects_the_exact_policy_and_cannot_downgrade_to_direct_execution() {
    let p = policy();
    let mut changed = p.spec().clone();
    changed.policy.id = "another.policy".into();
    changed.goal = "Use the same tool under another frozen goal".into();
    let other = Policy::new(changed).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let mut worker = Worker::default();
    for policy in [p.clone(), other.clone()] {
        worker
            .register(
                ModelCapability::new(
                    policy,
                    Arc::new(Planner {
                        calls: count.clone(),
                        action: None,
                    }),
                    tools(Arc::new(AtomicUsize::new(0))),
                )
                .unwrap(),
            )
            .unwrap();
    }
    let now = SystemClock.now_unix_ms().unwrap();
    let mut r = request(&p);
    r.issued_at_unix_ms = now;
    r.deadline_unix_ms = now + 10000;
    let grant = ExecutionGrant::bind(&r).unwrap();
    let mut wrong = r.clone();
    wrong.model_policy = Some(other.binding().clone());
    assert!(worker.execute(&wrong, &grant).is_err());
    let mut direct = r.clone();
    direct.model_policy = None;
    direct.protocol_version = 1;
    assert!(
        worker
            .execute(&direct, &ExecutionGrant::bind(&direct).unwrap())
            .is_err()
    );
    let mut downgrade = r.clone();
    downgrade.protocol_version = 1;
    assert!(downgrade.validate_shape().is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let accepted = worker.execute(&r, &grant).unwrap().into_result();
    verify_result(&p, &r, &accepted).unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(verify_result(&other, &r, &accepted).is_err());
}

fn retry_policy() -> Policy {
    let mut p = policy().spec().clone();
    p.retry = Some(RetryPolicy {
        max_retries: 2,
        initial_backoff_ms: 10,
        max_backoff_ms: 20,
    });
    for code in ["model_rate_limited", "model_authentication"] {
        p.task
            .error_codes
            .insert(code.into(), FailureClass::Permanent);
    }
    Policy::new(p).unwrap()
}
#[test]
fn retry_eligibility_and_admitted_call_budget_survive_checkpoint_replay() {
    let p = retry_policy();
    let r = request(&p);
    let planner = Planner {
        calls: Arc::new(AtomicUsize::new(0)),
        action: Some(Action::Complete {
            outputs: r.inputs.clone(),
            summary: "verified".into(),
        }),
    };
    let mut s = Session::new(p.clone(), r.clone(), planner.identity(), 900).unwrap();
    let Next::Model(call) = s.next_action().unwrap() else {
        panic!()
    };
    s.replied(
        digest(&call).unwrap(),
        100,
        Reply::Failed {
            failure: ModelFailure::Temporary {
                retry_after_ms: None,
            },
        },
    )
    .unwrap();
    assert_eq!(s.retry_at(), 110);
    let checkpoint: ModelCheckpoint =
        parse_message(&to_message(&s.checkpoint(None)).unwrap()).unwrap();
    let mut s = Session::restore(&p, &checkpoint).unwrap();
    let Next::Model(call) = s.next_action().unwrap() else {
        panic!()
    };
    assert_eq!(call.remaining_model_calls, 2);
    let failure = Reply::Failed {
        failure: ModelFailure::RateLimited {
            retry_after_ms: Some(15),
        },
    };
    assert!(
        s.replied(digest(&call).unwrap(), 109, failure.clone())
            .is_err()
    );
    s.replied(digest(&call).unwrap(), 110, failure).unwrap();
    assert_eq!(
        s.retry_at(),
        130,
        "Retry-After and exponential backoff are both respected"
    );
    let mut s = Session::restore(&p, &s.checkpoint(None)).unwrap();
    let Next::Model(call) = s.next_action().unwrap() else {
        panic!()
    };
    assert_eq!(call.remaining_model_calls, 1);
    s.replied(digest(&call).unwrap(), 130, planner.complete(&call))
        .unwrap();
    let record = s.record().unwrap();
    verify_record(&p, &r, &record).unwrap();
    assert!(matches!(record.outcome, AdapterOutcome::Succeeded { .. }));
    for failure in [
        ModelFailure::Authentication,
        ModelFailure::Refused,
        ModelFailure::InvalidResponse,
        ModelFailure::RateLimited {
            retry_after_ms: Some(21),
        },
        ModelFailure::Temporary {
            retry_after_ms: Some(1000),
        },
    ] {
        let mut s = Session::new(p.clone(), r.clone(), planner.identity(), 900).unwrap();
        let Next::Model(call) = s.next_action().unwrap() else {
            panic!()
        };
        s.replied(digest(&call).unwrap(), 100, Reply::Failed { failure })
            .unwrap();
        assert!(matches!(s.next_action().unwrap(), Next::Complete));
    }
}
struct MemoryJournal {
    checkpoint: std::sync::Mutex<Option<ModelCheckpoint>>,
    saves: AtomicUsize,
    fail_at: usize,
}
impl SessionJournal for MemoryJournal {
    fn load(&self, _: &WorkRequest) -> Result<Option<ModelCheckpoint>> {
        Ok(self.checkpoint.lock().unwrap().clone())
    }
    fn save(
        &self,
        _: &WorkRequest,
        previous: Option<&ModelCheckpoint>,
        next: &ModelCheckpoint,
    ) -> Result<()> {
        if self.saves.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at {
            return Err(Error::new(
                ErrorCode::InvalidResult,
                "simulated process loss before acknowledgement",
            ));
        }
        let mut stored = self.checkpoint.lock().unwrap();
        assert_eq!(stored.as_ref(), previous);
        verify_checkpoint_update(previous, next, 100).unwrap();
        *stored = Some(next.clone());
        Ok(())
    }
}
#[test]
fn acknowledged_tools_are_not_repeated_after_journal_reopen() {
    let p = policy();
    let r = request(&p);
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Planner {
        calls: calls.clone(),
        action: None,
    };
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tools = tools(tool_calls.clone());
    let journal = MemoryJournal {
        checkpoint: std::sync::Mutex::new(None),
        saves: AtomicUsize::new(0),
        fail_at: 5,
    };
    assert!(execute_journaled(&p, &r, &model, &tools, &Time, 900, Some(&journal)).is_err());
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let record = execute_journaled(&p, &r, &model, &tools, &Time, 900, Some(&journal)).unwrap();
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    verify_record(&p, &r, &record).unwrap();
    let original = journal.load(&r).unwrap().unwrap();
    let mut foreign = original.clone();
    foreign.identity.model = "changed-provider".into();
    assert!(verify_checkpoint_update(Some(&original), &foreign, 100).is_err());
    foreign = original.clone();
    foreign.events.remove(0);
    assert!(Session::restore(&p, &foreign).is_err());
    foreign = original.clone();
    foreign.request.inputs.insert("value".into(), json!(99));
    assert!(Session::restore(&p, &foreign).is_err());
}
#[test]
fn unacknowledged_call_is_explicitly_uncertain_and_consumes_the_original_budget() {
    let p = policy();
    let r = request(&p);
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Planner {
        calls: calls.clone(),
        action: None,
    };
    let journal = MemoryJournal {
        checkpoint: std::sync::Mutex::new(None),
        saves: AtomicUsize::new(0),
        fail_at: 2,
    };
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tools = tools(tool_calls.clone());
    assert!(execute_journaled(&p, &r, &model, &tools, &Time, 900, Some(&journal)).is_err());
    let record = execute_journaled(&p, &r, &model, &tools, &Time, 900, Some(&journal)).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "legacy policy does not authorize another provider call"
    );
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        &record.events[0],
        ModelEvent::Replied {
            response: Reply::Failed {
                failure: ModelFailure::Uncertain
            },
            ..
        }
    ));
    verify_record(&p, &r, &record).unwrap();
}
