use super::*;
impl SqliteRunStore {
    /// Explicit, transactional storage upgrade. Ordinary create/open never migrates.
    pub fn migrate(path: impl AsRef<Path>) -> Result<Self> {
        Self::migrate_with_artifacts(path, None)
    }
    pub fn migrate_with_artifacts(
        path: impl AsRef<Path>,
        artifacts: Option<Box<dyn workflow_artifacts::ArtifactReader>>,
    ) -> Result<Self> {
        let mut connection = connect(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(storage)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage)?;
        if app != APPLICATION_ID || !(1..=STORAGE_VERSION).contains(&version) {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "only run store schema 1 through 7 can migrate",
            ));
        }
        if version < STORAGE_VERSION {
            if version == 1 {
                tx.execute_batch(SCHEMA).map_err(storage)?;
            }
            let ids = {
                let mut q = tx
                    .prepare("SELECT run_id FROM runs ORDER BY run_id")
                    .map_err(storage)?;
                q.query_map([], |r| r.get::<_, String>(0))
                    .map_err(storage)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(storage)?
            };
            for id in &ids {
                if version == 1 {
                    init_head(&tx, id)?;
                }
            }
            tx.pragma_update(None, "user_version", STORAGE_VERSION)
                .map_err(storage)?;
            for id in ids {
                crate::recovery::recover(&tx, &id, artifacts.as_deref())?;
            }
        } else {
            check_version(&tx)?;
        }
        tx.commit().map_err(storage)?;
        Ok(Self {
            connection,
            artifacts,
        })
    }
    pub(crate) fn acquire_lease(
        &mut self,
        request: &LeaseRequest,
        clock: &dyn Clock,
    ) -> Result<Lease> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, &request.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        if now < r.engine.snapshot().now_unix_ms {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "host time precedes run time",
            ));
        }
        let lease = a.next_lease(request, now)?;
        append(
            &tx,
            &mut a,
            ExecutionAction::Acquired {
                lease: lease.clone(),
            },
        )?;
        commit_guard(clock, &lease, now, lease.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(lease)
    }
    pub(crate) fn renew_lease(&mut self, l: &Lease, ttl: u64, clock: &dyn Clock) -> Result<Lease> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        let mut renewed = l.clone();
        renewed.expires_at_unix_ms = lease_deadline(now, ttl)?;
        append(
            &tx,
            &mut a,
            ExecutionAction::Renewed {
                lease: renewed.clone(),
                at_unix_ms: now,
            },
        )?;
        commit_guard(clock, l, now, l.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(renewed)
    }
    pub(crate) fn release_lease(&mut self, l: &Lease, clock: &dyn Clock) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let r = crate::recovery::recover(&tx, &l.run_id, self.artifacts.as_deref())?;
        let (mut a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let now = clock.now_unix_ms()?;
        live(&a, &r, l, now)?;
        append(
            &tx,
            &mut a,
            ExecutionAction::Released {
                epoch: l.epoch,
                at_unix_ms: now,
            },
        )?;
        commit_guard(clock, l, now, l.expires_at_unix_ms)?;
        tx.commit().map_err(storage)?;
        Ok(())
    }
    pub(crate) fn read_execution_history(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<ExecutionRecord, u64>> {
        validate_limit(limit)?;
        let tx = self.connection.transaction().map_err(storage)?;
        let r = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        let (_, records) = read(&tx, &r, self.artifacts.as_deref())?;
        let mut items: Vec<_> = records
            .into_iter()
            .filter(|r| r.sequence > after)
            .take(limit as usize + 1)
            .collect();
        let next_cursor = if items.len() > limit as usize {
            items.pop();
            items.last().map(|r| r.sequence)
        } else {
            None
        };
        tx.commit().map_err(storage)?;
        Ok(Page { items, next_cursor })
    }
}
