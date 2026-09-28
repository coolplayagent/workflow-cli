use super::*;
use std::{collections::BTreeMap, path::PathBuf};
use workflow_ir::*;
use workflow_worker::{CapabilityDescriptor, EffectContract, FailureClass};
fn fixture(name: &str) -> Workflow {
    let base = if let Ok(dir) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(dir).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    parse(
        &std::fs::read_to_string(base.join(format!("examples/{name}.json"))).unwrap(),
        Format::Json,
        name,
    )
    .unwrap()
}
fn spec(workflows: Vec<Workflow>) -> BundleSpec {
    let root = VersionRef {
        id: workflows[0].id.clone(),
        version: workflows[0].version.clone(),
    };
    let mut capabilities = BTreeMap::new();
    for w in &workflows {
        for n in &w.nodes {
            if let NodeKind::Task { capability, .. } = &n.kind {
                capabilities
                    .entry(format!("{}@{}", capability.id, capability.version))
                    .or_insert(CapabilityDescriptor {
                        schema_version: 1,
                        capability: capability.clone(),
                        inputs: n.inputs.clone(),
                        outputs: n.outputs.clone(),
                        timeout_ms: 1000,
                        error_codes: BTreeMap::from([(
                            "test_failure".into(),
                            FailureClass::Permanent,
                        )]),
                        effects: EffectContract::ReadOnly,
                        usage:
                            "Deterministic test adapter contract; no task is invoked by the kernel"
                                .into(),
                        skill: None,
                    });
            }
        }
    }
    BundleSpec {
        postconditions: vec![],
        model_policies: vec![],
        schema_version: 1,
        root,
        workflows,
        capabilities: capabilities.into_values().collect(),
    }
}
fn start(workflows: Vec<Workflow>) -> (Engine, Transition) {
    Engine::start(
        CompiledBundle::compile(spec(workflows)).unwrap(),
        "test-run",
        Values::new(),
        100,
        Limits::default(),
    )
    .unwrap()
}
fn id(e: &Engine, frame: u64, node: &str) -> u64 {
    e.snapshot().frames[&frame].nodes[node].instance_id
}
fn event(e: &Engine, kind: EventKind, at: u64) -> Event {
    Event {
        event_id: format!("event-{}", e.snapshot().revision),
        run_id: e.snapshot().run_id.clone(),
        run_digest: e.snapshot().run_digest.clone(),
        expected_revision: e.snapshot().revision,
        at_unix_ms: at,
        kind,
    }
}
fn complete(e: &mut Engine, frame: u64, node: &str, result: TaskResult) -> Transition {
    let kind = EventKind::TaskCompleted {
        instance_id: id(e, frame, node),
        result,
    };
    e.apply(event(e, kind, e.snapshot().now_unix_ms)).unwrap()
}
fn success() -> TaskResult {
    TaskResult::Succeeded {
        outputs: Values::new(),
    }
}

#[test]
fn review_acceptance_and_rejection_follow_declared_routes() {
    for accepted in [true, false] {
        let (mut e, t) = start(vec![fixture("review")]);
        assert!(matches!(t.commands[0], Command::AwaitSignal { .. }));
        let signal = EventKind::Signal {
            instance_id: id(&e, 1, "review"),
            event: "design-review".into(),
            accepted,
            outputs: Values::new(),
        };
        let t = e.apply(event(&e, signal, 101)).unwrap();
        if accepted {
            assert!(
                t.commands
                    .iter()
                    .any(|c| matches!(c,Command::ExecuteTask{node_id,..} if node_id=="implement"))
            );
            complete(&mut e, 1, "implement", success());
            assert_eq!(e.snapshot().status, RunStatus::Succeeded);
        } else {
            assert!(
                !t.commands
                    .iter()
                    .any(|c| matches!(c, Command::ExecuteTask { .. }))
            );
            assert_eq!(e.snapshot().status, RunStatus::Cancelled);
        }
    }
}
#[test]
fn parallel_all_waits_and_never_counts_a_failure_as_success() {
    for failed in [false, true] {
        let (mut e, t) = start(vec![fixture("parallel-tests")]);
        assert_eq!(
            t.commands
                .iter()
                .filter(|c| matches!(c, Command::ExecuteTask { .. }))
                .count(),
            2
        );
        complete(&mut e, 1, "unit", success());
        assert_eq!(e.snapshot().status, RunStatus::Running);
        complete(
            &mut e,
            1,
            "integration",
            if failed {
                TaskResult::Failed {
                    code: "test_failure".into(),
                }
            } else {
                success()
            },
        );
        assert_eq!(
            e.snapshot().status,
            if failed {
                RunStatus::Failed
            } else {
                RunStatus::Succeeded
            }
        );
    }
}

