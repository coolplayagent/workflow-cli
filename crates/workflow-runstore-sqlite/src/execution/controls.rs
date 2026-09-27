use super::*;
use workflow_kernel::{EventKind, NodeState, RunStatus, TaskResult};
fn node(r: &Recovered, instance: u64) -> Result<&workflow_kernel::NodeInstance> {
    r.engine
        .snapshot()
        .frames
        .values()
        .flat_map(|f| f.nodes.values())
        .find(|n| n.instance_id == instance)
        .ok_or_else(|| corrupt("unknown command instance"))
}
pub(super) fn read_only_instance(r: &Recovered, instance: u64) -> Result<()> {
    let e = r
        .outbox
        .iter()
        .find(|e| matches!(e.command,Command::ExecuteTask{instance_id,..} if instance_id==instance))
        .ok_or_else(|| corrupt("missing original task intent"))?;
    proof::capability(r, e)?;
    Ok(())
}
impl SqliteRunStore {
    pub(crate) fn advance_due(
        &mut self,
        l: &Lease,
        clock: &dyn Clock,
    ) -> Result<Option<Committed>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        let active = matches!(
            r.engine.snapshot().status,
            RunStatus::Running | RunStatus::Cancelling
        );
        let due = r
            .engine
            .snapshot()
            .frames
            .values()
            .flat_map(|f| f.nodes.values())
            .any(|n| match n.state {
                NodeState::Waiting { deadline_unix_ms } => deadline_unix_ms <= now,
                NodeState::Child {
                    deadline_unix_ms: Some(d),
                    exhausting: false,
                    ..
                } => d <= now,
                _ => false,
            });
        if !active || !due {
            tx.commit().map_err(storage)?;
            return Ok(None);
        }
        let e = Event {
            event_id: format!("timer-{}-{}", l.epoch, r.engine.snapshot().revision),
            run_id: l.run_id.clone(),
            run_digest: r.engine.snapshot().run_digest.clone(),
            expected_revision: r.engine.snapshot().revision,
            at_unix_ms: now,
            kind: EventKind::AdvanceTime,
        };
        let committed = crate::writes::persist_event(&tx, &mut r, &e, |_| {})?;
        append(
            &tx,
            &mut a,
            ExecutionAction::TimerAdvanced {
                epoch: l.epoch,
                event_id: e.event_id,
                event_revision: committed.snapshot.revision,
                at_unix_ms: now,
            },
        )?;
        commit_guard(clock, l, now, l.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(Some(committed))
    }
    pub(crate) fn claim_owned(&mut self, l: &Lease, clock: &dyn Clock) -> Result<Claimed> {
        self.claim_internal(l, clock, |_| {})
    }
    pub(crate) fn claim_internal(
        &mut self,
        l: &Lease,
        clock: &dyn Clock,
        hook: impl Fn(&str),
    ) -> Result<Claimed> {
        hook("before_transaction");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        let Some(entry) = r.outbox.iter().find(|e| e.receipt.is_none()).cloned() else {
            tx.commit().map_err(storage)?;
            return Ok(Claimed::Idle);
        };
        let mut cancel_instance = None;
        match &entry.command {
            Command::CheckGate {
                instance_id,
                context,
            } => match &node(&r, *instance_id)?.state {
                NodeState::CheckingGate {
                    context: current,
                    awaiting: true,
                } if current == context => {
                    hook("before_gate_evaluation");
                    let evaluation = gates::evaluate(
                        &r,
                        &a,
                        context,
                        self.artifacts.as_deref(),
                        now,
                        r.engine.snapshot().revision,
                    )?;
                    let deadline = evaluation
                        .decision
                        .expires_at_unix_ms
                        .unwrap_or(l.expires_at_unix_ms)
                        .min(l.expires_at_unix_ms);
                    let event = Event {
                        event_id: format!("gate-{}-{}", l.epoch, entry.sequence),
                        run_id: l.run_id.clone(),
                        run_digest: r.engine.snapshot().run_digest.clone(),
                        expected_revision: r.engine.snapshot().revision,
                        at_unix_ms: now,
                        kind: EventKind::GateEvaluated {
                            instance_id: *instance_id,
                            context_digest: digest(context)?,
                            evaluation: Box::new(evaluation),
                        },
                    };
                    let committed = crate::writes::persist_event(&tx, &mut r, &event, &hook)?;
                    crate::writes::persist_receipt(&tx, &r, &receipt(l, &entry))?;
                    append(
                        &tx,
                        &mut a,
                        ExecutionAction::GateChecked {
                            epoch: l.epoch,
                            command_id: entry.command_id.clone(),
                            event_id: event.event_id,
                            event_revision: committed.snapshot.revision,
                            at_unix_ms: now,
                        },
                    )?;
                    hook("gate_written");
                    commit_guard(clock, l, now, deadline)?;
                    hook("before_commit");
                    tx.commit().map_err(storage)?;
                    hook("after_commit");
                    return Ok(Claimed::Handled {
                        command_id: entry.command_id,
                    });
                }
                state if state.terminal() => {}
                _ => {
                    return Err(Error::new(
                        ErrorCode::TransitionRejected,
                        "gate command no longer matches a pending observation",
                    ));
                }
            },
            Command::ExecuteTask { instance_id, .. } => {
                proof::capability(&r, &entry)?;
                if a.attempts.values().any(|t| {
                    t.prepared.command_id == entry.command_id
                        && t.prepared.epoch == l.epoch
                        && t.outcome.is_none()
                }) {
                    return Err(Error::new(
                        ErrorCode::AttemptInProgress,
                        "current owner already prepared this task",
                    ));
                }
                match node(&r, *instance_id)?.state {
                    NodeState::TaskReady => {
                        let attempt = proof::prepare(
                            &r,
                            &entry,
                            l,
                            a.attempt_number(&entry.command_id)?,
                            now,
                        )?;
                        append(
                            &tx,
                            &mut a,
                            ExecutionAction::Prepared {
                                attempt: Box::new(attempt.clone()),
                            },
                        )?;
                        commit_guard(clock, l, now, attempt.request.deadline_unix_ms)?;
                        tx.commit().map_err(storage)?;
                        return Ok(Claimed::Task {
                            attempt: Box::new(attempt),
                        });
                    }
                    NodeState::CancelRequested => cancel_instance = Some(*instance_id),
                    NodeState::Reconciling => {
                        return Err(Error::new(
                            ErrorCode::ManualReconciliation,
                            "uncertain task requires a verified reconciliation result",
                        ));
                    }
                    ref state if state.terminal() => {}
                    _ => return Err(corrupt("execute intent has an ineligible node state")),
                }
            }
            Command::CancelTask { instance_id } => {
                read_only_instance(&r, *instance_id)?;
                match node(&r, *instance_id)?.state {
                    NodeState::CancelRequested => cancel_instance = Some(*instance_id),
                    NodeState::Reconciling => {
                        return Err(Error::new(
                            ErrorCode::ManualReconciliation,
                            "cancellation still needs effect reconciliation",
                        ));
                    }
                    ref state if state.terminal() => {}
                    _ => {
                        return Err(Error::new(
                            ErrorCode::AttemptInProgress,
                            "task cancellation has not reached a safe settlement point",
                        ));
                    }
                }
            }
            Command::ReconcileTask { .. } => {
                return Err(Error::new(
                    ErrorCode::ManualReconciliation,
                    "reconciliation command needs a verified host result",
                ));
            }
            Command::AwaitSignal { .. }
            | Command::ScheduleLoopDeadline { .. }
            | Command::CancelTimer { .. } => {}
        }
        let (event_id, event_revision) = if let Some(instance_id) = cancel_instance {
            let e = Event {
                event_id: format!("cancel-{}-{}", l.epoch, entry.sequence),
                run_id: l.run_id.clone(),
                run_digest: r.engine.snapshot().run_digest.clone(),
                expected_revision: r.engine.snapshot().revision,
                at_unix_ms: now,
                kind: EventKind::TaskCompleted {
                    instance_id,
                    result: TaskResult::Cancelled,
                },
            };
            let c = crate::writes::persist_event(&tx, &mut r, &e, |_| {})?;
            (Some(e.event_id), Some(c.snapshot.revision))
        } else {
            (None, None)
        };
        crate::writes::persist_receipt(&tx, &r, &receipt(l, &entry))?;
        append(
            &tx,
            &mut a,
            ExecutionAction::Handled {
                epoch: l.epoch,
                command_id: entry.command_id.clone(),
                event_id,
                event_revision,
                at_unix_ms: now,
            },
        )?;
        commit_guard(clock, l, now, l.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(Claimed::Handled {
            command_id: entry.command_id,
        })
    }
}
