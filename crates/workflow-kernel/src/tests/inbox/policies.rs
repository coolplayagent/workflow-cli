use super::*;

fn policy() -> WaitPolicy {
    WaitPolicy {
        identity: VersionRef {
            id: "review-policy".into(),
            version: "1.0.0".into(),
        },
        kind: WaitKind::HumanApproval,
        responders: ["verified-host".into()].into(),
        subjects: [("revision".into(), SubjectKind::Digest)].into(),
        max_validity_ms: 1000,
        exception: Some(ExceptionPolicy {
            identity: VersionRef {
                id: "emergency".into(),
                version: "1.0.0".into(),
            },
            responders: ["incident-commander".into()].into(),
            codes: ["incident".into()].into(),
        }),
    }
}
fn protected_spec() -> BundleSpec {
    let mut s = spec(vec![fixture("review")]);
    let wait = s.workflows[0]
        .nodes
        .iter_mut()
        .find(|n| n.id == "review")
        .unwrap();
    wait.inputs.insert(
        "revision".into(),
        Field {
            value_type: ValueType::String,
            required: true,
        },
    );
    wait.bindings.insert(
        "revision".into(),
        Binding::Literal {
            value: serde_json::json!(format!("sha256:{}", "a".repeat(64))),
        },
    );
    s.wait_policies.push(WaitPolicyBinding {
        workflow: s.root.clone(),
        node_id: "review".into(),
        policy: policy(),
    });
    s
}
fn start_spec(s: BundleSpec) -> Engine {
    Engine::start(
        CompiledBundle::compile(s).unwrap(),
        "test-run",
        Values::new(),
        100,
        Limits::default(),
    )
    .unwrap()
    .0
}
fn response(e: &Engine, name: &str, decision: SignalDecision) -> SignalMessage {
    let mut m = message(e, name, decision);
    m.target.input_digest =
        workflow_worker::digest(&e.snapshot().frames[&1].nodes["review"].inputs).unwrap();
    m.correlation_id = signal_correlation(&e.snapshot().run_digest, &m.target).unwrap();
    m.expires_at_unix_ms = 1100;
    m
}

#[test]
fn protected_wait_rejects_raw_signals_and_wrong_actor_subject_validity_or_exception() {
    for (case, expected) in [
        ("actor", SignalRejection::ResponderNotAllowed),
        ("subject", SignalRejection::InputMismatch),
        ("validity", SignalRejection::ResponseValidityExceeded),
        ("exception-actor", SignalRejection::ExceptionNotAllowed),
        ("exception-code", SignalRejection::ExceptionNotAllowed),
        ("exception-version", SignalRejection::ExceptionNotAllowed),
        ("exception-decision", SignalRejection::ExceptionNotAllowed),
    ] {
        let mut e = start_spec(protected_spec());
        let before = e.snapshot().clone();
        let direct = event(
            &e,
            EventKind::Signal {
                instance_id: id(&e, 1, "review"),
                event: "design-review".into(),
                accepted: true,
                outputs: Values::new(),
            },
            101,
        );
        assert_eq!(e.apply(direct).unwrap_err().code, ErrorCode::InvalidSignal);
        assert_eq!(e.snapshot(), &before);
        let mut m = response(&e, "invalid", SignalDecision::Approve);
        match case {
            "actor" => m.source = "model-generated-person".into(),
            "subject" => {
                m.target.input_digest = workflow_worker::digest(&"stale artifact").unwrap()
            }
            "validity" => m.expires_at_unix_ms = 1102,
            _ => {
                m.exception = Some(ExceptionClaim {
                    policy: policy().exception.unwrap().identity,
                    code: "incident".into(),
                });
                if case != "exception-actor" {
                    m.source = "incident-commander".into();
                }
                if case == "exception-code" {
                    m.exception.as_mut().unwrap().code = "skip-all-checks".into();
                }
                if case == "exception-version" {
                    m.exception.as_mut().unwrap().policy.version = "2.0.0".into();
                }
                if case == "exception-decision" {
                    m.decision = SignalDecision::Reject;
                }
            }
        }
        m.correlation_id = signal_correlation(&e.snapshot().run_digest, &m.target).unwrap();
        receive(&mut e, m, 101);
        assert_eq!(rejection(&e, "invalid"), &expected, "{case}");
        assert!(matches!(
            e.snapshot().frames[&1].nodes["review"].state,
            NodeState::Waiting { .. }
        ));
        let m = response(&e, "valid", SignalDecision::Approve);
        receive(&mut e, m, 102);
        assert!(is_applied(&e, "valid"));
    }
}

#[test]
fn exception_is_distinct_and_audited_after_checkpoint_and_complete_replay() {
    let mut e = start_spec(protected_spec());
    let mut m = response(&e, "emergency", SignalDecision::Approve);
    m.source = "incident-commander".into();
    m.exception = Some(ExceptionClaim {
        policy: policy().exception.unwrap().identity,
        code: "incident".into(),
    });
    let observed = receive(&mut e, m.clone(), 101);
    assert!(is_applied(&e, "emergency"));
    assert_eq!(e.snapshot().inbox["emergency"].message, m);
    let restored = Engine::restore(e.bundle().clone(), e.checkpoint().unwrap()).unwrap();
    assert_eq!(restored.snapshot(), e.snapshot());
    let mut replay = start_spec(protected_spec());
    replay.apply(observed).unwrap();
    assert_eq!(replay.snapshot(), e.snapshot());
}

