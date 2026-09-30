use super::*;
use std::collections::BTreeSet;

const MAX_RESTORE_RUNS: usize = 1000;
const MAX_RESTORE_SCOPES: usize = 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseRestoreRequest {
    /// Must name the isolated destination database, never a live source.
    pub database: String,
    pub backup_digest: String,
    pub actor: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatabaseRestoreReport {
    pub database: String,
    pub backup_digest: String,
    pub generation: String,
    pub restored_at_unix_ms: i64,
    pub runs: usize,
    pub scopes: usize,
    pub revoked_credentials: u64,
    pub fenced_assignments: u64,
    pub fenced_effect_assignments: u64,
}

#[derive(Debug)]
pub struct RestoredAdministrator {
    pub tenant: String,
    pub project: String,
    pub credential: IssuedCredential,
}

/// Only the explicit private delivery channel should expose the new bearers.
#[derive(Debug)]
pub struct RestoredDatabase {
    pub report: DatabaseRestoreReport,
    pub administrators: Vec<RestoredAdministrator>,
}

impl AuthenticatedService {
    pub fn recovery_barrier(&mut self, token: &str, run: &str) -> Result<Option<RecoveryBarrier>> {
        self.transact(
            token,
            operations::READ,
            "recovery_barrier",
            run,
            |tx, who| who.read(tx, run, |store| store.recovery_barrier(run)),
        )
    }

    /// Reconcile an actual source intent/provider receipt lost after the backup.
    /// A recovery credential and its live lease are required; the authenticated
    /// actor replaces any actor supplied in the imported document.
    pub fn import_restored_effect(
        &mut self,
        token: &str,
        lease: &Lease,
        import: &RestoredEffect,
    ) -> Result<Committed> {
        self.transact(
            token,
            &[Role::Recovery],
            "import_restored_effect",
            &lease.run_id,
            |tx, who| {
                if lease.owner != who.id {
                    return Err(denied());
                }
                let mut import = import.clone();
                import.resolution.actor = who.actor.clone();
                who.fence_execution(
                    lease.issued_at_unix_ms as i64,
                    lease.expires_at_unix_ms as i64,
                );
                who.change(tx, &lease.run_id, false, |store, clock| {
                    effects::authority(store, &lease.run_id)?
                        .check_live(lease, clock.now_unix_ms()?)?;
                    store.import_restored_effect(lease, &import, clock)
                })
            },
        )
    }

    /// Trusted offline deployment operation, deliberately absent from the RPC
    /// protocol. Run only against an isolated restored database before exposing
    /// its service. One transaction verifies all run/artifact dependencies,
    /// changes ownership, holds writes and replaces every retained credential.
    /// A failure rolls back all of those changes. Repeating creates a new held
    /// generation and revokes the previous recovery credentials as well.
    pub fn fence_restored_database(
        client: &mut Client,
        request: &DatabaseRestoreRequest,
    ) -> Result<RestoredDatabase> {
        validate_id(&request.actor)?;
        if request.database.is_empty()
            || request.database.len() > 63
            || request.reason.trim().is_empty()
            || request.reason.len() > 1024
            || request.reason.contains('\0')
            || !request
                .backup_digest
                .strip_prefix("sha256:")
                .is_some_and(|s| {
                    s.len() == 64
                        && s.bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                })
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "invalid database restoration request",
            ));
        }
        let mut tx = client.transaction().map_err(storage)?;
        PostgresRunStore::transaction_settings(&mut tx)?;
        let database: String = tx
            .query_one("SELECT current_database()", &[])
            .map_err(storage)?
            .get(0);
        if database != request.database {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "restore destination database differs from request",
            ));
        }
        check_schema(&mut tx)?;
        // The destination must be offline. These locks also make any accidental
        // concurrent admission wait until its credential has been revoked.
        tx.batch_execute(
            "LOCK TABLE workflow_access.credentials, workflow_access.assignments,
            workflow_access.published_bundles, workflow_access.audit,
            workflow_authority.runs, workflow_authority.bindings IN ACCESS EXCLUSIVE MODE",
        )
        .map_err(storage)?;
        let authority_version: i32 = tx
            .query_one(
                "SELECT version FROM workflow_authority.schema_version WHERE singleton=true",
                &[],
            )
            .map_err(storage)?
            .get(0);
        if authority_version != 1 {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "unsupported restored authority schema",
            ));
        }
        let artifacts = artifact_catalog::exists(&mut tx)?;
        if artifacts {
            artifact_catalog::check(&mut tx)?;
            tx.batch_execute(
                "LOCK TABLE workflow_artifacts.artifacts, workflow_artifacts.uploads,
                workflow_artifacts.chunks, workflow_artifacts.downloads IN ACCESS EXCLUSIVE MODE",
            )
            .map_err(storage)?;
        }
        let effects: bool = tx.query_one("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_effect_dispatch')", &[]).map_err(storage)?.get(0);
        if effects {
            effects::check(&mut tx)?;
            tx.batch_execute(
                "LOCK TABLE workflow_effect_dispatch.assignments IN ACCESS EXCLUSIVE MODE",
            )
            .map_err(storage)?;
        }
        let runs = tx.query("SELECT tenant,project,run_id FROM workflow_authority.runs ORDER BY tenant,project,run_id LIMIT 1001", &[]).map_err(storage)?;
        let scope_rows = tx.query("SELECT tenant,project FROM workflow_access.credentials UNION SELECT tenant,project FROM workflow_authority.runs UNION SELECT tenant,project FROM workflow_access.published_bundles LIMIT 1001", &[]).map_err(storage)?;
        if runs.len() > MAX_RESTORE_RUNS || scope_rows.len() > MAX_RESTORE_SCOPES {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "restore exceeds 1000 runs or 1000 scopes",
            ));
        }
        let scopes: BTreeSet<(String, String)> =
            scope_rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        let generation = random("sha256:")?;
        let at = now(&mut tx)?;
        let revoked_credentials = tx
            .execute(
                "UPDATE workflow_access.credentials SET revoked=true WHERE revoked=false",
                &[],
            )
            .map_err(storage)?;
        let fenced_assignments = tx
            .execute(
                "UPDATE workflow_access.assignments SET settled=true WHERE settled=false",
                &[],
            )
            .map_err(storage)?;
        let fenced_effect_assignments = if effects {
            tx.execute(
                "UPDATE workflow_effect_dispatch.assignments SET settled=true WHERE settled=false",
                &[],
            )
            .map_err(storage)?
        } else {
            0
        };
        let mut administrators = vec![];
        scheduling::fence_restored(&mut tx)?;
        for (tenant, project) in scopes {
            let credential = issue(
                &mut tx,
                &tenant,
                &project,
                &request.actor,
                Role::Administrator,
                &[],
                MAX_TTL,
            )?;
            let who = authenticate(&mut tx, credential.expose_secret())?;
            for row in runs
                .iter()
                .filter(|r| r.get::<_, &str>(0) == tenant && r.get::<_, &str>(1) == project)
            {
                let run: &str = row.get(2);
                who.change(&mut tx, run, false, |store, clock| {
                    let barriers = store.fence_restored_runs(
                        &request.backup_digest,
                        &generation,
                        &request.actor,
                        &request.reason,
                        clock,
                    )?;
                    if barriers.len() != 1 || barriers[0].0 != run {
                        return Err(corrupt("restored aggregate contains a different run"));
                    }
                    store.verify(run)?;
                    Ok(())
                })?;
                audit(&mut tx, &who, "restore_run", run, "held")?;
            }
            audit(
                &mut tx,
                &who,
                "restore_database",
                &request.backup_digest,
                "held",
            )?;
            live(&mut tx, &who)?;
            administrators.push(RestoredAdministrator {
                tenant,
                project,
                credential,
            });
        }
        let report = DatabaseRestoreReport {
            database,
            backup_digest: request.backup_digest.clone(),
            generation,
            restored_at_unix_ms: at,
            runs: runs.len(),
            scopes: administrators.len(),
            revoked_credentials,
            fenced_assignments,
            fenced_effect_assignments,
        };
        for administrator in &administrators {
            authenticate(&mut tx, administrator.credential.expose_secret())?;
        }
        tx.commit().map_err(storage)?;
        Ok(RestoredDatabase {
            report,
            administrators,
        })
    }
}
