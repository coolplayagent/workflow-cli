use super::*;

fn fixture() -> (BundleSpec, Values) {
    let (mut b, mut inputs) = gated();
    b.postconditions[1].action = b.postconditions[0].action.clone();
    let mut initial = run(b.clone(), inputs.clone());
    inspect(&mut initial, 1);
    let c = context(&initial, 1, "inspect");
    let review = workflow_gates::review_digest(&c.policy, &c.target).unwrap();
    let field = json!({"value_type":{"type":"string"},"required":true});
    let w = &mut b.workflows[0];
    w.inputs.insert(
        "review_digest".into(),
        serde_json::from_value(field.clone()).unwrap(),
    );
    w.nodes.push(
        serde_json::from_value(json!({
            "id":"review", "kind":{"type":"wait","event":"release-review","timeout_ms":60000},
            "inputs":{"review_digest":field},
            "bindings":{"review_digest":{"source":"workflow_input","field":"review_digest"}}
        }))
        .unwrap(),
    );
    w.entry = "review".into();
    for (id, route, to) in [
        ("approved", "accepted", "inspect"),
        ("rejected", "rejected", "invalid"),
        ("expired", "timed_out", "invalid"),
    ] {
        w.edges.push(
            serde_json::from_value(json!({"id":id,"from":"review","to":to,"route":{"type":route}}))
                .unwrap(),
        );
    }
    b.wait_policies.push(serde_json::from_value(json!({
        "workflow":b.root,"node_id":"review","policy":{
            "identity":{"id":"release-review","version":"1.0.0"},"kind":"human_approval",
            "responders":["reviewer"],"subjects":{"review_digest":"digest"},"max_validity_ms":10000,
            "exception":{"identity":{"id":"emergency","version":"1.0.0"},"responders":["emergency-reviewer"],"codes":["incident-recovery"]}
        }
    })).unwrap());
    for p in &mut b.postconditions {
        p.exception = Some(workflow_gates::ApprovalRequirement {
            node_id: "review".into(),
            subject_field: "review_digest".into(),
        });
    }
    inputs.insert("review_digest".into(), review.into());
    (b, inputs)
}

fn approve(e: &mut Engine, actor: &str, exception: bool) {
    let target = WaitTarget {
        instance_id: id(e, 1, "review"),
        definition_digest: e.snapshot().frames[&1].definition_digest.clone(),
        input_digest: workflow_worker::digest(&e.snapshot().frames[&1].nodes["review"].inputs)
            .unwrap(),
        event: "release-review".into(),
    };
    let message = SignalMessage {
        schema_version: 1,
        message_id: "approval".into(),
        correlation_id: signal_correlation(&e.snapshot().run_digest, &target).unwrap(),
        target,
        source: actor.into(),
        decision: SignalDecision::Approve,
        reason: "Restore service under incident scope".into(),
        outputs: Values::new(),
        expires_at_unix_ms: 1500,
        exception: exception.then(|| ExceptionClaim {
            policy: VersionRef {
                id: "emergency".into(),
                version: "1.0.0".into(),
            },
            code: "incident-recovery".into(),
        }),
    };
    e.apply(event(
        e,
        EventKind::ReceiveSignal {
            message: Box::new(message),
        },
        1000,
    ))
    .unwrap();
}

fn excepted(e: &Engine, node: &str, passed: Option<bool>) -> Event {
    let mut ev = observation(e, 1, node, passed);
    if let EventKind::GateEvaluated { evaluation, .. } = &mut ev.kind {
        evaluation.exception = context(e, 1, node).exception;
    }
    ev
}

#[test]
fn scoped_exception_preserves_fail_and_unknown_at_both_boundaries_and_survives_replay() {
    for (passed, verdict) in [(Some(false), Verdict::Fail), (None, Verdict::Unknown)] {
        let (b, inputs) = fixture();
        let mut e = run(b, inputs);
        approve(&mut e, "emergency-reviewer", true);
        inspect(&mut e, 1);
        for node in ["inspect", "accepted"] {
            let mut forged = excepted(&e, node, passed);
            if let EventKind::GateEvaluated { evaluation, .. } = &mut forged.kind {
                evaluation.exception.as_mut().unwrap().actor = "intruder".into();
            }
            assert_eq!(
                e.apply(forged).unwrap_err().code,
                ErrorCode::InvalidGateResult
            );
            e.apply(excepted(&e, node, passed)).unwrap();
            let n = &e.snapshot().frames[&1].nodes[node];
            assert_eq!(n.gate_decision.as_ref().unwrap().verdict, verdict);
            let approval = n.gate_exception.as_ref().unwrap();
            assert_eq!(approval.actor, "emergency-reviewer");
            assert_eq!(approval.reason, "Restore service under incident scope");
        }
        assert_eq!(e.snapshot().status, RunStatus::Succeeded);
        assert_eq!(
            Engine::restore(e.bundle().clone(), e.checkpoint().unwrap())
                .unwrap()
                .snapshot(),
            e.snapshot()
        );
    }
}

#[test]
fn approved_quality_run_cannot_remove_its_checks_by_definition_migration() {
    let (mut b, inputs) = fixture();
    let mut e = run(b.clone(), inputs.clone());
    approve(&mut e, "emergency-reviewer", true);
    e.apply(event(
        &e,
        EventKind::Pause {
            reason: "review a definition change".into(),
        },
        1001,
    ))
    .unwrap();
    b.postconditions.clear();
    let request = MigrationRequest {
        migration_id: "remove-checks".into(),
        target_bundle: b,
        target_inputs: inputs,
        execution_policy: MigrationExecutionPolicy::RestartWithFreshEvidence,
        timer_policy: MigrationTimerPolicy::CancelAndRearmOnResume,
        node_mapping: vec![],
        decision_summary: "Try dropping original quality contract".into(),
    };
    assert!(
        e.plan_migration(&request)
            .unwrap_err()
            .message
            .contains("keeps its original graph")
    );
}
#[test]
fn ordinary_approval_wrong_actor_expiry_and_changed_subject_cannot_waive_a_check() {
    for case in ["ordinary", "actor", "expiry", "subject"] {
        let (b, mut inputs) = fixture();
        if case == "subject" {
            inputs.insert(
                "review_digest".into(),
                format!("sha256:{}", "a".repeat(64)).into(),
            );
        }
        let mut e = run(b, inputs);
        approve(
            &mut e,
            if case == "ordinary" || case == "actor" {
                "reviewer"
            } else {
                "emergency-reviewer"
            },
            case != "ordinary",
        );
        if case == "actor" {
            assert!(matches!(
                e.snapshot().inbox["approval"].status,
                SignalStatus::Rejected {
                    reason: SignalRejection::ExceptionNotAllowed,
                    ..
                }
            ));
            continue;
        }
        if case == "expiry" {
            tick(&mut e, 1500);
        }
        inspect(&mut e, 1);
        if case != "subject" {
            assert!(context(&e, 1, "inspect").exception.is_none());
            e.apply(observation(&e, 1, "inspect", Some(false))).unwrap();
        }
        assert_eq!(e.snapshot().status, RunStatus::Failed);
        assert!(
            e.snapshot().frames[&1].nodes["inspect"]
                .gate_exception
                .is_none()
        );
    }
}