#[test]
fn invalid_subject_and_policy_contracts_cannot_authorize_work() {
    for change in [
        "missing-subject",
        "optional-subject",
        "wrong-type",
        "mutable-policy",
        "wrong-node",
        "duplicate",
        "no-responders",
        "no-subjects",
    ] {
        let mut s = protected_spec();
        match change {
            "missing-subject" => {
                s.wait_policies[0].policy.subjects =
                    [("missing".into(), SubjectKind::Digest)].into();
            }
            "optional-subject" => {
                s.workflows[0].nodes[0]
                    .inputs
                    .get_mut("revision")
                    .unwrap()
                    .required = false;
            }
            "wrong-type" => {
                s.wait_policies[0]
                    .policy
                    .subjects
                    .insert("revision".into(), SubjectKind::Artifact);
            }
            "mutable-policy" => {
                s.wait_policies[0].policy.identity.version = "latest".into();
            }
            "wrong-node" => {
                s.wait_policies[0].node_id = "implement".into();
            }
            "duplicate" => s.wait_policies.push(s.wait_policies[0].clone()),
            "no-responders" => s.wait_policies[0].policy.responders.clear(),
            _ => s.wait_policies[0].policy.subjects.clear(),
        }
        assert!(CompiledBundle::compile(s).is_err(), "{change}");
    }
    let mut s = protected_spec();
    s.workflows[0].nodes[0].bindings.insert(
        "revision".into(),
        Binding::Literal {
            value: "not-a-digest".into(),
        },
    );
    let mut e = start_spec(s);
    let m = response(&e, "bad-subject", SignalDecision::Approve);
    receive(&mut e, m, 101);
    assert_eq!(
        rejection(&e, "bad-subject"),
        &SignalRejection::InvalidSubject
    );
}

#[test]
fn rejection_and_request_changes_rework_to_a_new_subject_and_reject_old_approval() {
    for decision in [SignalDecision::Reject, SignalDecision::RequestChanges] {
        let mut s = protected_spec();
        let w = &mut s.workflows[0];
        let mut next_review = w.nodes[0].clone();
        next_review.id = "review-again".into();
        next_review.bindings.insert(
            "revision".into(),
            Binding::NodeOutput {
                node: "implement".into(),
                field: "revision".into(),
            },
        );
        w.nodes.push(next_review);
        w.nodes
            .iter_mut()
            .find(|n| n.id == "implement")
            .unwrap()
            .outputs
            .insert(
                "revision".into(),
                Field {
                    value_type: ValueType::String,
                    required: true,
                },
            );
        s.capabilities[0].outputs = w
            .nodes
            .iter()
            .find(|n| n.id == "implement")
            .unwrap()
            .outputs
            .clone();
        for edge in &mut w.edges {
            if edge.from == "review" && edge.route == Route::Rejected {
                edge.to = "implement".into();
            }
            if edge.from == "review" && edge.route == Route::Accepted {
                edge.to = "done".into();
            }
            if edge.from == "implement" {
                edge.to = "review-again".into();
            }
        }
        for (name, route, to) in [
            ("again-yes", Route::Accepted, "done"),
            ("again-no", Route::Rejected, "declined"),
            ("again-timeout", Route::TimedOut, "expired"),
        ] {
            w.edges.push(Edge {
                id: name.into(),
                from: "review-again".into(),
                to: to.into(),
                route,
            });
        }
        let mut binding = s.wait_policies[0].clone();
        binding.node_id = "review-again".into();
        s.wait_policies.push(binding);
        let mut e = start_spec(s);
        let old = response(&e, "old-approval", SignalDecision::Approve);
        let m = response(&e, "rework", decision.clone());
        receive(&mut e, m, 101);
        assert_eq!(e.snapshot().inbox["rework"].message.decision, decision);
        complete(
            &mut e,
            1,
            "implement",
            TaskResult::Succeeded {
                outputs: [(
                    "revision".into(),
                    serde_json::json!(format!("sha256:{}", "b".repeat(64))),
                )]
                .into(),
            },
        );
        receive(&mut e, old.clone(), 102);
        assert_eq!(
            rejection(&e, "old-approval"),
            &SignalRejection::AlreadySettled
        );
        let mut stale = old;
        stale.message_id = "stale-subject".into();
        stale.target.instance_id = id(&e, 1, "review-again");
        stale.correlation_id = signal_correlation(&e.snapshot().run_digest, &stale.target).unwrap();
        receive(&mut e, stale.clone(), 103);
        assert_eq!(
            rejection(&e, "stale-subject"),
            &SignalRejection::InputMismatch
        );
        let mut current = stale;
        current.message_id = "new-approval".into();
        current.target.input_digest =
            workflow_worker::digest(&e.snapshot().frames[&1].nodes["review-again"].inputs).unwrap();
        current.correlation_id =
            signal_correlation(&e.snapshot().run_digest, &current.target).unwrap();
        receive(&mut e, current, 104);
        assert!(is_applied(&e, "new-approval"));
        assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    }
}