fn any_workflow(cancel: bool) -> Workflow {
    let mut w = fixture("parallel-tests");
    w.nodes.iter_mut().find(|n| n.id == "join").unwrap().kind = NodeKind::Join {
        mode: JoinMode::Any,
        remaining: if cancel {
            RemainingPolicy::CancelAndReconcile
        } else {
            RemainingPolicy::Await
        },
    };
    w
}
fn tick(e: &mut Engine, at: u64) -> Transition {
    e.apply(event(e, EventKind::AdvanceTime, at)).unwrap()
}
#[test]
fn any_await_keeps_loser_results_and_orders_winners_by_accepted_events() {
    for first in ["unit", "integration"] {
        let (mut e, _) = start(vec![any_workflow(false)]);
        let loser = if first == "unit" {
            "integration"
        } else {
            "unit"
        };
        complete(&mut e, 1, first, success());
        assert_eq!(
            e.snapshot().frames[&1].nodes["join"].winner_edge,
            Some(format!("{first}-done"))
        );
        assert_eq!(
            e.snapshot().frames[&1].nodes["done"].state,
            NodeState::Succeeded
        );
        assert_eq!(e.snapshot().status, RunStatus::Running);
        complete(
            &mut e,
            1,
            loser,
            TaskResult::Failed {
                code: "test_failure".into(),
            },
        );
        assert_eq!(e.snapshot().status, RunStatus::Succeeded);
        assert_eq!(
            e.snapshot().frames[&1].nodes[loser].state,
            NodeState::Failed
        );
    }
}
#[test]
fn any_cancel_waits_for_cancellation_and_effect_reconciliation() {
    let (mut e, _) = start(vec![any_workflow(true)]);
    let t = complete(&mut e, 1, "unit", success());
    let loser = id(&e, 1, "integration");
    assert!(
        t.commands
            .contains(&Command::CancelTask { instance_id: loser })
    );
    assert_eq!(
        e.snapshot().frames[&1].nodes["integration"].state,
        NodeState::CancelRequested
    );
    let t = complete(
        &mut e,
        1,
        "integration",
        TaskResult::Uncertain {
            reason: "receipt lost".into(),
        },
    );
    assert_eq!(
        t.commands,
        vec![Command::ReconcileTask { instance_id: loser }]
    );
    assert_eq!(e.snapshot().status, RunStatus::Running);
    let kind = EventKind::TaskReconciled {
        instance_id: loser,
        result: success(),
    };
    e.apply(event(&e, kind, 101)).unwrap();
    assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    assert_eq!(
        e.snapshot().frames[&1].nodes["integration"].state,
        NodeState::Cancelled
    );
}
#[test]
fn any_without_a_success_propagates_failure_and_all_skipped_is_not_success() {
    let (mut e, _) = start(vec![any_workflow(false)]);
    complete(
        &mut e,
        1,
        "unit",
        TaskResult::Failed {
            code: "test_failure".into(),
        },
    );
    complete(&mut e, 1, "integration", TaskResult::Cancelled);
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    let mut w = fixture("parallel-tests");
    for n in w
        .nodes
        .iter_mut()
        .filter(|n| matches!(n.kind, NodeKind::Task { .. }))
    {
        n.inputs.insert(
            "enabled".into(),
            Field {
                value_type: ValueType::Boolean,
                required: true,
            },
        );
        n.bindings.insert(
            "enabled".into(),
            Binding::Literal {
                value: serde_json::json!(false),
            },
        );
        n.preconditions.push(Condition::Eq {
            field: "enabled".into(),
            value: serde_json::json!(true),
        });
    }
    let (e, t) = start(vec![w]);
    assert!(t.commands.is_empty());
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert_eq!(
        e.snapshot().frames[&1].nodes["join"].state,
        NodeState::Skipped
    );
}
#[test]
fn loop_retries_two_failed_rounds_then_succeeds_with_distinct_instances() {
    let mut outer = fixture("bounded-repair");
    if let NodeKind::Loop { max_iterations, .. } = &mut outer
        .nodes
        .iter_mut()
        .find(|n| n.id == "repair")
        .unwrap()
        .kind
    {
        *max_iterations = 3;
    }
    let bundle = CompiledBundle::compile(spec(vec![outer, fixture("repair-round")])).unwrap();
    let (mut e, _) =
        Engine::start(bundle.clone(), "run", Values::new(), 100, Limits::default()).unwrap();
    let mut instances = std::collections::BTreeSet::new();
    for frame in 2..=4 {
        assert!(instances.insert(id(&e, frame, "fix")));
        complete(&mut e, frame, "fix", success());
        let cp = e.checkpoint().unwrap();
        let restored = Engine::restore(bundle.clone(), cp).unwrap();
        assert_eq!(restored.snapshot(), e.snapshot());
        e = restored;
        complete(
            &mut e,
            frame,
            "test",
            TaskResult::Succeeded {
                outputs: Values::from([("passed".into(), serde_json::json!(frame == 4))]),
            },
        );
    }
    assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    assert_eq!(e.snapshot().frames.len(), 4);
    assert_eq!(e.snapshot().frames[&2].status, FrameStatus::Failed);
    assert_eq!(e.snapshot().frames[&3].status, FrameStatus::Failed);
    assert_eq!(e.snapshot().frames[&4].status, FrameStatus::Succeeded);
}
#[test]
fn loop_exhaustion_and_deadline_do_not_release_unsettled_child_effects() {
    let (mut e, _) = start(vec![fixture("bounded-repair"), fixture("repair-round")]);
    for frame in 2..=3 {
        complete(&mut e, frame, "fix", success());
        complete(
            &mut e,
            frame,
            "test",
            TaskResult::Succeeded {
                outputs: Values::from([("passed".into(), serde_json::json!(false))]),
            },
        );
    }
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert_eq!(e.snapshot().frames.len(), 3);
    let (mut e, _) = start(vec![fixture("bounded-repair"), fixture("repair-round")]);
    let instance = id(&e, 2, "fix");
    let t = tick(&mut e, 600100);
    assert!(t.commands.contains(&Command::CancelTask {
        instance_id: instance
    }));
    assert_eq!(e.snapshot().status, RunStatus::Running);
    complete(
        &mut e,
        2,
        "fix",
        TaskResult::Uncertain {
            reason: "unknown write receipt".into(),
        },
    );
    assert_eq!(e.snapshot().status, RunStatus::Running);
    let kind = EventKind::TaskReconciled {
        instance_id: instance,
        result: TaskResult::Cancelled,
    };
    e.apply(event(&e, kind, 600101)).unwrap();
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert_eq!(e.snapshot().frames.len(), 2);
    assert_eq!(
        e.snapshot().frames[&1].nodes["repair"].reason.as_deref(),
        Some("loop_exhausted")
    );
}
#[test]
fn wait_deadlines_survive_replay_and_equal_deadline_signals_cannot_win() {
    let bundle = CompiledBundle::compile(spec(vec![fixture("review")])).unwrap();
    let (mut e, _) =
        Engine::start(bundle.clone(), "run", Values::new(), 100, Limits::default()).unwrap();
    let old = e.snapshot().clone();
    let kind = EventKind::Signal {
        instance_id: id(&e, 1, "review"),
        event: "design-review".into(),
        accepted: true,
        outputs: Values::new(),
    };
    assert_eq!(
        e.apply(event(&e, kind, 86400100)).unwrap_err().code,
        ErrorCode::InvalidSignal
    );
    assert_eq!(e.snapshot(), &old);
    e = Engine::restore(bundle, e.checkpoint().unwrap()).unwrap();
    tick(&mut e, 86400100);
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert_eq!(
        e.snapshot().frames[&1].nodes["review"].reason.as_deref(),
        Some("timed_out")
    );
}
#[test]
fn events_have_atomic_cas_exact_deduplication_and_terminal_absorption() {
    let (mut e, _) = start(vec![fixture("parallel-tests")]);
    let first = event(
        &e,
        EventKind::TaskCompleted {
            instance_id: id(&e, 1, "unit"),
            result: success(),
        },
        100,
    );
    e.apply(first.clone()).unwrap();
    let snapshot = e.snapshot().clone();
    let retry = e.apply(first.clone()).unwrap();
    assert!(retry.duplicate && retry.commands.is_empty());
    assert_eq!(e.snapshot(), &snapshot);
    let mut changed = first.clone();
    changed.at_unix_ms += 1;
    assert_eq!(e.apply(changed).unwrap_err().code, ErrorCode::EventConflict);
    let mut stale = first.clone();
    stale.event_id = "different".into();
    assert_eq!(
        e.apply(stale).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    let bad = event(
        &e,
        EventKind::TaskCompleted {
            instance_id: id(&e, 1, "integration"),
            result: TaskResult::Succeeded {
                outputs: Values::from([("undeclared".into(), serde_json::json!(true))]),
            },
        },
        100,
    );
    assert_eq!(e.apply(bad).unwrap_err().code, ErrorCode::InvalidTaskResult);
    assert_eq!(e.snapshot(), &snapshot);
    complete(&mut e, 1, "integration", success());
    assert!(e.apply(first).unwrap().duplicate);
    assert_eq!(
        e.apply(event(&e, EventKind::AdvanceTime, 101))
            .unwrap_err()
            .code,
        ErrorCode::TerminalRun
    );
}
#[test]
fn cancellation_waits_for_issued_tasks_and_remains_possible_after_event_budget() {
    let b = CompiledBundle::compile(spec(vec![fixture("parallel-tests")])).unwrap();
    let limits = Limits {
        max_events: 1,
        ..Limits::default()
    };
    let (mut e, _) = Engine::start(b.clone(), "run", Values::new(), 100, limits).unwrap();
    complete(&mut e, 1, "unit", success());
    let before = e.snapshot().clone();
    let denied = event(
        &e,
        EventKind::TaskCompleted {
            instance_id: id(&e, 1, "integration"),
            result: success(),
        },
        100,
    );
    assert_eq!(e.apply(denied).unwrap_err().code, ErrorCode::BudgetExceeded);
    assert_eq!(e.snapshot(), &before);
    let cancel = event(&e, EventKind::Cancel, 101);
    let t = e.apply(cancel).unwrap();
    assert_eq!(e.snapshot().status, RunStatus::Cancelling);
    assert_eq!(
        t.commands
            .iter()
            .filter(|c| matches!(c, Command::CancelTask { .. }))
            .count(),
        1
    );
    complete(&mut e, 1, "integration", success());
    assert_eq!(e.snapshot().status, RunStatus::Cancelled);
    assert_eq!(
        Engine::restore(b, e.checkpoint().unwrap())
            .unwrap()
            .snapshot(),
        e.snapshot()
    );
}
#[test]
fn bundle_resolution_rejects_missing_contracts_cycles_and_policy_bypass() {
    let original = spec(vec![fixture("bounded-repair"), fixture("repair-round")]);
    let first = CompiledBundle::compile(original.clone()).unwrap();
    let mut reordered = original.clone();
    reordered.workflows.reverse();
    reordered.capabilities.reverse();
    for w in &mut reordered.workflows {
        w.nodes.reverse();
    }
    assert_eq!(
        first.digest(),
        CompiledBundle::compile(reordered).unwrap().digest()
    );
    let mut missing = original.clone();
    missing.capabilities.clear();
    assert_eq!(
        CompiledBundle::compile(missing).unwrap_err().code,
        ErrorCode::MissingReference
    );
    let mut recursive = original.clone();
    recursive.workflows[1]
        .nodes
        .iter_mut()
        .find(|n| n.id == "fix")
        .unwrap()
        .kind = NodeKind::Subworkflow {
        workflow: recursive.root.clone(),
    };
    assert_eq!(
        CompiledBundle::compile(recursive).unwrap_err().code,
        ErrorCode::RecursiveBundle
    );
    let mut contract = original.clone();
    contract.capabilities[0].inputs.insert(
        "extra".into(),
        Field {
            value_type: ValueType::String,
            required: true,
        },
    );
    assert_eq!(
        CompiledBundle::compile(contract).unwrap_err().code,
        ErrorCode::ContractMismatch
    );
    let mut policy = original;
    if let NodeKind::Task { policy, .. } = &mut policy.workflows[1]
        .nodes
        .iter_mut()
        .find(|n| n.id == "fix")
        .unwrap()
        .kind
    {
        *policy = Some(VersionRef {
            id: "model".into(),
            version: "1".into(),
        });
    }
    assert_eq!(
        CompiledBundle::compile(policy).unwrap_err().code,
        ErrorCode::UnsupportedPolicy
    );
}
#[test]
fn corrupted_checkpoints_and_changed_bundles_do_not_restore() {
    let b = CompiledBundle::compile(spec(vec![fixture("parallel-tests")])).unwrap();
    let (mut e, _) =
        Engine::start(b.clone(), "run", Values::new(), 100, Limits::default()).unwrap();
    complete(&mut e, 1, "unit", success());
    let cp = e.checkpoint().unwrap();
    let mut bad = cp.clone();
    bad.state_digest = workflow_worker::digest(&"corrupt").unwrap();
    assert_eq!(
        Engine::restore(b.clone(), bad).unwrap_err().code,
        ErrorCode::CorruptCheckpoint
    );
    let mut bad = cp.clone();
    bad.events[0].at_unix_ms += 1;
    assert_eq!(
        Engine::restore(b.clone(), bad).unwrap_err().code,
        ErrorCode::CorruptCheckpoint
    );
    let mut changed = b.spec().clone();
    changed.capabilities[0].timeout_ms += 1;
    assert_eq!(
        Engine::restore(CompiledBundle::compile(changed).unwrap(), cp)
            .unwrap_err()
            .code,
        ErrorCode::CorruptCheckpoint
    );
}
#[test]
fn any_cannot_fabricate_outputs_from_a_branch_that_has_not_finished() {
    let mut w = any_workflow(false);
    w.nodes
        .iter_mut()
        .find(|n| n.id == "integration")
        .unwrap()
        .outputs
        .insert(
            "report".into(),
            Field {
                value_type: ValueType::String,
                required: true,
            },
        );
    let terminal = w.nodes.iter_mut().find(|n| n.id == "done").unwrap();
    terminal.inputs.insert(
        "report".into(),
        Field {
            value_type: ValueType::String,
            required: true,
        },
    );
    terminal.bindings.insert(
        "report".into(),
        Binding::NodeOutput {
            node: "integration".into(),
            field: "report".into(),
        },
    );
    let (mut e, _) = start(vec![w]);
    complete(&mut e, 1, "unit", success());
    assert_eq!(
        e.snapshot().frames[&1].nodes["done"].state,
        NodeState::Failed
    );
    complete(
        &mut e,
        1,
        "integration",
        TaskResult::Succeeded {
            outputs: Values::from([("report".into(), serde_json::json!("late"))]),
        },
    );
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert!(e.snapshot().frames[&1].outputs.is_empty());
}
#[test]
fn unsafe_any_cancellation_regions_are_rejected_before_dispatch() {
    let mut w = any_workflow(true);
    let branch = w.nodes.iter_mut().find(|n| n.id == "integration").unwrap();
    branch.kind = NodeKind::Decision {
        mode: DecisionMode::Exclusive,
    };
    branch.inputs.insert(
        "continue".into(),
        Field {
            value_type: ValueType::Boolean,
            required: true,
        },
    );
    branch.bindings.insert(
        "continue".into(),
        Binding::Literal {
            value: serde_json::json!(true),
        },
    );
    w.edges
        .iter_mut()
        .find(|e| e.id == "integration-done")
        .unwrap()
        .route = Route::Case {
        when: Condition::Eq {
            field: "continue".into(),
            value: serde_json::json!(true),
        },
    };
    w.edges.push(Edge {
        id: "escape".into(),
        from: "integration".into(),
        to: "escape".into(),
        route: Route::Otherwise,
    });
    w.nodes.push(Node {
        id: "escape".into(),
        kind: NodeKind::Terminal {
            outcome: TerminalOutcome::Failed,
        },
        inputs: Contract::new(),
        outputs: Contract::new(),
        bindings: BTreeMap::new(),
        preconditions: vec![],
    });
    assert!(workflow_validator::validate(&w, "test").is_empty());
    assert_eq!(
        CompiledBundle::compile(spec(vec![w])).unwrap_err().code,
        ErrorCode::UnsafeCancellation
    );
}

#[test]
fn subworkflows_receive_typed_inputs_and_return_terminal_values() {
    let integer = Field {
        value_type: ValueType::Integer,
        required: true,
    };
    let input = Contract::from([("number".into(), integer.clone())]);
    let output = Contract::from([("answer".into(), integer)]);
    let child = WorkflowBuilder::new("child", "1", "compute")
        .input("number", input["number"].clone())
        .node(Node {
            id: "compute".into(),
            kind: NodeKind::Task {
                capability: VersionRef {
                    id: "compute".into(),
                    version: "1".into(),
                },
                policy: None,
            },
            inputs: input.clone(),
            outputs: output.clone(),
            bindings: BTreeMap::from([(
                "number".into(),
                Binding::WorkflowInput {
                    field: "number".into(),
                },
            )]),
            preconditions: vec![],
        })
        .node(Node {
            id: "return".into(),
            kind: NodeKind::Terminal {
                outcome: TerminalOutcome::Succeeded,
            },
            inputs: output.clone(),
            outputs: Contract::new(),
            bindings: BTreeMap::from([(
                "answer".into(),
                Binding::NodeOutput {
                    node: "compute".into(),
                    field: "answer".into(),
                },
            )]),
            preconditions: vec![],
        })
        .edge(Edge {
            id: "done".into(),
            from: "compute".into(),
            to: "return".into(),
            route: Route::Next,
        })
        .build();
    let parent = WorkflowBuilder::new("parent", "1", "child")
        .input("number", input["number"].clone())
        .node(Node {
            id: "child".into(),
            kind: NodeKind::Subworkflow {
                workflow: VersionRef {
                    id: "child".into(),
                    version: "1".into(),
                },
            },
            inputs: input,
            outputs: output.clone(),
            bindings: BTreeMap::from([(
                "number".into(),
                Binding::WorkflowInput {
                    field: "number".into(),
                },
            )]),
            preconditions: vec![],
        })
        .node(Node {
            id: "done".into(),
            kind: NodeKind::Terminal {
                outcome: TerminalOutcome::Succeeded,
            },
            inputs: output,
            outputs: Contract::new(),
            bindings: BTreeMap::from([(
                "answer".into(),
                Binding::NodeOutput {
                    node: "child".into(),
                    field: "answer".into(),
                },
            )]),
            preconditions: vec![],
        })
        .edge(Edge {
            id: "return".into(),
            from: "child".into(),
            to: "done".into(),
            route: Route::Next,
        })
        .build();
    let b = CompiledBundle::compile(spec(vec![parent, child])).unwrap();
    let inputs = Values::from([("number".into(), serde_json::json!(41))]);
    let (mut e, t) = Engine::start(b, "run", inputs.clone(), 100, Limits::default()).unwrap();
    assert!(
        t.commands.iter().any(
            |c| matches!(c,Command::ExecuteTask{inputs:actual,frame_id:2,..} if actual==&inputs)
        )
    );
    let outputs = Values::from([("answer".into(), serde_json::json!(42))]);
    complete(
        &mut e,
        2,
        "compute",
        TaskResult::Succeeded {
            outputs: outputs.clone(),
        },
    );
    assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    assert_eq!(e.snapshot().frames[&1].outputs, outputs);
}
#[test]
fn event_scope_is_bound_to_run_start_inputs_limits_and_identity() {
    let b = CompiledBundle::compile(spec(vec![fixture("parallel-tests")])).unwrap();
    let (mut first, _) =
        Engine::start(b.clone(), "same-id", Values::new(), 100, Limits::default()).unwrap();
    let (second, _) = Engine::start(b, "same-id", Values::new(), 101, Limits::default()).unwrap();
    let foreign = event(
        &second,
        EventKind::TaskCompleted {
            instance_id: id(&second, 1, "unit"),
            result: success(),
        },
        101,
    );
    assert_eq!(
        first.apply(foreign).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(first.snapshot().revision, 1);
    let mut cp = first.checkpoint().unwrap();
    cp.limits.max_events -= 1;
    assert_eq!(
        Engine::restore(first.bundle().clone(), cp)
            .unwrap_err()
            .code,
        ErrorCode::CorruptCheckpoint
    );
}
#[test]
fn transition_budgets_leave_the_original_state_intact_and_allow_cancellation() {
    let b = CompiledBundle::compile(spec(vec![fixture("parallel-tests")])).unwrap();
    let (mut e, _) = Engine::start(
        b,
        "run",
        Values::new(),
        100,
        Limits {
            max_transitions: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    complete(&mut e, 1, "unit", success());
    let before = e.snapshot().clone();
    let update = event(
        &e,
        EventKind::TaskCompleted {
            instance_id: id(&e, 1, "integration"),
            result: success(),
        },
        100,
    );
    assert_eq!(e.apply(update).unwrap_err().code, ErrorCode::BudgetExceeded);
    assert_eq!(e.snapshot(), &before);
    e.apply(event(&e, EventKind::Cancel, 100)).unwrap();
    complete(&mut e, 1, "integration", TaskResult::Cancelled);
    assert_eq!(e.snapshot().status, RunStatus::Cancelled);
    let b = CompiledBundle::compile(spec(vec![
        fixture("bounded-repair"),
        fixture("repair-round"),
    ]))
    .unwrap();
    assert_eq!(
        Engine::start(
            b,
            "run",
            Values::new(),
            100,
            Limits {
                max_frames: 1,
                ..Limits::default()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::BudgetExceeded
    );
}
#[test]
fn a_task_result_at_the_loop_deadline_cannot_complete_the_loop_successfully() {
    let (mut e, _) = start(vec![fixture("bounded-repair"), fixture("repair-round")]);
    complete(&mut e, 2, "fix", success());
    let update = event(
        &e,
        EventKind::TaskCompleted {
            instance_id: id(&e, 2, "test"),
            result: TaskResult::Succeeded {
                outputs: Values::from([("passed".into(), serde_json::json!(true))]),
            },
        },
        600100,
    );
    e.apply(update).unwrap();
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert_eq!(e.snapshot().frames.len(), 2);
    assert_eq!(
        e.snapshot().frames[&1].nodes["repair"].reason.as_deref(),
        Some("loop_exhausted")
    );
}

#[test]
fn decision_errors_fail_the_node_and_first_match_keeps_edge_order() {
    for mode in [DecisionMode::Exclusive, DecisionMode::FirstMatch] {
        let mut w = fixture("repair-round");
        w.nodes.iter_mut().find(|n| n.id == "choose").unwrap().kind =
            NodeKind::Decision { mode: mode.clone() };
        w.edges.insert(
            2,
            Edge {
                id: "earlier-case".into(),
                from: "choose".into(),
                to: "retry".into(),
                route: Route::Case {
                    when: Condition::Eq {
                        field: "passed".into(),
                        value: serde_json::json!(true),
                    },
                },
            },
        );
        let (mut e, _) = start(vec![w]);
        complete(&mut e, 1, "fix", success());
        complete(
            &mut e,
            1,
            "test",
            TaskResult::Succeeded {
                outputs: Values::from([("passed".into(), serde_json::json!(true))]),
            },
        );
        assert_eq!(e.snapshot().status, RunStatus::Failed);
        let node = &e.snapshot().frames[&1].nodes["choose"];
        if mode == DecisionMode::Exclusive {
            assert_eq!(node.state, NodeState::Failed);
        } else {
            assert_eq!(node.state, NodeState::Succeeded);
            assert_eq!(
                e.snapshot().frames[&1].edges["earlier-case"].status,
                TokenStatus::Selected
            );
            assert_eq!(
                e.snapshot().frames[&1].edges["passed"].status,
                TokenStatus::Skipped
            );
        }
    }
    let mut w = fixture("repair-round");
    w.nodes
        .iter_mut()
        .find(|n| n.id == "test")
        .unwrap()
        .outputs
        .get_mut("passed")
        .unwrap()
        .required = false;
    w.nodes
        .iter_mut()
        .find(|n| n.id == "choose")
        .unwrap()
        .inputs
        .get_mut("passed")
        .unwrap()
        .required = false;
    let (mut e, _) = start(vec![w]);
    complete(&mut e, 1, "fix", success());
    complete(&mut e, 1, "test", success());
    assert_eq!(
        e.snapshot().frames[&1].nodes["choose"].state,
        NodeState::Failed
    );
    assert_eq!(e.snapshot().status, RunStatus::Failed);
}

#[test]
fn an_all_join_treats_a_skipped_branch_as_neutral_when_another_succeeds() {
    let mut w = fixture("parallel-tests");
    w.nodes
        .iter_mut()
        .find(|n| n.id == "unit")
        .unwrap()
        .preconditions
        .push(Condition::Exists {
            field: "optional".into(),
        });
    w.nodes
        .iter_mut()
        .find(|n| n.id == "unit")
        .unwrap()
        .inputs
        .insert(
            "optional".into(),
            Field {
                value_type: ValueType::String,
                required: false,
            },
        );
    let (mut e, _) = start(vec![w]);
    assert_eq!(
        e.snapshot().frames[&1].nodes["unit"].state,
        NodeState::Skipped
    );
    complete(&mut e, 1, "integration", success());
    assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    assert_eq!(
        e.snapshot().frames[&1].nodes["unit"].state,
        NodeState::Skipped
    );
}

#[test]
fn unresolved_effect_query_contracts_reject_the_bundle() {
    let mut b = spec(vec![fixture("parallel-tests")]);
    b.capabilities[0].effects = EffectContract::Write {
        idempotency: workflow_worker::Idempotency::None,
        query: Some(VersionRef {
            id: "missing-query".into(),
            version: "1".into(),
        }),
        compensation: None,
    };
    assert_eq!(
        CompiledBundle::compile(b).unwrap_err().code,
        ErrorCode::MissingReference
    );
}

#[test]
fn declared_unknown_effect_errors_require_reconciliation_before_loop_retry() {
    let mut b = spec(vec![fixture("bounded-repair"), fixture("repair-round")]);
    b.capabilities
        .iter_mut()
        .find(|c| c.capability.id == "coding.fix")
        .unwrap()
        .error_codes
        .insert("receipt_lost".into(), FailureClass::UnknownEffect);
    b.capabilities
        .iter_mut()
        .find(|c| c.capability.id == "coding.fix")
        .unwrap()
        .effects = EffectContract::Write {
        idempotency: workflow_worker::Idempotency::None,
        query: None,
        compensation: None,
    };
    let (mut e, _) = Engine::start(
        CompiledBundle::compile(b).unwrap(),
        "run",
        Values::new(),
        100,
        Limits::default(),
    )
    .unwrap();
    let instance = id(&e, 2, "fix");
    let uncertain = TaskResult::Failed {
        code: "receipt_lost".into(),
    };
    let t = complete(&mut e, 2, "fix", uncertain.clone());
    assert!(t.commands.contains(&Command::ReconcileTask {
        instance_id: instance
    }));
    assert_eq!(
        e.snapshot().frames[&2].nodes["fix"].state,
        NodeState::Reconciling
    );
    assert_eq!(e.snapshot().frames.len(), 2);
    let before = e.snapshot().clone();
    let still_unknown = event(
        &e,
        EventKind::TaskReconciled {
            instance_id: instance,
            result: uncertain,
        },
        101,
    );
    assert_eq!(
        e.apply(still_unknown).unwrap_err().code,
        ErrorCode::InvalidTaskResult
    );
    assert_eq!(e.snapshot(), &before);
    let settled = event(
        &e,
        EventKind::TaskReconciled {
            instance_id: instance,
            result: TaskResult::Failed {
                code: "test_failure".into(),
            },
        },
        101,
    );
    e.apply(settled).unwrap();
    assert_eq!(e.snapshot().frames.len(), 3);
    assert_eq!(
        e.snapshot().frames[&3].nodes["fix"].state,
        NodeState::TaskReady
    );
}

mod lifecycle;
mod postconditions;
