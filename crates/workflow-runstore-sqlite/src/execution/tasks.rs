use super::*;
use workflow_kernel::{EventKind, NodeState};
use workflow_worker::WorkResult;
impl SqliteRunStore {
    pub(crate) fn session_checkpoint(
        &mut self,
        request: &workflow_worker::WorkRequest,
        update: Option<(Option<&str>, &workflow_models::ModelCheckpoint)>,
        clock: &dyn Clock,
    ) -> Result<Option<workflow_models::ModelCheckpoint>> {
        let workflow_worker::InvocationScope::Workflow {
            run_id,
            attempt_id,
            lease_epoch,
            ..
        } = &request.scope
        else {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "session persistence requires a workflow attempt",
            ));
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, run_id, self.artifacts.as_deref())?;
        let (mut authority, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        let lease = authority
            .lease
            .clone()
            .ok_or_else(|| Error::new(ErrorCode::LeaseConflict, "session owner missing"))?;
        live(&authority, &r, &lease, now)?;
        let attempt = authority
            .attempts
            .get(attempt_id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "session attempt missing"))?;
        if attempt.prepared.request != *request
            || *lease_epoch != lease.epoch
            || attempt.outcome.is_some()
            || now >= request.deadline_unix_ms
        {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "session attempt is stale or settled",
            ));
        }
        let command_id = attempt.prepared.command_id.clone();
        if let Some((previous, checkpoint)) = update {
            if checkpoint.admitted.is_some() {
                let workflow_worker::InvocationScope::Workflow {
                    node_instance_id, ..
                } = &request.scope
                else {
                    unreachable!()
                };
                if r.engine
                    .snapshot()
                    .frames
                    .values()
                    .flat_map(|f| f.nodes.values())
                    .any(|n| {
                        format!("instance-{}", n.instance_id) == *node_instance_id
                            && n.cancel_requested
                    })
                {
                    return Err(Error::new(
                        ErrorCode::TransitionRejected,
                        "cancelled activity cannot admit another model or tool call",
                    ));
                }
            }
            let action = ExecutionAction::ModelCheckpoint {
                epoch: lease.epoch,
                attempt_id: attempt_id.clone(),
                previous_digest: previous.map(str::to_owned),
                checkpoint: Box::new(checkpoint.clone()),
                at_unix_ms: now,
            };
            proof::verify(&r, &authority, &action, self.artifacts.as_deref())?;
            append(&tx, &mut authority, action)?;
        }
        let result = authority.model_checkpoints.get(&command_id).cloned();
        commit_guard(
            &self.admission,
            clock,
            &lease,
            now,
            request.deadline_unix_ms,
        )?;
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    pub(crate) fn record_progress(
        &mut self,
        lease: &Lease,
        attempt_id: &str,
        clock: &dyn Clock,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, &lease.run_id, self.artifacts.as_deref())?;
        let (mut authority, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        live(&authority, &r, lease, now)?;
        append(
            &tx,
            &mut authority,
            ExecutionAction::Progress {
                epoch: lease.epoch,
                attempt_id: attempt_id.into(),
                at_unix_ms: now,
            },
        )?;
        commit_guard(&self.admission, clock, lease, now, lease.expires_at_unix_ms)?;
        tx.commit().map_err(storage)
    }
    pub(crate) fn finish_owned_task(
        &mut self,
        l: &Lease,
        id: &str,
        result: &WorkResult,
        clock: &dyn Clock,
    ) -> Result<Committed> {
        self.finish_task_internal(l, id, result, clock, |_| {})
    }
    pub(crate) fn finish_task_internal(
        &mut self,
        l: &Lease,
        id: &str,
        result: &WorkResult,
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
        let attempt = a
            .attempts
            .get(id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "attempt not found"))?
            .clone();
        if let Some(AttemptOutcome::Finished {
            result: previous, ..
        }) = &attempt.outcome
        {
            if previous != result {
                return Err(Error::new(
                    ErrorCode::ReceiptConflict,
                    "attempt already committed a different result",
                ));
            }
            let snapshot = r.engine.snapshot().clone();
            tx.commit().map_err(storage)?;
            return Ok(Committed {
                transition: Transition {
                    revision: snapshot.revision,
                    duplicate: true,
                    commands: vec![],
                },
                snapshot,
            });
        }
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        if attempt.prepared.epoch != l.epoch || attempt.outcome.is_some() {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "attempt belongs to an old owner or is already settled",
            ));
        }
        let p = &attempt.prepared;
        let entry = proof::entry(&r, &p.command_id)?.clone();
        let capability = proof::capability(&r, &entry)?;
        workflow_worker::accept_result(&p.request, &p.grant, &capability, result.clone(), now)?;
        proof::model_record(&r, &a, &p.command_id, &p.request, result)?;
        proof::verify_artifacts(result, &p.request, self.artifacts.as_deref())?;
        let Command::ExecuteTask {
            instance_id,
            frame_id,
            node_id,
            ..
        } = &entry.command
        else {
            unreachable!()
        };
        let node = &r.engine.snapshot().frames[frame_id].nodes[node_id];
        if !matches!(
            node.state,
            NodeState::TaskReady | NodeState::CancelRequested
        ) || node.inputs != p.request.inputs
        {
            return Err(Error::new(
                ErrorCode::TransitionRejected,
                "task is no longer eligible for this result",
            ));
        }
        let event = Event {
            event_id: format!("{id}.done"),
            run_id: l.run_id.clone(),
            run_digest: r.engine.snapshot().run_digest.clone(),
            expected_revision: r.engine.snapshot().revision,
            at_unix_ms: now,
            kind: EventKind::TaskCompleted {
                instance_id: *instance_id,
                result: proof::task_result(result)?,
            },
        };
        let committed =
            crate::writes::persist_event(&tx, &mut r, &event, self.artifacts.as_deref(), &hook)?;
        crate::writes::persist_receipt(&tx, &r, &receipt(l, &entry))?;
        append(
            &tx,
            &mut a,
            ExecutionAction::Finished {
                attempt_id: id.into(),
                result: result.clone(),
                event_id: event.event_id,
                event_revision: committed.snapshot.revision,
                at_unix_ms: now,
            },
        )?;
        hook("execution_written");
        let commit_at = commit_guard(&self.admission, clock, l, now, p.request.deadline_unix_ms)?;
        check_signal_admission(&committed.snapshot, commit_at)?;
        hook("before_commit");
        tx.commit().map_err(storage)?;
        hook("after_commit");
        Ok(committed)
    }
    pub(crate) fn fail_owned_task(
        &mut self,
        l: &Lease,
        id: &str,
        error: &workflow_worker::Error,
        clock: &dyn Clock,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        if a.attempts
            .get(id)
            .is_none_or(|a| a.prepared.epoch != l.epoch)
        {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "attempt is not owned by this lease",
            ));
        }
        append(
            &tx,
            &mut a,
            ExecutionAction::Failed {
                attempt_id: id.into(),
                error: error.clone(),
                at_unix_ms: now,
            },
        )?;
        commit_guard(&self.admission, clock, l, now, l.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(())
    }
}
