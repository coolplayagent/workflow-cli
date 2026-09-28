use super::*;
use workflow_effects::{
    CallKind, Decision, EffectAttempt, EffectBinding, EffectChange, EffectIntent, EffectRecord,
    EffectState, EffectStatus, ManualResolution, Observation, decision, operation_key,
};
use workflow_kernel::{EventKind, NodeState, TaskResult};

fn binding<'a>(r: &'a Recovered, entry: &OutboxEntry) -> Result<Option<&'a EffectBinding>> {
    let Command::ExecuteTask {
        frame_id, node_id, ..
    } = &entry.command
    else {
        return Err(corrupt("effect requires its original execute intent"));
    };
    let workflow = &r.engine.snapshot().frames[frame_id].workflow;
    Ok(r.engine
        .bundle()
        .spec()
        .effect_bindings
        .iter()
        .find(|b| &b.workflow == workflow && &b.node_id == node_id))
}
pub(super) fn original<'a>(r: &'a Recovered, entry: &'a OutboxEntry) -> Result<&'a OutboxEntry> {
    match &entry.command {
        Command::ExecuteTask { .. } => Ok(entry),
        Command::CancelTask { instance_id } | Command::ReconcileTask { instance_id } =>
            r.outbox.iter().find(|e| matches!(e.command, Command::ExecuteTask { instance_id: i, .. } if i == *instance_id))
                .ok_or_else(|| corrupt("effect control has no original execute command")),
        _ => Err(corrupt("not an effect command")),
    }
}
pub(crate) fn managed(r: &Recovered, entry: &OutboxEntry) -> Result<bool> {
    if !matches!(
        entry.command,
        Command::ExecuteTask { .. } | Command::CancelTask { .. } | Command::ReconcileTask { .. }
    ) {
        return Ok(false);
    }
    Ok(binding(r, original(r, entry)?)?.is_some())
}
fn intent(r: &Recovered, e: &OutboxEntry, now: u64) -> Result<EffectIntent> {
    let Command::ExecuteTask {
        instance_id,
        frame_id,
        node_id,
        inputs,
        capability,
        ..
    } = &e.command
    else {
        return Err(corrupt("effect task required"));
    };
    let binding = binding(r, e)?.ok_or_else(|| {
        Error::new(
            ErrorCode::UnsupportedEffect,
            "write requires a frozen effect binding",
        )
    })?;
    let descriptor = r
        .engine
        .bundle()
        .spec()
        .capabilities
        .iter()
        .find(|d| &d.capability == capability)
        .ok_or_else(|| corrupt("effect capability missing"))?;
    let s = r.engine.snapshot();
    let result = EffectIntent {
        schema_version: 1,
        operation_key: operation_key(&s.run_digest, *instance_id)?,
        run_id: s.run_id.clone(),
        run_digest: s.run_digest.clone(),
        instance_id: *instance_id,
        workflow: s.frames[frame_id].workflow.clone(),
        node_id: node_id.clone(),
        command_id: e.command_id.clone(),
        command_digest: e.command_digest.clone(),
        capability: descriptor.clone(),
        inputs: inputs.clone(),
        input_digest: digest(inputs)?,
        policy: binding.policy.clone(),
        created_at_unix_ms: now,
    };
    result.validate()?;
    Ok(result)
}
fn state<'a>(snapshot: &'a Snapshot, e: &OutboxEntry) -> Result<&'a NodeState> {
    let Command::ExecuteTask {
        frame_id, node_id, ..
    } = &e.command
    else {
        return Err(corrupt("effect original command required"));
    };
    Ok(&snapshot.frames[frame_id].nodes[node_id].state)
}
fn pending_timer_deadline(snapshot: &Snapshot) -> Option<u64> {
    snapshot
        .frames
        .values()
        .flat_map(|f| f.nodes.values())
        .filter_map(|n| match n.state {
            NodeState::Waiting { deadline_unix_ms } => Some(deadline_unix_ms),
            NodeState::Child {
                deadline_unix_ms: Some(deadline),
                exhausting: false,
                ..
            } => Some(deadline),
            _ => None,
        })
        .min()
}
fn snapshot_at(r: &Recovered, revision: u64) -> Result<Snapshot> {
    let cp = r.engine.checkpoint()?;
    if revision == 0 || revision > r.engine.snapshot().revision {
        return Err(corrupt("effect revision outside run history"));
    }
    let (mut engine, _) = workflow_kernel::Engine::start(
        r.engine.bundle().clone(),
        &cp.run_id,
        cp.inputs,
        cp.started_at_unix_ms,
        cp.limits,
    )?;
    for e in &r.events {
        if e.revision > revision {
            break;
        }
        engine.apply(e.event.clone())?;
    }
    Ok(engine.snapshot().clone())
}
fn outcome(status: &EffectStatus) -> Option<TaskResult> {
    match status {
        EffectStatus::Applied { receipt } => Some(TaskResult::Succeeded {
            outputs: receipt.outputs.clone(),
        }),
        EffectStatus::Failed { code } => Some(TaskResult::Failed { code: code.clone() }),
        EffectStatus::Cancelled => Some(TaskResult::Cancelled),
        EffectStatus::Uncertain { reason } => Some(TaskResult::Uncertain {
            reason: reason.clone(),
        }),
        _ => None,
    }
}
fn event_kind(r: &Recovered, entry: &OutboxEntry, result: TaskResult) -> Result<EventKind> {
    let Command::ExecuteTask { instance_id, .. } = entry.command else {
        return Err(corrupt("effect task required"));
    };
    Ok(
        if *state(r.engine.snapshot(), entry)? == NodeState::Reconciling {
            EventKind::TaskReconciled {
                instance_id,
                result,
            }
        } else {
            EventKind::TaskCompleted {
                instance_id,
                result,
            }
        },
    )
}
fn unchanged(r: &Recovered, duplicate: bool) -> Committed {
    Committed {
        snapshot: r.engine.snapshot().clone(),
        transition: Transition {
            revision: r.engine.snapshot().revision,
            duplicate,
            commands: vec![],
        },
    }
}
fn commit_record(
    c: &Connection,
    r: &mut Recovered,
    a: &mut Authority,
    l: &Lease,
    record: EffectRecord,
    hook: &impl Fn(&str),
) -> Result<Committed> {
    // Derive status from the record before touching kernel state. The final
    // journal action binds that status to its event/receipt in this transaction.
    let mut projected = a.clone();
    projected.apply(&ExecutionAction::Effect {
        record: Box::new(record.clone()),
        transition: None,
    })?;
    let effect = &projected.effects[record.change.key()];
    let original = proof::entry(r, &effect.intent.command_id)?.clone();
    let (committed, transition) = if let Some(result) = outcome(&effect.status) {
        let pending = r
            .outbox
            .iter()
            .find(|e| e.receipt.is_none())
            .cloned()
            .ok_or_else(|| corrupt("effect settlement has no pending command"))?;
        if self::original(r, &pending)?.command_id != original.command_id {
            return Err(Error::new(
                ErrorCode::DeliveryOrder,
                "effect is not the first pending command",
            ));
        }
        let event = Event {
            event_id: format!("effect-{}-{}", l.epoch, a.sequence + 1),
            run_id: l.run_id.clone(),
            run_digest: r.engine.snapshot().run_digest.clone(),
            expected_revision: r.engine.snapshot().revision,
            at_unix_ms: record.at_unix_ms,
            kind: event_kind(r, &original, result)?,
        };
        let committed = crate::writes::persist_event(c, r, &event, hook)?;
        crate::writes::persist_receipt(c, r, &receipt(l, &pending))?;
        let transition = EffectTransition {
            event_id: event.event_id,
            event_revision: committed.snapshot.revision,
            command_id: pending.command_id,
        };
        (committed, Some(transition))
    } else {
        (unchanged(r, false), None)
    };
    append(
        c,
        a,
        ExecutionAction::Effect {
            record: Box::new(record),
            transition,
        },
    )?;
    hook("execution_written");
    Ok(committed)
}
impl EffectStore for SqliteRunStore {
    fn claim_effect(&mut self, l: &Lease, clock: &dyn Clock) -> Result<EffectClaim> {
        self.claim_effect_internal(l, clock, |_| {})
    }
    fn observe_effect(
        &mut self,
        l: &Lease,
        id: &str,
        observation: &Observation,
        clock: &dyn Clock,
    ) -> Result<Committed> {
        self.observe_effect_internal(l, id, observation, clock, |_| {})
    }
    fn resolve_effect(
        &mut self,
        l: &Lease,
        key: &str,
        resolution: &ManualResolution,
        clock: &dyn Clock,
    ) -> Result<Committed> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, records) = read(&tx, &r, self.artifacts.as_deref())?;
        for item in records {
            if let ExecutionAction::Effect { record, .. } = item.action
                && let EffectChange::Resolved {
                    operation_key,
                    resolution: old,
                } = record.change
                && old.resolution_id == resolution.resolution_id
            {
                if operation_key != key || old != *resolution {
                    return Err(Error::new(
                        ErrorCode::ReceiptConflict,
                        "manual resolution ID content conflict",
                    ));
                }
                return Ok(unchanged(&r, true));
            }
        }
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        let record = EffectRecord {
            epoch: l.epoch,
            at_unix_ms: now,
            change: EffectChange::Resolved {
                operation_key: key.into(),
                resolution: resolution.clone(),
            },
        };
        let committed = commit_record(&tx, &mut r, &mut a, l, record, &|_| {})?;
        let end = commit_guard(clock, l, now, l.expires_at_unix_ms)?;
        check_signal_admission(&committed.snapshot, end)?;
        tx.commit().map_err(storage)?;
        Ok(committed)
    }
    fn effects(&mut self, id: &str, after: u64, limit: u32) -> Result<Page<EffectState, u64>> {
        validate_limit(limit)?;
        let tx = self.connection.transaction().map_err(storage)?;
        let r = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        let (a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let mut items: Vec<_> = a
            .effects
            .into_values()
            .filter(|e| e.intent.instance_id > after)
            .collect();
        items.sort_by_key(|e| e.intent.instance_id);
        let next_cursor = if items.len() > limit as usize {
            Some(items[limit as usize - 1].intent.instance_id)
        } else {
            None
        };
        items.truncate(limit as usize);
        tx.commit().map_err(storage)?;
        Ok(Page { items, next_cursor })
    }
}
impl SqliteRunStore {
    pub(crate) fn claim_effect_internal(
        &mut self,
        l: &Lease,
        clock: &dyn Clock,
        hook: impl Fn(&str),
    ) -> Result<EffectClaim> {
        hook("before_transaction");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        if r.engine.snapshot().pause.is_some() {
            return Ok(EffectClaim::Idle);
        }
        let Some(pending) = r.outbox.iter().find(|e| e.receipt.is_none()).cloned() else {
            return Ok(EffectClaim::Idle);
        };
        if !managed(&r, &pending)? {
            return Ok(EffectClaim::Idle);
        }
        let original = original(&r, &pending)?.clone();
        let candidate = intent(&r, &original, now)?;
        let node = state(r.engine.snapshot(), &original)?;
        let allow_write = *node == NodeState::TaskReady;
        if node.terminal()
            || (*node == NodeState::CancelRequested
                && !a.effects.contains_key(&candidate.operation_key))
        {
            let (event_id, event_revision, committed) = if node.terminal() {
                (None, None, unchanged(&r, false))
            } else {
                let event = Event {
                    event_id: format!("effect-cancel-{}-{}", l.epoch, a.sequence + 1),
                    run_id: l.run_id.clone(),
                    run_digest: r.engine.snapshot().run_digest.clone(),
                    expected_revision: r.engine.snapshot().revision,
                    at_unix_ms: now,
                    kind: event_kind(&r, &original, TaskResult::Cancelled)?,
                };
                let c = crate::writes::persist_event(&tx, &mut r, &event, &hook)?;
                (Some(event.event_id), Some(c.snapshot.revision), c)
            };
            crate::writes::persist_receipt(&tx, &r, &receipt(l, &pending))?;
            append(
                &tx,
                &mut a,
                ExecutionAction::Handled {
                    epoch: l.epoch,
                    command_id: pending.command_id,
                    event_id,
                    event_revision,
                    at_unix_ms: now,
                },
            )?;
            let end = commit_guard(clock, l, now, l.expires_at_unix_ms)?;
            if event_revision.is_some() {
                check_signal_admission(&committed.snapshot, end)?;
            }
            tx.commit().map_err(storage)?;
            return Ok(EffectClaim::Handled);
        }
        if !matches!(
            node,
            NodeState::TaskReady | NodeState::CancelRequested | NodeState::Reconciling
        ) {
            return Err(corrupt("effect node is ineligible"));
        }
        let existing = a.effects.get(&candidate.operation_key);
        let (frozen, number, plan) = if let Some(s) = existing {
            (
                s.intent.clone(),
                s.calls.len() as u32 + 1,
                decision(s, l.epoch, now, allow_write),
            )
        } else {
            (candidate, 1, Decision::Call(CallKind::Write))
        };
        match plan {
            Decision::Wait(not_before_unix_ms) => Ok(EffectClaim::Waiting { not_before_unix_ms }),
            Decision::InProgress => Err(Error::new(
                ErrorCode::AttemptInProgress,
                "effect call already prepared; never dispatch it twice",
            )),
            Decision::Done => Err(corrupt("settled effect still has an active task")),
            Decision::Manual(reason) => {
                let key = frozen.operation_key.clone();
                if !matches!(a.effects[&key].status, EffectStatus::Uncertain { .. }) {
                    let record = EffectRecord {
                        epoch: l.epoch,
                        at_unix_ms: now,
                        change: EffectChange::Stopped {
                            operation_key: key.clone(),
                            reason: reason.clone(),
                        },
                    };
                    let committed = commit_record(&tx, &mut r, &mut a, l, record, &hook)?;
                    let end = commit_guard(clock, l, now, l.expires_at_unix_ms)?;
                    check_signal_admission(&committed.snapshot, end)?;
                }
                tx.commit().map_err(storage)?;
                Ok(EffectClaim::Manual {
                    operation_key: key,
                    reason,
                })
            }
            Decision::Call(kind) => {
                let mut deadline = now
                    .saturating_add(frozen.capability.timeout_ms)
                    .min(l.expires_at_unix_ms);
                if kind == CallKind::Write {
                    deadline = deadline.min(frozen.write_deadline());
                    if let Some(timer) = pending_timer_deadline(r.engine.snapshot()) {
                        if now >= timer {
                            return Err(Error::new(
                                ErrorCode::TransitionRejected,
                                "advance due workflow timers before admitting a write",
                            ));
                        }
                        deadline = deadline.min(timer);
                    }
                }
                let p = EffectAttempt {
                    intent: frozen,
                    attempt_id: format!("effect-call-{}-{}-{number}", original.sequence, l.epoch),
                    epoch: l.epoch,
                    number,
                    kind,
                    prepared_revision: r.engine.snapshot().revision,
                    issued_at_unix_ms: now,
                    deadline_unix_ms: deadline,
                };
                let record = EffectRecord {
                    epoch: l.epoch,
                    at_unix_ms: now,
                    change: EffectChange::Prepared {
                        request_digest: digest(&p)?,
                        attempt: Box::new(p.clone()),
                    },
                };
                append(
                    &tx,
                    &mut a,
                    ExecutionAction::Effect {
                        record: Box::new(record),
                        transition: None,
                    },
                )?;
                hook("intent_written");
                commit_guard(clock, l, now, deadline)?;
                hook("before_commit");
                tx.commit().map_err(storage)?;
                hook("after_commit");
                Ok(EffectClaim::Call {
                    attempt: Box::new(p),
                })
            }
        }
    }
    pub(crate) fn observe_effect_internal(
        &mut self,
        l: &Lease,
        id: &str,
        observation: &Observation,
        clock: &dyn Clock,
        hook: impl Fn(&str),
    ) -> Result<Committed> {
        hook("before_transaction");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let (effect, call) = a
            .effects
            .values()
            .find_map(|s| {
                s.calls
                    .iter()
                    .find(|c| c.attempt.attempt_id == id)
                    .map(|c| (s, c))
            })
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "effect attempt missing"))?;
        if let Some(old) = &call.observation {
            if old != observation {
                return Err(Error::new(
                    ErrorCode::ReceiptConflict,
                    "effect attempt observation conflict",
                ));
            }
            return Ok(unchanged(&r, true));
        }
        let key = effect.intent.operation_key.clone();
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        let record = EffectRecord {
            epoch: l.epoch,
            at_unix_ms: now,
            change: EffectChange::Observed {
                operation_key: key,
                attempt_id: id.into(),
                observation: observation.clone(),
            },
        };
        let committed = commit_record(&tx, &mut r, &mut a, l, record, &hook)?;
        let end = commit_guard(clock, l, now, l.expires_at_unix_ms)?;
        if committed.transition.revision > r.events.last().map_or(1, |e| e.revision) {
            check_signal_admission(&committed.snapshot, end)?;
        }
        hook("before_commit");
        tx.commit().map_err(storage)?;
        hook("after_commit");
        Ok(committed)
    }
}

