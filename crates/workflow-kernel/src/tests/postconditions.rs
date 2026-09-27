use super::*;
use serde_json::json;
use workflow_artifacts::{ArtifactLink, ArtifactReader, ArtifactRef, Producer};
use workflow_gates::{CheckOutcome, EvidenceSource, ExecutedCheck, Request, Verdict};
fn gated() -> (BundleSpec, Values) {
    let root = if let Ok(d) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(d).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    let value: serde_json::Value = workflow_worker::parse_message(
        &std::fs::read(root.join("examples/gates/guarded-start.json")).unwrap(),
    )
    .unwrap();
    (
        serde_json::from_value(value["bundle"].clone()).unwrap(),
        serde_json::from_value(value["inputs"].clone()).unwrap(),
    )
}
fn run(bundle: BundleSpec, inputs: Values) -> Engine {
    Engine::start(
        CompiledBundle::compile(bundle).unwrap(),
        "gated",
        inputs,
        1000,
        Limits::default(),
    )
    .unwrap()
    .0
}
fn inspect(e: &mut Engine, frame: u64) -> Transition {
    complete(
        e,
        frame,
        "inspect",
        TaskResult::Succeeded {
            outputs: Values::from([
                ("valid".into(), true.into()),
                ("diagnostics".into(), json!([])),
            ]),
        },
    )
}
fn context(e: &Engine, frame: u64, node: &str) -> GateContext {
    let NodeState::CheckingGate { context, .. } = &e.snapshot().frames[&frame].nodes[node].state
    else {
        panic!("gate")
    };
    *context.clone()
}
struct Source {
    reference: ArtifactRef,
    check: ExecutedCheck,
}
impl ArtifactReader for Source {
    fn verify(&self, link: &ArtifactLink) -> workflow_artifacts::Result<ArtifactRef> {
        assert_eq!(*link, self.reference.link());
        Ok(self.reference.clone())
    }
}
impl EvidenceSource for Source {
    fn executed_check(&self, p: &Producer) -> workflow_artifacts::Result<Option<ExecutedCheck>> {
        assert_eq!(*p, self.check.producer);
        Ok(Some(self.check.clone()))
    }
}
fn evaluation(c: &GateContext, passed: Option<bool>, now: u64) -> GateEvaluation {
    let q = &c.policy.requirements[0];
    let producer = Producer {
        run_id: c.target.run_id.clone(),
        node_instance_id: format!("instance-{}", c.expected_instances[&q.id]),
        attempt_id: "attempt-test".into(),
        request_digest: format!("sha256:{}", "0".repeat(64)),
        input_digest: c.target.input_digest.clone(),
    };
    let reference = workflow_artifacts::reference(
        &workflow_artifacts::PublishSpec {
            schema_version: 1,
            artifact_type: q.report_type.clone(),
            producer: producer.clone(),
            source_revision: c.target.source_revision.clone(),
            inputs: vec![],
            access: workflow_artifacts::AccessScope::Run {
                run_id: c.target.run_id.clone(),
            },
            retention: workflow_artifacts::Retention::RunDependency,
        },
        br#"{"valid":true}"#,
    )
    .unwrap();
    let request = Request {
        policy: c.policy.clone(),
        target: c.target.clone(),
        evidence: if passed.is_some() {
            vec![workflow_gates::Evidence {
                requirement_id: q.id.clone(),
                report: reference.link(),
            }]
        } else {
            vec![]
        },
    };
    let check = ExecutedCheck {
        producer,
        run_digest: c.target.run_digest.clone(),
        node_id: q.node_id.clone(),
        capability: q.capability.clone(),
        contract_digest: q.contract_digest.clone(),
        completed_at_unix_ms: 1000,
        settled_at_unix_ms: 1000,
        outcome: CheckOutcome::Succeeded {
            outputs: Values::from([("valid".into(), passed.unwrap_or(false).into())]),
        },
        evidence: vec![reference.link()],
    };
    let decision = workflow_gates::evaluate(&request, &Source { reference, check }, now).unwrap();
    GateEvaluation { request, decision }
}
fn observation(e: &Engine, frame: u64, node: &str, passed: Option<bool>) -> Event {
    let c = context(e, frame, node);
    event(
        e,
        EventKind::GateEvaluated {
            instance_id: id(e, frame, node),
            context_digest: workflow_worker::digest(&c).unwrap(),
            evaluation: Box::new(evaluation(&c, passed, e.snapshot().now_unix_ms)),
        },
        e.snapshot().now_unix_ms,
    )
}
#[test]
fn invalid_dynamic_target_preserves_observed_outputs_and_fails_without_retrying_work() {
    for node in ["inspect", "accepted"] {
        let (mut b, inputs) = gated();
        b.postconditions
            .iter_mut()
            .find(|g| g.node_id == node)
            .unwrap()
            .revision = workflow_ir::Binding::WorkflowInput {
            field: "format".into(),
        };
        let mut e = run(b, inputs);
        let mut t = inspect(&mut e, 1);
        if node == "accepted" {
            t = e.apply(observation(&e, 1, "inspect", Some(true))).unwrap();
        }
        assert!(t.commands.is_empty());
        assert_eq!(e.snapshot().status, RunStatus::Failed);
        let nodes = &e.snapshot().frames[&1].nodes;
        assert_eq!(nodes["inspect"].outputs["valid"], true);
        assert_eq!(nodes[node].state, NodeState::Failed);
        assert!(
            nodes[node]
                .reason
                .as_ref()
                .unwrap()
                .starts_with("postcondition_target:")
        );
        assert_eq!(
            Engine::restore(e.bundle().clone(), e.checkpoint().unwrap())
                .unwrap()
                .snapshot(),
            e.snapshot()
        );
    }
}
#[test]
fn observed_task_and_terminal_each_wait_for_their_own_gate_and_replay_exactly() {
    let (b, inputs) = gated();
    let mut e = run(b, inputs);
    let t = inspect(&mut e, 1);
    assert!(matches!(t.commands[0], Command::CheckGate { .. }));
    assert_eq!(
        e.snapshot().frames[&1].nodes["route"].state,
        NodeState::Pending
    );
    let fake = event(
        &e,
        EventKind::TaskCompleted {
            instance_id: id(&e, 1, "inspect"),
            result: success(),
        },
        1000,
    );
    assert_eq!(
        e.apply(fake).unwrap_err().code,
        ErrorCode::InvalidTaskResult
    );
    let restored = Engine::restore(e.bundle().clone(), e.checkpoint().unwrap()).unwrap();
    assert_eq!(restored.snapshot(), e.snapshot());
    let t = e.apply(observation(&e, 1, "inspect", Some(true))).unwrap();
    assert!(matches!(t.commands[0], Command::CheckGate { .. }));
    assert_eq!(e.snapshot().status, RunStatus::Running);
    assert_eq!(
        e.snapshot().frames[&1].nodes["inspect"].state,
        NodeState::Succeeded
    );
    let event = observation(&e, 1, "accepted", Some(true));
    e.apply(event.clone()).unwrap();
    assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    assert!(e.apply(event).unwrap().duplicate);
    assert_eq!(
        Engine::restore(e.bundle().clone(), e.checkpoint().unwrap())
            .unwrap()
            .snapshot(),
        e.snapshot()
    );
}
#[test]
fn unknown_is_durable_idle_until_explicit_retry_and_cancel_fences_late_results() {
    let (b, inputs) = gated();
    let mut e = run(b, inputs);
    inspect(&mut e, 1);
    let c = context(&e, 1, "inspect");
    e.apply(observation(&e, 1, "inspect", None)).unwrap();
    assert!(tick(&mut e, 1001).commands.is_empty());
    assert_eq!(
        e.snapshot().frames[&1].nodes["inspect"]
            .gate_decision
            .as_ref()
            .unwrap()
            .verdict,
        Verdict::Unknown
    );
    assert_eq!(
        Engine::restore(e.bundle().clone(), e.checkpoint().unwrap())
            .unwrap()
            .snapshot(),
        e.snapshot()
    );
    let retry = EventKind::RetryGate {
        instance_id: id(&e, 1, "inspect"),
        context_digest: workflow_worker::digest(&c).unwrap(),
    };
    assert_eq!(
        e.apply(event(&e, retry.clone(), 1001))
            .unwrap()
            .commands
            .len(),
        1
    );
    assert_eq!(
        e.apply(event(&e, retry, 1001)).unwrap_err().code,
        ErrorCode::InvalidGateResult
    );
    let late = observation(&e, 1, "inspect", Some(true));
    e.apply(event(&e, EventKind::Cancel, 1002)).unwrap();
    assert_eq!(e.snapshot().status, RunStatus::Cancelled);
    let mut late = late;
    late.expected_revision = e.snapshot().revision;
    late.event_id = "late".into();
    late.at_unix_ms = 1002;
    assert_eq!(e.apply(late).unwrap_err().code, ErrorCode::TerminalRun);
}
#[test]
fn altered_context_missing_checks_expired_or_other_instance_pass_is_rejected_atomically() {
    let (b, inputs) = gated();
    let mut e = run(b, inputs);
    inspect(&mut e, 1);
    let before = e.snapshot().clone();
    for mutate in [
        |ev: &mut Event| {
            if let EventKind::GateEvaluated { context_digest, .. } = &mut ev.kind {
                *context_digest = "other".into()
            }
        },
        |ev: &mut Event| {
            if let EventKind::GateEvaluated { evaluation, .. } = &mut ev.kind {
                evaluation.decision.checks.clear()
            }
        },
        |ev: &mut Event| {
            if let EventKind::GateEvaluated { evaluation, .. } = &mut ev.kind {
                evaluation.decision.checks[0]
                    .producer
                    .as_mut()
                    .unwrap()
                    .node_instance_id = "instance-999".into()
            }
        },
        |ev: &mut Event| {
            if let EventKind::GateEvaluated { evaluation, .. } = &mut ev.kind {
                evaluation.decision.checks[0].expires_at_unix_ms = Some(1000)
            }
        },
        |ev: &mut Event| {
            if let EventKind::GateEvaluated { evaluation, .. } = &mut ev.kind {
                evaluation.request.target.source_revision.revision = "b".repeat(40)
            }
        },
    ] {
        let mut ev = observation(&e, 1, "inspect", Some(true));
        mutate(&mut ev);
        assert_eq!(e.apply(ev).unwrap_err().code, ErrorCode::InvalidGateResult);
        assert_eq!(e.snapshot(), &before);
    }
}
fn loop_bundle() -> (BundleSpec, Values) {
    let (mut b, inputs) = gated();
    let child = b.root.clone();
    let contract = b.workflows[0].inputs.clone();
    let mut parent:Workflow=serde_json::from_value(json!({"schema_version":1,"id":"repair-gated","version":"1.0.0","entry":"repair","inputs":contract,"nodes":[
        {"id":"repair","kind":{"type":"loop","body":child,"max_iterations":3,"deadline_ms":100},"inputs":contract,"bindings":{"document":{"source":"workflow_input","field":"document"},"format":{"source":"workflow_input","field":"format"}}},
        {"id":"done","kind":{"type":"terminal","outcome":"succeeded"}}, {"id":"exhausted","kind":{"type":"terminal","outcome":"failed"}}
    ],"edges":[{"id":"done","from":"repair","to":"done","route":{"type":"completed"}},{"id":"exhausted","from":"repair","to":"exhausted","route":{"type":"exhausted"}}]})).unwrap();
    parent.nodes.sort_by(|a, b| a.id.cmp(&b.id));
    b.root = VersionRef {
        id: parent.id.clone(),
        version: parent.version.clone(),
    };
    b.workflows.push(parent);
    (b, inputs)
}
#[test]
fn confirmed_gate_failure_uses_declared_repair_budget_and_unknown_obeys_total_deadline() {
    let (b, inputs) = loop_bundle();
    let mut e = run(b.clone(), inputs.clone());
    let mut old = None;
    for frame in 2..=4 {
        inspect(&mut e, frame);
        if let Some(mut prior) = old.take() {
            let prior: &mut Event = &mut prior;
            prior.event_id = format!("stale-{frame}");
            prior.expected_revision = e.snapshot().revision;
            assert_eq!(
                e.apply(prior.clone()).unwrap_err().code,
                ErrorCode::InvalidGateResult
            );
        }
        old = Some(observation(&e, frame, "inspect", Some(true)));
        e.apply(observation(&e, frame, "inspect", Some(false)))
            .unwrap();
    }
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert_eq!(e.snapshot().frames.len(), 4);
    assert_eq!(
        e.snapshot().frames[&1].nodes["repair"].reason.as_deref(),
        Some("loop_exhausted")
    );
    let mut e = run(b, inputs);
    inspect(&mut e, 2);
    e.apply(observation(&e, 2, "inspect", None)).unwrap();
    tick(&mut e, 1100);
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert_eq!(e.snapshot().frames.len(), 2);
}
#[test]
fn bundle_rejects_duplicate_or_weak_check_contracts_before_start() {
    let (b, _) = gated();
    for mutate in [
        |b: &mut BundleSpec| b.postconditions.push(b.postconditions[0].clone()),
        |b: &mut BundleSpec| {
            b.postconditions[0].policy.requirements[0].contract_digest =
                format!("sha256:{}", "0".repeat(64))
        },
        |b: &mut BundleSpec| b.postconditions[0].policy.requirements[0].node_id = "accepted".into(),
        |b: &mut BundleSpec| b.postconditions[0].input_node = "accepted".into(),
        |b: &mut BundleSpec| {
            b.postconditions[0].repository = Binding::Literal {
                value: false.into(),
            }
        },
        |b: &mut BundleSpec| b.postconditions[0].policy.requirements[0].max_age_ms = 1,
    ] {
        let mut x = b.clone();
        mutate(&mut x);
        assert!(CompiledBundle::compile(x).is_err());
    }
}
