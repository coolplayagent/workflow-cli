use super::*;

fn source() -> Engine {
    let mut workflow = fixture("review");
    let field = Field {
        value_type: ValueType::String,
        required: true,
    };
    workflow.inputs.insert("subject".into(), field.clone());
    let wait = workflow
        .nodes
        .iter_mut()
        .find(|n| n.id == "review")
        .unwrap();
    wait.inputs.insert("subject".into(), field);
    wait.bindings.insert(
        "subject".into(),
        Binding::WorkflowInput {
            field: "subject".into(),
        },
    );
    let mut bundle = spec(vec![workflow]);
    bundle.wait_policies.push(WaitPolicyBinding {
        workflow: bundle.root.clone(),
        node_id: "review".into(),
        policy: WaitPolicy {
            identity: VersionRef {
                id: "migration-review".into(),
                version: "1".into(),
            },
            kind: WaitKind::HumanApproval,
            responders: ["reviewer".into()].into(),
            subjects: [("subject".into(), SubjectKind::Digest)].into(),
            max_validity_ms: 10000,
            exception: None,
        },
    });
    Engine::start(
        CompiledBundle::compile(bundle).unwrap(),
        "migration-run",
        [(
            "subject".into(),
            serde_json::json!(format!("sha256:{}", "a".repeat(64))),
        )]
        .into(),
        100,
        Limits::default(),
    )
    .unwrap()
    .0
}

fn request(engine: &Engine) -> MigrationRequest {
    let mut target = engine.bundle().spec().clone();
    target.root.version = "2.0.0".into();
    target.workflows[0].version = "2.0.0".into();
    target.workflows[0].nodes.retain(|n| n.id != "implement");
    target.workflows[0].edges.retain(|e| e.from != "implement");
    for edge in &mut target.workflows[0].edges {
        if edge.to == "implement" {
            edge.to = "done".into();
        }
    }
    let wait = target.workflows[0]
        .nodes
        .iter_mut()
        .find(|n| n.id == "review")
        .unwrap();
    wait.kind = NodeKind::Wait {
        event: "design-review".into(),
        timeout_ms: 20000,
    };
    target.capabilities.clear();
    target.wait_policies[0].workflow = target.root.clone();
    target.wait_policies[0].policy.identity.version = "2".into();
    MigrationRequest {
        migration_id: "migration-v2".into(),
        target_bundle: target,
        target_inputs: [(
            "subject".into(),
            serde_json::json!(format!("sha256:{}", "b".repeat(64))),
        )]
        .into(),
        execution_policy: MigrationExecutionPolicy::RestartWithFreshEvidence,
        timer_policy: MigrationTimerPolicy::CancelAndRearmOnResume,
        node_mapping: vec![],
        decision_summary: "Remove the implementation node; review the changed subject again".into(),
    }
}
fn message(engine: &Engine, name: &str) -> SignalMessage {
    let frame = &engine.snapshot().frames[&1];
    let node = &frame.nodes["review"];
    let target = WaitTarget {
        instance_id: node.instance_id,
        definition_digest: frame.definition_digest.clone(),
        input_digest: workflow_worker::digest(&node.inputs).unwrap(),
        event: "design-review".into(),
    };
    SignalMessage {
        schema_version: 1,
        message_id: name.into(),
        correlation_id: signal_correlation(&engine.snapshot().run_digest, &target).unwrap(),
        target,
        source: "reviewer".into(),
        exception: None,
        decision: SignalDecision::Approve,
        reason: "Reviewed this exact subject".into(),
        outputs: Values::new(),
        expires_at_unix_ms: 5000,
    }
}
fn pause(engine: &mut Engine) {
    engine
        .apply(event(
            engine,
            EventKind::Pause {
                reason: "review migration".into(),
            },
            102,
        ))
        .unwrap();
}