pub(super) fn verify(
    r: &Recovered,
    a: &Authority,
    record: &EffectRecord,
    transition: &Option<EffectTransition>,
) -> Result<()> {
    let effect = a
        .effects
        .get(record.change.key())
        .ok_or_else(|| corrupt("effect proof has no ledger state"))?;
    let entry = proof::entry(r, &effect.intent.command_id)?;
    if intent(r, entry, effect.intent.created_at_unix_ms)? != effect.intent {
        return Err(corrupt(
            "effect intent differs from frozen task/target/policy/input",
        ));
    }
    if let EffectChange::Prepared { attempt, .. } = &record.change {
        if attempt.attempt_id
            != format!(
                "effect-call-{}-{}-{}",
                entry.sequence, record.epoch, attempt.number
            )
        {
            return Err(corrupt(
                "effect attempt ID differs from its command, epoch and number",
            ));
        }
        let prior = snapshot_at(r, attempt.prepared_revision)?;
        let node = state(&prior, entry)?;
        if prior.pause.is_some()
            || prior.now_unix_ms > record.at_unix_ms
            || !matches!(
                node,
                NodeState::TaskReady | NodeState::CancelRequested | NodeState::Reconciling
            )
            || (attempt.kind == CallKind::Write
                && (*node != NodeState::TaskReady
                    || pending_timer_deadline(&prior)
                        .is_some_and(|d| attempt.deadline_unix_ms > d)))
            || transition.is_some()
        {
            return Err(corrupt(
                "effect call was admitted outside its active node/clock boundary",
            ));
        }
        return Ok(());
    }
    match (outcome(&effect.status), transition) {
        (None, None) => Ok(()),
        (Some(result), Some(t)) => {
            let event = proof::stored_event(r, &t.event_id, t.event_revision, record.at_unix_ms)?;
            let prior = snapshot_at(r, t.event_revision - 1)?;
            let Command::ExecuteTask { instance_id, .. } = entry.command else {
                return Err(corrupt("effect original task required"));
            };
            let expected = if *state(&prior, entry)? == NodeState::Reconciling {
                EventKind::TaskReconciled {
                    instance_id,
                    result,
                }
            } else {
                EventKind::TaskCompleted {
                    instance_id,
                    result,
                }
            };
            if event.kind != expected
                || original(r, proof::entry(r, &t.command_id)?)?.command_id != entry.command_id
            {
                return Err(corrupt(
                    "effect receipt differs from committed task outcome",
                ));
            }
            proof::verify_receipt(r, &t.command_id, record.epoch)
        }
        _ => Err(corrupt(
            "effect final outcome and kernel transition must commit together",
        )),
    }
}
pub(super) fn verify_handled(r: &Recovered, a: &Authority, entry: &OutboxEntry) -> Result<()> {
    let original = original(r, entry)?;
    let candidate = intent(r, original, 1)?;
    if !state(r.engine.snapshot(), original)?.terminal()
        || a.effects.get(&candidate.operation_key).is_some_and(|s| {
            !matches!(
                s.status,
                EffectStatus::Applied { .. }
                    | EffectStatus::Failed { .. }
                    | EffectStatus::Cancelled
            )
        })
    {
        return Err(corrupt("cannot acknowledge an unresolved write effect"));
    }
    Ok(())
}

