use super::*;

fn message(e: &Engine, name: &str, decision: SignalDecision) -> SignalMessage {
    let target = WaitTarget {
        instance_id: id(e, 1, "review"),
        definition_digest: e.snapshot().frames[&1].definition_digest.clone(),
        input_digest: workflow_worker::digest(&Values::new()).unwrap(),
        event: "design-review".into(),
    };
    SignalMessage {
        schema_version: 1,
        message_id: name.into(),
        correlation_id: signal_correlation(&e.snapshot().run_digest, &target).unwrap(),
        target,
        source: "verified-host".into(),
        decision,
        reason: "fixture observation".into(),
        outputs: Values::new(),
        expires_at_unix_ms: 86_400_100,
    }
}
fn receive(e: &mut Engine, message: SignalMessage, at: u64) -> Event {
    let event = event(
        e,
        EventKind::ReceiveSignal {
            message: Box::new(message),
        },
        at,
    );
    e.apply(event.clone()).unwrap();
    event
}
fn is_applied(e: &Engine, name: &str) -> bool {
    matches!(
        e.snapshot().inbox[name].status,
        SignalStatus::Applied { .. }
    )
}
fn rejection<'a>(e: &'a Engine, name: &str) -> &'a SignalRejection {
    let SignalStatus::Rejected { reason, .. } = &e.snapshot().inbox[name].status else {
        panic!("expected rejected receipt")
    };
    reason
}

#[test]
fn inbox_applies_exact_target_once_and_retains_late_conflicting_decisions() {
    let (mut e, _) = start(vec![fixture("review")]);
    let accepted = message(&e, "approved", SignalDecision::Approve);
    let original = receive(&mut e, accepted, 101);
    assert!(is_applied(&e, "approved"));
    assert_eq!(
        e.snapshot().frames[&1].nodes["implement"].state,
        NodeState::TaskReady
    );
    assert!(e.apply(original).unwrap().duplicate);
    let late = message(&e, "late-rejection", SignalDecision::Reject);
    receive(&mut e, late, 102);
    assert_eq!(
        rejection(&e, "late-rejection"),
        &SignalRejection::AlreadySettled
    );
    complete(&mut e, 1, "implement", success());
    let terminal = message(&e, "after-terminal", SignalDecision::Approve);
    receive(&mut e, terminal, 103);
    assert_eq!(
        rejection(&e, "after-terminal"),
        &SignalRejection::AlreadySettled
    );
    assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    assert_eq!(
        Engine::restore(e.bundle().clone(), e.checkpoint().unwrap())
            .unwrap()
            .snapshot(),
        e.snapshot()
    );
}

#[test]
fn early_callback_survives_checkpoint_until_wait_activation_and_applies_in_receipt_order() {
    let mut workflow = fixture("review");
    let mut prepare = workflow
        .nodes
        .iter()
        .find(|n| n.id == "implement")
        .unwrap()
        .clone();
    prepare.id = "prepare".into();
    workflow.nodes.push(prepare);
    workflow.entry = "prepare".into();
    workflow.edges.push(Edge {
        id: "prepared".into(),
        from: "prepare".into(),
        to: "review".into(),
        route: Route::Next,
    });
    let (mut e, _) = start(vec![workflow]);
    let first = message(&e, "z-first", SignalDecision::RequestChanges);
    receive(&mut e, first, 101);
    let second = message(&e, "a-second", SignalDecision::Approve);
    receive(&mut e, second, 102);
    assert_eq!(e.snapshot().inbox["z-first"].status, SignalStatus::Pending);
    let mut e = Engine::restore(e.bundle().clone(), e.checkpoint().unwrap()).unwrap();
    complete(&mut e, 1, "prepare", success());
    assert!(is_applied(&e, "z-first"));
    assert_eq!(rejection(&e, "a-second"), &SignalRejection::AlreadySettled);
    assert_eq!(e.snapshot().status, RunStatus::Cancelled);
    assert_eq!(
        e.snapshot().frames[&1].nodes["implement"].state,
        NodeState::Skipped
    );
}

#[test]
fn paused_callbacks_are_durable_but_resume_rechecks_message_and_wait_expiry() {
    for (expiry, resume_at, expected) in [
        (200, 103, None),
        (102, 103, Some(SignalRejection::Expired)),
        (90_000_000, 86_400_100, Some(SignalRejection::WaitExpired)),
    ] {
        let (mut e, _) = start(vec![fixture("review")]);
        e.apply(event(
            &e,
            EventKind::Pause {
                reason: "maintenance".into(),
            },
            101,
        ))
        .unwrap();
        let mut m = message(&e, "callback", SignalDecision::Approve);
        m.expires_at_unix_ms = expiry;
        receive(&mut e, m, 101);
        assert_eq!(e.snapshot().inbox["callback"].status, SignalStatus::Pending);
        assert_eq!(
            e.snapshot().frames[&1].nodes["implement"].state,
            NodeState::Pending
        );
        let mut e = Engine::restore(e.bundle().clone(), e.checkpoint().unwrap()).unwrap();
        e.apply(event(
            &e,
            EventKind::Resume {
                reason: "ready".into(),
            },
            resume_at,
        ))
        .unwrap();
        if let Some(reason) = expected {
            assert_eq!(rejection(&e, "callback"), &reason);
        } else {
            assert!(is_applied(&e, "callback"));
        }
    }
}

