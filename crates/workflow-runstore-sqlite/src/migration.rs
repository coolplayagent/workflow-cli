use crate::*;
use rusqlite::{OptionalExtension, params};
use workflow_kernel::{CompiledBundle, EventKind};
use workflow_worker::Clock;

fn eligible(authority: &Authority) -> Result<()> {
    if authority.recovery.is_some() || !authority.effects.is_empty() {
        return Err(Error::new(
            ErrorCode::MigrationBlocked,
            "finish or reconcile the old run before migration; recorded business effects and recovery barriers cannot be restarted",
        ));
    }
    Ok(())
}

pub(crate) fn retained_bundle(c: &Connection, bundle: &CompiledBundle, create: bool) -> Result<()> {
    crate::bindings::lock_bindings(c, bundle, create)?;
    let old: Option<String> = c
        .query_row(
            "SELECT document FROM bundles WHERE digest=?1",
            [bundle.digest()],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    let expected = document(bundle.spec())?;
    match old {
        Some(actual) if actual == expected => Ok(()),
        None if create => {
            c.execute(
                "INSERT INTO bundles(digest,document) VALUES(?1,?2)",
                params![bundle.digest(), expected],
            )
            .map_err(storage)?;
            Ok(())
        }
        _ => Err(corrupt("migration target bundle is missing or changed")),
    }
}

impl MigrationStore for SqliteRunStore {
    fn plan_migration(&mut self, id: &str, request: &MigrationRequest) -> Result<MigrationPlan> {
        let tx = self.connection.transaction().map_err(storage)?;
        let recovered = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        let (authority, _) = crate::execution::read(&tx, &recovered, self.artifacts.as_deref())?;
        eligible(&authority)?;
        let plan = recovered.engine.plan_migration(request)?;
        crate::bindings::compatible(
            &tx,
            &CompiledBundle::compile(plan.request.target_bundle.clone())?,
        )?;
        tx.commit().map_err(storage)?;
        Ok(plan)
    }
    fn migrate_definition(
        &mut self,
        lease: &Lease,
        plan: &MigrationPlan,
        actor: &str,
        clock: &dyn Clock,
    ) -> Result<Committed> {
        self.migrate_definition_internal(lease, plan, actor, clock, |_| {})
    }
    fn historical_snapshot(&mut self, id: &str, revision: u64) -> Result<Snapshot> {
        self.read(id, |r| {
            Ok(r.engine.at_revision(revision)?.snapshot().clone())
        })
    }
}

impl SqliteRunStore {
    pub(crate) fn migrate_definition_internal(
        &mut self,
        lease: &Lease,
        plan: &MigrationPlan,
        actor: &str,
        clock: &dyn Clock,
        hook: impl Fn(&str),
    ) -> Result<Committed> {
        workflow_worker::to_message(plan)?;
        validate_id(actor)?;
        if plan.run_id != lease.run_id {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "migration and lease name different runs",
            ));
        }
        let event_id = migration_event_id(&plan.request.migration_id)?;
        hook("before_transaction");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut current = crate::recovery::recover(&tx, &lease.run_id, self.artifacts.as_deref())?;
        let (mut authority, records) =
            crate::execution::read(&tx, &current, self.artifacts.as_deref())?;
        if let Some(previous) = current.events.iter().find(|e| e.event.event_id == event_id) {
            let proof = records
                .iter()
                .find_map(|r| match &r.action {
                    ExecutionAction::Migrated { migration } if migration.event_id == event_id => {
                        Some(migration)
                    }
                    _ => None,
                })
                .ok_or_else(|| corrupt("migration event has no execution authority"))?;
            if previous.event.kind
                != (EventKind::MigrateDefinition {
                    plan: Box::new(plan.clone()),
                })
                || proof.actor != actor
            {
                return Err(Error::new(
                    ErrorCode::ReceiptConflict,
                    "migration identity names different content or actor",
                ));
            }
            let snapshot = current.engine.snapshot().clone();
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
        authority.check_live(lease, now)?;
        if now < current.engine.snapshot().now_unix_ms {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "migration clock precedes the source state",
            ));
        }
        eligible(&authority)?;
        if &current.engine.plan_migration(&plan.request)? != plan {
            return Err(Error::new(
                ErrorCode::TransitionRejected,
                "migration plan is stale or its impact was modified",
            ));
        }
        let target = CompiledBundle::compile(plan.request.target_bundle.clone())?;
        retained_bundle(&tx, &target, true)?;
        hook("target_retained");
        let pending: Vec<_> = current
            .outbox
            .iter()
            .filter(|e| e.receipt.is_none())
            .cloned()
            .collect();
        let plan_digest = digest(plan)?;
        let migration = MigrationAuthority {
            epoch: lease.epoch,
            generation: migration_generation(&lease.run_id, &event_id, &plan_digest)?,
            actor: actor.into(),
            plan_digest,
            source_revision: plan.source_revision,
            event_id: event_id.clone(),
            event_revision: plan.source_revision + 1,
            previous_delivery_sequence: pending
                .first()
                .map_or(current.outbox.len() as u64, |e| e.sequence - 1),
            retired_commands: pending.iter().map(|e| e.command_id.clone()).collect(),
            at_unix_ms: now,
        };
        // Check all ownership/quiescence constraints before writing any event.
        let mut preview = authority.clone();
        preview.apply(&ExecutionAction::Migrated {
            migration: Box::new(migration.clone()),
        })?;
        let event = Event {
            event_id,
            run_id: plan.run_id.clone(),
            run_digest: plan.run_digest.clone(),
            expected_revision: plan.source_revision,
            at_unix_ms: now,
            kind: EventKind::MigrateDefinition {
                plan: Box::new(plan.clone()),
            },
        };
        let committed = crate::writes::persist_event(
            &tx,
            &mut current,
            &event,
            self.artifacts.as_deref(),
            &hook,
        )?;
        for entry in pending {
            let receipt = DeliveryReceipt {
                run_id: lease.run_id.clone(),
                command_id: entry.command_id.clone(),
                command_digest: entry.command_digest.clone(),
                delivery_id: migration_delivery_id(&migration.plan_digest, entry.sequence)?,
            };
            current.outbox[entry.sequence as usize - 1] =
                crate::writes::persist_receipt(&tx, &current, &receipt)?;
        }
        hook("commands_retired");
        crate::execution::append(
            &tx,
            &mut authority,
            ExecutionAction::Migrated {
                migration: Box::new(migration),
            },
        )?;
        hook("execution_written");
        let commit_at = clock.now_unix_ms()?;
        if commit_at < now || commit_at >= lease.expires_at_unix_ms {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "migration lease expired before commit admission",
            ));
        }
        crate::image::record_admission(&self.admission, commit_at, lease.expires_at_unix_ms);
        hook("before_commit");
        tx.commit().map_err(storage)?;
        hook("after_commit");
        Ok(committed)
    }
}