pub(super) fn verify_coverage(r: &Recovered, records: &[ExecutionRecord]) -> Result<()> {
    use std::collections::BTreeSet;
    let mut events = BTreeSet::new();
    for e in &r.events {
        if let EventKind::TaskCompleted { instance_id, .. }
        | EventKind::TaskReconciled { instance_id, .. } = e.event.kind
        {
            let original = r.outbox.iter().find(|c| matches!(c.command, Command::ExecuteTask { instance_id: i, .. } if i == instance_id))
                .ok_or_else(|| corrupt("task event has no original command"))?;
            if managed(r, original)? {
                events.insert(e.event.event_id.as_str());
            }
        }
    }
    let mut receipts = BTreeSet::new();
    for e in &r.outbox {
        if e.receipt.is_some() && managed(r, e)? {
            receipts.insert(e.command_id.as_str());
        }
    }
    let mut event_proofs = vec![];
    let mut receipt_proofs = vec![];
    for rcd in records {
        match &rcd.action {
            ExecutionAction::Effect {
                transition: Some(t),
                ..
            } => {
                event_proofs.push(t.event_id.as_str());
                receipt_proofs.push(t.command_id.as_str());
            }
            ExecutionAction::Handled {
                command_id,
                event_id,
                ..
            } if managed(r, proof::entry(r, command_id)?)? => {
                receipt_proofs.push(command_id.as_str());
                if let Some(id) = event_id {
                    event_proofs.push(id.as_str());
                }
            }
            _ => {}
        }
    }
    if events.len() != event_proofs.len()
        || events != event_proofs.into_iter().collect()
        || receipts.len() != receipt_proofs.len()
        || receipts != receipt_proofs.into_iter().collect()
    {
        return Err(corrupt(
            "managed write events/receipts and effect execution proofs differ",
        ));
    }
    Ok(())
}
