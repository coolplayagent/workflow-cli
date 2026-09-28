use crate::*;
use workflow_worker::Clock;
impl SqliteRunStore {
    /// Apply only to an isolated restored database before exposing it for use.
    /// Retains all old journal records and budgets; active runs start paused.
    pub fn fence_restored_runs(
        &mut self,
        backup_digest: &str,
        generation: &str,
        actor: &str,
        reason: &str,
        clock: &dyn Clock,
    ) -> Result<Vec<(String, RecoveryBarrier)>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let now = clock.now_unix_ms()?;
        let ids = {
            let mut q = tx
                .prepare("SELECT run_id FROM runs ORDER BY run_id")
                .map_err(storage)?;
            q.query_map([], |r| r.get::<_, String>(0))
                .map_err(storage)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        let mut result = vec![];
        for id in ids {
            let mut r = crate::recovery::recover(&tx, &id, self.artifacts.as_deref())?;
            let (mut a, _) = execution::read(&tx, &r, self.artifacts.as_deref())?;
            if now < r.engine.snapshot().now_unix_ms {
                return Err(Error::new(
                    ErrorCode::LeaseConflict,
                    "restore clock precedes the backup run",
                ));
            }
            let recovery = RecoveryBarrier {
                generation: generation.into(),
                backup_digest: backup_digest.into(),
                source_revision: r.engine.snapshot().revision,
                source_state_digest: digest(r.engine.snapshot())?,
                restored_by: actor.into(),
                reason: reason.into(),
            };
            execution::append(
                &tx,
                &mut a,
                ExecutionAction::Restored {
                    recovery: recovery.clone(),
                    at_unix_ms: now,
                },
            )?;
            if r.engine.snapshot().status == RunStatus::Running
                && r.engine.snapshot().pause.is_none()
            {
                let event = Event {
                    event_id: format!("restore-pause-{}", &generation[7..]),
                    run_id: id.clone(),
                    run_digest: r.engine.snapshot().run_digest.clone(),
                    expected_revision: r.engine.snapshot().revision,
                    at_unix_ms: now,
                    kind: workflow_kernel::EventKind::Pause {
                        reason: "restored backup; inspect recovery barrier before resuming".into(),
                    },
                };
                crate::writes::persist_event(&tx, &mut r, &event, |_| {})?;
            }
            result.push((id, recovery));
        }
        if clock.now_unix_ms()? < now {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "restore clock reversed before commit",
            ));
        }
        tx.commit().map_err(storage)?;
        Ok(result)
    }
}
impl RestorationStore for SqliteRunStore {
    fn import_restored_effect(
        &mut self,
        lease: &Lease,
        import: &RestoredEffect,
        clock: &dyn Clock,
    ) -> Result<Committed> {
        self.import_recovered_effect(lease, import, clock)
    }
    fn recovery_barrier(&mut self, id: &str) -> Result<Option<RecoveryBarrier>> {
        let tx = self.connection.transaction().map_err(storage)?;
        let r = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        let (a, _) = execution::read(&tx, &r, self.artifacts.as_deref())?;
        tx.commit().map_err(storage)?;
        Ok(a.recovery)
    }
    fn acknowledge_recovery(
        &mut self,
        id: &str,
        resolution: &RecoveryAcknowledgement,
        clock: &dyn Clock,
    ) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        let (mut a, records) = execution::read(&tx, &r, self.artifacts.as_deref())?;
        for record in records {
            if let ExecutionAction::RecoveryAcknowledged {
                resolution: old, ..
            } = record.action
                && old.resolution_id == resolution.resolution_id
            {
                if old != *resolution {
                    return Err(Error::new(
                        ErrorCode::ReceiptConflict,
                        "recovery resolution ID content conflict",
                    ));
                }
                return Ok(true);
            }
        }
        let now = clock.now_unix_ms()?;
        if now < r.engine.snapshot().now_unix_ms {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "recovery clock precedes run time",
            ));
        }
        execution::append(
            &tx,
            &mut a,
            ExecutionAction::RecoveryAcknowledged {
                resolution: resolution.clone(),
                at_unix_ms: now,
            },
        )?;
        if clock.now_unix_ms()? < now {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "recovery clock reversed before commit",
            ));
        }
        tx.commit().map_err(storage)?;
        Ok(false)
    }
}
