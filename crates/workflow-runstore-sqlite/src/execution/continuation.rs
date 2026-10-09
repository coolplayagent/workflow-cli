use super::*;
pub(super) fn verify(
    r: &Recovered,
    authority: &Authority,
    plan: &ContinuationPlan,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<()> {
    plan.validate()?;
    let h = &plan.handoff;
    let snapshot = r.engine.snapshot();
    if snapshot.run_id != h.source_run_id
        || snapshot.run_digest != h.source_run_digest
        || snapshot.revision != h.source_revision
        || snapshot.status != RunStatus::Succeeded
        || snapshot.pause.is_some()
        || authority.recovery.is_some()
        || plan.successor.started_at_unix_ms < snapshot.now_unix_ms
        || r.outbox.iter().any(|e| e.receipt.is_none())
        || authority.effects.values().any(|s| {
            !matches!(
                s.status,
                workflow_effects::EffectStatus::Applied { .. }
                    | workflow_effects::EffectStatus::Failed { .. }
                    | workflow_effects::EffectStatus::Cancelled
            )
        })
    {
        return Err(Error::new(
            ErrorCode::TransitionRejected,
            "continuation requires the exact successful, settled source segment",
        ));
    }
    for fact in &h.verified_facts {
        if !snapshot
            .frames
            .values()
            .flat_map(|f| f.nodes.values())
            .any(|n| {
                n.instance_id == fact.instance_id
                    && n.state == workflow_kernel::NodeState::Succeeded
                    && n.outputs.get(&fact.field) == Some(&fact.value)
            })
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "handoff fact differs from acknowledged task output",
            ));
        }
    }
    for link in &h.artifacts {
        let committed = authority.attempts.values().any(|a| match &a.outcome {
            Some(AttemptOutcome::Finished { result, .. }) => {
                let evidence = match &result.outcome {
                    workflow_worker::AdapterOutcome::Succeeded { evidence, .. }
                    | workflow_worker::AdapterOutcome::Failed { evidence, .. } => evidence,
                };
                evidence
                    .iter()
                    .any(|e| e.artifact_id == link.artifact_id && e.digest == link.digest)
            }
            _ => false,
        });
        if !committed {
            return Err(Error::new(
                ErrorCode::ArtifactRejected,
                "handoff artifact was not acknowledged by source execution",
            ));
        }
        artifacts
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::ArtifactUnavailable,
                    "handoff artifacts require retained evidence",
                )
            })?
            .verify(link)?;
    }
    Ok(())
}
impl ContinuationStore for SqliteRunStore {
    fn prepare_continuation(
        &mut self,
        lease: &Lease,
        plan: &ContinuationPlan,
        clock: &dyn Clock,
    ) -> Result<ContinuationPlan> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, &lease.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        if let Some(recorded) = &a.continuation {
            if recorded != plan {
                return Err(Error::new(
                    ErrorCode::ReceiptConflict,
                    "source already reserved a different successor",
                ));
            }
            return Ok(recorded.clone());
        }
        let now = clock.now_unix_ms()?;
        live(&a, &r, lease, now)?;
        verify(&r, &a, plan, self.artifacts.as_deref())?;
        append(
            &tx,
            &mut a,
            ExecutionAction::Continued {
                epoch: lease.epoch,
                plan: Box::new(plan.clone()),
                at_unix_ms: now,
            },
        )?;
        commit_guard(&self.admission, clock, lease, now, lease.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(plan.clone())
    }
    fn continuation(&mut self, id: &str) -> Result<Option<ContinuationPlan>> {
        let tx = self.connection.transaction().map_err(storage)?;
        let r = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        let (a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        tx.commit().map_err(storage)?;
        Ok(a.continuation)
    }
}
