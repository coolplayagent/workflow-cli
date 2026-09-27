use super::*;
use workflow_kernel::{EventKind, NodeState};
use workflow_worker::WorkResult;
impl SqliteRunStore {
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
        let committed = crate::writes::persist_event(&tx, &mut r, &event, &hook)?;
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
        commit_guard(clock, l, now, p.request.deadline_unix_ms)?;
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
        commit_guard(clock, l, now, l.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(())
    }
}
