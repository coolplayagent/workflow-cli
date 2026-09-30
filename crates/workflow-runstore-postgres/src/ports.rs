use crate::*;
impl MigrationStore for PostgresRunStore {
    fn plan_migration(&mut self, id: &str, request: &MigrationRequest) -> Result<MigrationPlan> {
        let plan = self.read(id, |s| s.plan_migration(id, request))?;
        let compiled =
            workflow_kernel::CompiledBundle::compile(plan.request.target_bundle.clone())?;
        let mut tx = self.client.transaction().map_err(storage)?;
        Self::transaction_settings(&mut tx)?;
        for b in workflow_runstore_sqlite::bundle_bindings(&compiled)? {
            let row = tx.query_opt("SELECT digest FROM workflow_authority.bindings WHERE tenant=$1 AND project=$2 AND kind=$3 AND id=$4 AND version=$5 FOR SHARE", &[&self.tenant,&self.project,&b.kind,&b.id,&b.version]).map_err(storage)?;
            if row.is_some_and(|r| r.get::<_, &str>(0) != b.digest) {
                return Err(Error::new(
                    ErrorCode::BindingConflict,
                    "immutable shared version differs",
                ));
            }
        }
        tx.commit().map_err(storage)?;
        Ok(plan)
    }
    fn migrate_definition(
        &mut self,
        lease: &Lease,
        plan: &MigrationPlan,
        actor: &str,
        _: &dyn Clock,
    ) -> Result<Committed> {
        let reader = self.reader();
        let mut tx = self.client.transaction().map_err(storage)?;
        Self::transaction_settings(&mut tx)?;
        let compiled =
            workflow_kernel::CompiledBundle::compile(plan.request.target_bundle.clone())?;
        bind_versions(
            &mut tx,
            &self.tenant,
            &self.project,
            workflow_runstore_sqlite::bundle_bindings(&compiled)?,
            true,
        )?;
        let (result, _, _) = Self::change_in(
            &mut tx,
            &self.tenant,
            &self.project,
            reader,
            &plan.run_id,
            false,
            |s, c| s.migrate_definition(lease, plan, actor, c),
        )?;
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    fn historical_snapshot(&mut self, id: &str, revision: u64) -> Result<Snapshot> {
        self.read(id, |s| s.historical_snapshot(id, revision))
    }
}
impl RunStore for PostgresRunStore {
    fn acceptance(&mut self, id: &str) -> Result<AcceptanceManifest> {
        self.read(id, |s| s.acceptance(id))
    }
    fn start(&mut self, r: &StartRun) -> Result<Committed> {
        self.change(&r.run_id, true, |s, _| s.start(r))
    }
    fn apply(&mut self, e: &Event) -> Result<Committed> {
        self.change(&e.run_id, false, |s, _| s.apply(e))
    }
    fn get(&mut self, id: &str) -> Result<Snapshot> {
        self.read(id, |s| s.get(id))
    }
    fn bundle(&mut self, id: &str) -> Result<BundleSpec> {
        self.read(id, |s| s.bundle(id))
    }
    fn history(&mut self, id: &str, after: u64, limit: u32) -> Result<Page<RecordedEvent, u64>> {
        self.read(id, |s| s.history(id, after, limit))
    }
    fn outbox(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
        pending: bool,
    ) -> Result<Page<OutboxEntry, u64>> {
        self.read(id, |s| s.outbox(id, after, limit, pending))
    }
    fn acknowledge(&mut self, r: &DeliveryReceipt) -> Result<OutboxEntry> {
        self.change(&r.run_id, false, |s, _| s.acknowledge(r))
    }
    fn verify(&mut self, id: &str) -> Result<Verification> {
        self.read(id, |s| s.verify(id))
    }
    fn list(&mut self, after: Option<&str>, limit: u32) -> Result<Page<RunSummary, String>> {
        validate_limit(limit)?;
        if let Some(id) = after {
            validate_id(id)?;
        }
        let artifacts = self.artifacts.clone();
        let mut tx = self
            .client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(storage)?;
        tx.batch_execute("SET LOCAL statement_timeout='30s'; SET LOCAL idle_in_transaction_session_timeout='30s'").map_err(storage)?;
        let rows = tx.query("SELECT run_id FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND ($3::text IS NULL OR run_id COLLATE \"C\" > $3 COLLATE \"C\") ORDER BY run_id COLLATE \"C\" LIMIT $4", &[&self.tenant,&self.project,&after,&(i64::from(limit)+1)]).map_err(storage)?;
        let mut items = vec![];
        // One bounded image at a time, all under the same MVCC snapshot.
        for row in &rows {
            let id: &str = row.get(0);
            let image = tx.query_one("SELECT image,image_digest FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&self.tenant,&self.project,&id]).map_err(storage)?;
            let reader = artifacts
                .as_ref()
                .map(|r| Box::new(Reader(r.clone())) as Box<dyn ArtifactReader>);
            let mut reducer =
                Self::checked_row(&mut tx, &self.tenant, &self.project, &image, id, reader)?;
            let s = reducer.get(id)?;
            items.push(RunSummary {
                run_id: s.run_id,
                revision: s.revision,
                status: s.status,
                pause: s.pause,
                bundle_digest: s.bundle_digest,
            });
        }
        tx.commit().map_err(storage)?;
        let next_cursor = if items.len() > limit as usize {
            items.pop();
            items.last().map(|s| s.run_id.clone())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
}
// Caller clocks cannot influence shared lease or effect authority. All executor
// methods supply the primary database clock to the same validated reducer.
impl ExecutionStore for PostgresRunStore {
    fn acquire(&mut self, r: &LeaseRequest, _: &dyn Clock) -> Result<Lease> {
        self.change(&r.run_id, false, |s, c| s.acquire(r, c))
    }
    fn renew(&mut self, l: &Lease, ttl: u64, _: &dyn Clock) -> Result<Lease> {
        self.change(&l.run_id, false, |s, c| s.renew(l, ttl, c))
    }
    fn release(&mut self, l: &Lease, _: &dyn Clock) -> Result<()> {
        self.change(&l.run_id, false, |s, c| s.release(l, c))
    }
    fn tick_due(&mut self, l: &Lease, _: &dyn Clock) -> Result<Option<Committed>> {
        self.change(&l.run_id, false, |s, c| s.tick_due(l, c))
    }
    fn claim_next(&mut self, l: &Lease, _: &dyn Clock) -> Result<Claimed> {
        self.change(&l.run_id, false, |s, c| s.claim_next(l, c))
    }
    fn finish_task(
        &mut self,
        l: &Lease,
        id: &str,
        r: &workflow_worker::WorkResult,
        _: &dyn Clock,
    ) -> Result<Committed> {
        self.change(&l.run_id, false, |s, c| s.finish_task(l, id, r, c))
    }
    fn fail_task(
        &mut self,
        l: &Lease,
        id: &str,
        e: &workflow_worker::Error,
        _: &dyn Clock,
    ) -> Result<()> {
        self.change(&l.run_id, false, |s, c| s.fail_task(l, id, e, c))
    }
    fn execution_history(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<ExecutionRecord, u64>> {
        self.read(id, |s| s.execution_history(id, after, limit))
    }
}
impl InboxStore for PostgresRunStore {
    fn receive_signal(&mut self, r: &SignalSubmission, _: &dyn Clock) -> Result<SignalReceipt> {
        self.change(&r.run_id, false, |s, c| s.receive_signal(r, c))
    }
    fn inbox(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<workflow_kernel::InboxEntry, u64>> {
        self.read(id, |s| s.inbox(id, after, limit))
    }
    fn waits(&mut self, id: &str, after: u64, limit: u32) -> Result<Page<WaitRegistration, u64>> {
        self.read(id, |s| s.waits(id, after, limit))
    }
}
impl EffectStore for PostgresRunStore {
    fn validate_effect_dispatch(
        &mut self,
        attempt: &workflow_effects::EffectAttempt,
        _: &dyn Clock,
    ) -> Result<()> {
        self.change(&attempt.intent.run_id, false, |s, c| {
            s.validate_effect_dispatch(attempt, c)
        })
    }
    fn claim_effect(&mut self, l: &Lease, _: &dyn Clock) -> Result<EffectClaim> {
        self.change(&l.run_id, false, |s, c| s.claim_effect(l, c))
    }
    fn observe_effect(
        &mut self,
        l: &Lease,
        id: &str,
        o: &workflow_effects::Observation,
        _: &dyn Clock,
    ) -> Result<Committed> {
        self.change(&l.run_id, false, |s, c| s.observe_effect(l, id, o, c))
    }
    fn resolve_effect(
        &mut self,
        l: &Lease,
        id: &str,
        r: &workflow_effects::ManualResolution,
        _: &dyn Clock,
    ) -> Result<Committed> {
        self.change(&l.run_id, false, |s, c| s.resolve_effect(l, id, r, c))
    }
    fn effects(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<workflow_effects::EffectState, u64>> {
        self.read(id, |s| s.effects(id, after, limit))
    }
}
impl RestorationStore for PostgresRunStore {
    fn import_restored_effect(
        &mut self,
        l: &Lease,
        r: &RestoredEffect,
        _: &dyn Clock,
    ) -> Result<Committed> {
        self.change(&l.run_id, false, |s, c| s.import_restored_effect(l, r, c))
    }
    fn recovery_barrier(&mut self, id: &str) -> Result<Option<RecoveryBarrier>> {
        self.read(id, |s| s.recovery_barrier(id))
    }
    fn acknowledge_recovery(
        &mut self,
        id: &str,
        r: &RecoveryAcknowledgement,
        _: &dyn Clock,
    ) -> Result<bool> {
        self.change(id, false, |s, c| s.acknowledge_recovery(id, r, c))
    }
}
