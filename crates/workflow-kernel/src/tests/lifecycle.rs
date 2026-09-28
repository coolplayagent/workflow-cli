use super::*;

fn pause(e: &mut Engine, at: u64) -> Event {
    let event = event(
        e,
        EventKind::Pause {
            reason: "maintenance".into(),
        },
        at,
    );
    assert!(e.apply(event.clone()).unwrap().commands.is_empty());
    event
}
fn resume(e: &mut Engine, at: u64) -> Transition {
    e.apply(event(
        e,
        EventKind::Resume {
            reason: "maintenance finished".into(),
        },
        at,
    ))
    .unwrap()
}

#[test]
fn pause_drains_results_without_activating_successors_and_replays_exactly() {
    let (mut e, _) = start(vec![fixture("parallel-tests")]);
    let original = pause(&mut e, 101);
    complete(&mut e, 1, "unit", success());
    complete(&mut e, 1, "integration", success());
    assert_eq!(e.snapshot().status, RunStatus::Running);
    assert!(e.snapshot().pause.is_some());
    assert!(
        e.snapshot().frames[&1]
            .nodes
            .values()
            .any(|n| n.state == NodeState::Pending)
    );
    let restored = Engine::restore(e.bundle().clone(), e.checkpoint().unwrap()).unwrap();
    assert_eq!(restored.snapshot(), e.snapshot());
    assert!(e.apply(original.clone()).unwrap().duplicate);
    let mut conflict = original;
    conflict.kind = EventKind::Pause {
        reason: "different reason".into(),
    };
    assert_eq!(
        e.apply(conflict).unwrap_err().code,
        ErrorCode::EventConflict
    );
    resume(&mut e, 102);
    assert_eq!(e.snapshot().status, RunStatus::Succeeded);
    assert!(e.snapshot().pause.is_none());
}

#[test]
fn pause_rejects_signals_and_time_advancement_and_resume_expires_original_deadline() {
    let (mut e, _) = start(vec![fixture("review")]);
    pause(&mut e, 101);
    let before = e.snapshot().clone();
    let kinds = [
        EventKind::AdvanceTime,
        EventKind::Signal {
            instance_id: id(&e, 1, "review"),
            event: "design-review".into(),
            accepted: true,
            outputs: Values::new(),
        },
    ];
    for kind in kinds {
        assert_eq!(
            e.apply(event(&e, kind, 86_400_100)).unwrap_err().code,
            ErrorCode::RunPaused
        );
        assert_eq!(e.snapshot(), &before);
    }
    let mut e = Engine::restore(e.bundle().clone(), e.checkpoint().unwrap()).unwrap();
    let t = resume(&mut e, 86_400_100);
    assert_eq!(e.snapshot().status, RunStatus::Failed);
    assert!(
        !t.commands
            .iter()
            .any(|c| matches!(c, Command::ExecuteTask { .. }))
    );
}

#[test]
fn controls_require_a_state_change_reason_revision_and_active_run() {
    let (mut e, _) = start(vec![fixture("review")]);
    for kind in [
        EventKind::Resume {
            reason: "not paused".into(),
        },
        EventKind::Pause { reason: " ".into() },
        EventKind::Pause {
            reason: "x".repeat(1025),
        },
    ] {
        assert_eq!(
            e.apply(event(&e, kind, 101)).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }
    let mut stale = event(&e, EventKind::Cancel, 101);
    stale.event_id = "stale-cancel".into();
    pause(&mut e, 101);
    assert_eq!(
        e.apply(stale).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    assert_eq!(
        e.apply(event(
            &e,
            EventKind::Pause {
                reason: "again".into()
            },
            101
        ))
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );
    e.apply(event(&e, EventKind::Cancel, 102)).unwrap();
    assert!(e.snapshot().pause.is_none());
    assert_eq!(e.snapshot().status, RunStatus::Cancelled);
    assert_eq!(
        e.apply(event(
            &e,
            EventKind::Resume {
                reason: "late".into()
            },
            103
        ))
        .unwrap_err()
        .code,
        ErrorCode::TerminalRun
    );
}