#[test]
fn migration_plan_maps_removals_inputs_approvals_and_timers_and_replays_exactly() {
    let mut engine = source();
    let request = request(&engine);
    assert!(engine.plan_migration(&request).is_err());
    let old_wait = message(&engine, "old-pending");
    pause(&mut engine);
    engine
        .apply(event(
            &engine,
            EventKind::ReceiveSignal {
                message: Box::new(old_wait),
            },
            103,
        ))
        .unwrap();
    assert_eq!(
        engine.snapshot().inbox["old-pending"].status,
        SignalStatus::Pending
    );
    let before = engine.snapshot().clone();
    let plan = engine.plan_migration(&request).unwrap();
    assert!(plan.inputs_changed);
    assert!(
        plan.nodes
            .iter()
            .any(|n| n.source.node_id == "implement" && n.target.is_none())
    );
    assert!(
        plan.nodes
            .iter()
            .any(|n| n.source.node_id == "review" && n.approval_policy_changed)
    );
    assert_eq!(
        plan.invalidated_instances.len(),
        before.frames[&1].nodes.len()
    );
    assert_eq!(plan.timers.len(), 1);
    assert_eq!(plan.timers[0].old_deadline_unix_ms, 86400100);
    assert_eq!(plan.timers[0].target_timeout_ms, Some(20000));
    assert_eq!(plan.invalidated_messages, ["old-pending"]);
    let mut tampered = plan.clone();
    tampered.timers.clear();
    assert!(
        engine
            .apply(event(
                &engine,
                EventKind::MigrateDefinition {
                    plan: Box::new(tampered)
                },
                110
            ))
            .is_err()
    );
    assert_eq!(engine.snapshot(), &before);
    let migration = event(
        &engine,
        EventKind::MigrateDefinition {
            plan: Box::new(plan.clone()),
        },
        110,
    );
    let transition = engine.apply(migration.clone()).unwrap();
    assert_eq!(
        transition.commands,
        [Command::CancelTimer {
            instance_id: before.frames[&1].nodes["review"].instance_id
        }]
    );
    assert!(engine.apply(migration).unwrap().duplicate);
    assert!(engine.snapshot().pause.is_some());
    assert_eq!(engine.snapshot().run_digest, before.run_digest);
    assert_eq!(engine.snapshot().bundle_digest, plan.target_bundle_digest);
    assert!(
        engine.snapshot().frames[&1]
            .nodes
            .values()
            .all(|n| n.instance_id >= before.next_instance_id)
    );
    assert!(matches!(
        engine.snapshot().inbox["old-pending"].status,
        SignalStatus::Rejected {
            reason: SignalRejection::DefinitionMismatch,
            ..
        }
    ));
    assert_eq!(
        engine.at_revision(before.revision).unwrap().snapshot(),
        &before
    );
    let restored = Engine::restore(
        engine.initial_bundle().clone(),
        engine.checkpoint().unwrap(),
    )
    .unwrap();
    assert_eq!(restored.snapshot(), engine.snapshot());
    engine
        .apply(event(
            &engine,
            EventKind::Resume {
                reason: "approved migration plan".into(),
            },
            500,
        ))
        .unwrap();
    assert_eq!(
        engine.snapshot().frames[&1].nodes["review"].state,
        NodeState::Waiting {
            deadline_unix_ms: 20500
        }
    );
    assert_eq!(engine.snapshot().status, RunStatus::Running);
    let fresh = message(&engine, "fresh-v2");
    engine
        .apply(event(
            &engine,
            EventKind::ReceiveSignal {
                message: Box::new(fresh),
            },
            501,
        ))
        .unwrap();
    assert_eq!(engine.snapshot().status, RunStatus::Succeeded);
    assert_eq!(
        Engine::restore(
            engine.initial_bundle().clone(),
            engine.checkpoint().unwrap()
        )
        .unwrap()
        .snapshot(),
        engine.snapshot()
    );
}

#[test]
fn prior_applied_approval_is_historical_and_cannot_authorize_the_new_definition() {
    let mut engine = source();
    let approved = message(&engine, "approved-v1");
    engine
        .apply(event(
            &engine,
            EventKind::ReceiveSignal {
                message: Box::new(approved.clone()),
            },
            101,
        ))
        .unwrap();
    assert!(matches!(
        engine.snapshot().inbox["approved-v1"].status,
        SignalStatus::Applied { .. }
    ));
    assert_eq!(
        engine.snapshot().frames[&1].nodes["implement"].state,
        NodeState::TaskReady
    );
    pause(&mut engine);
    let before = engine.snapshot().clone();
    let plan = engine.plan_migration(&request(&engine)).unwrap();
    assert!(
        plan.invalidated_instances
            .iter()
            .any(|n| n.node.node_id == "review" && n.state == "succeeded")
    );
    engine
        .apply(event(
            &engine,
            EventKind::MigrateDefinition {
                plan: Box::new(plan),
            },
            110,
        ))
        .unwrap();
    engine
        .apply(event(
            &engine,
            EventKind::Resume {
                reason: "begin new version".into(),
            },
            120,
        ))
        .unwrap();
    assert!(matches!(
        engine.snapshot().frames[&1].nodes["review"].state,
        NodeState::Waiting { .. }
    ));
    assert!(matches!(
        engine.snapshot().inbox["approved-v1"].status,
        SignalStatus::Applied { .. }
    ));
    let mut stale = approved;
    stale.message_id = "stale-redelivery".into();
    engine
        .apply(event(
            &engine,
            EventKind::ReceiveSignal {
                message: Box::new(stale),
            },
            121,
        ))
        .unwrap();
    assert!(matches!(
        engine.snapshot().inbox["stale-redelivery"].status,
        SignalStatus::Rejected {
            reason: SignalRejection::UnknownInstance,
            ..
        }
    ));
    assert_eq!(engine.snapshot().status, RunStatus::Running);
    assert_eq!(
        engine.at_revision(before.revision).unwrap().snapshot(),
        &before
    );
}