#[test]
fn input_definition_event_and_output_mismatches_never_approve_waits() {
    for case in [
        "input",
        "definition",
        "event",
        "output",
        "unknown",
        "not-wait",
    ] {
        let (mut e, _) = start(vec![fixture("review")]);
        let mut m = message(&e, "bad", SignalDecision::Approve);
        let expected = match case {
            "input" => {
                m.target.input_digest = workflow_worker::digest(&"other inputs").unwrap();
                SignalRejection::InputMismatch
            }
            "definition" => {
                m.target.definition_digest = workflow_worker::digest(&"other definition").unwrap();
                SignalRejection::DefinitionMismatch
            }
            "event" => {
                m.target.event = "another-event".into();
                SignalRejection::EventMismatch
            }
            "unknown" => {
                m.target.instance_id = 999;
                SignalRejection::UnknownInstance
            }
            "not-wait" => {
                m.target.instance_id = id(&e, 1, "implement");
                SignalRejection::NotAWait
            }
            _ => {
                m.outputs
                    .insert("undeclared".into(), serde_json::json!(true));
                SignalRejection::InvalidOutputs
            }
        };
        m.correlation_id = signal_correlation(&e.snapshot().run_digest, &m.target).unwrap();
        receive(&mut e, m, 101);
        assert_eq!(rejection(&e, "bad"), &expected);
        assert_eq!(
            e.snapshot().frames[&1].nodes["implement"].state,
            NodeState::Pending
        );
    }
}

#[test]
fn malformed_correlations_and_rejection_outputs_are_refused_without_any_journal_change() {
    let (mut e, _) = start(vec![fixture("review")]);
    for case in ["correlation", "rejection-output", "version", "empty-reason"] {
        let mut m = message(&e, "bad", SignalDecision::Reject);
        match case {
            "correlation" => m.correlation_id = "foreign".into(),
            "rejection-output" => {
                m.outputs.insert("a".into(), serde_json::json!(1));
            }
            "version" => m.schema_version = 2,
            _ => m.reason.clear(),
        }
        let before = e.snapshot().clone();
        assert_eq!(
            e.apply(event(
                &e,
                EventKind::ReceiveSignal {
                    message: Box::new(m)
                },
                101
            ))
            .unwrap_err()
            .code,
            ErrorCode::InvalidSignal
        );
        assert_eq!(e.snapshot(), &before);
    }
    e.apply(event(&e, EventKind::Cancel, 101)).unwrap();
    let cancelled = message(&e, "cancelled", SignalDecision::Approve);
    receive(&mut e, cancelled, 102);
    assert_eq!(rejection(&e, "cancelled"), &SignalRejection::RunCancelled);
    assert_eq!(e.snapshot().status, RunStatus::Cancelled);
}

#[test]
fn early_approval_for_old_inputs_is_rejected_when_the_wait_activates_with_current_inputs() {
    let mut workflow = fixture("review");
    let mut prepare = workflow
        .nodes
        .iter()
        .find(|n| n.id == "implement")
        .unwrap()
        .clone();
    prepare.id = "prepare".into();
    workflow.nodes.push(prepare);
    workflow.entry = "prepare".into();
    workflow.edges.push(Edge {
        id: "prepared".into(),
        from: "prepare".into(),
        to: "review".into(),
        route: Route::Next,
    });
    let wait = workflow
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
            value: serde_json::json!("new-revision"),
        },
    );
    let (mut e, _) = start(vec![workflow]);
    let mut old = message(&e, "old-approval", SignalDecision::Approve);
    old.target.input_digest = workflow_worker::digest(&Values::from([(
        "revision".into(),
        serde_json::json!("old-revision"),
    )]))
    .unwrap();
    old.correlation_id = signal_correlation(&e.snapshot().run_digest, &old.target).unwrap();
    receive(&mut e, old, 101);
    assert_eq!(
        e.snapshot().inbox["old-approval"].status,
        SignalStatus::Pending
    );
    complete(&mut e, 1, "prepare", success());
    assert_eq!(
        rejection(&e, "old-approval"),
        &SignalRejection::InputMismatch
    );
    assert!(matches!(
        e.snapshot().frames[&1].nodes["review"].state,
        NodeState::Waiting { .. }
    ));
    assert_eq!(
        e.snapshot().frames[&1].nodes["implement"].state,
        NodeState::Pending
    );
    let mut current = message(&e, "current-approval", SignalDecision::Approve);
    current.target.input_digest =
        workflow_worker::digest(&e.snapshot().frames[&1].nodes["review"].inputs).unwrap();
    current.correlation_id = signal_correlation(&e.snapshot().run_digest, &current.target).unwrap();
    receive(&mut e, current, 102);
    assert!(is_applied(&e, "current-approval"));
}
