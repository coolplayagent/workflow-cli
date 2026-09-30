use super::*;

fn published_target(tx: &mut Transaction<'_>, who: &Identity, bundle: &BundleSpec) -> Result<()> {
    let compiled = workflow_kernel::CompiledBundle::compile(bundle.clone())?;
    if tx.query_opt("SELECT digest FROM workflow_access.published_bundles WHERE tenant=$1 AND project=$2 AND digest=$3 FOR SHARE", &[&who.tenant,&who.project,&compiled.digest()]).map_err(storage)?.is_none() {
        return Err(denied());
    }
    bind_versions(
        tx,
        &who.tenant,
        &who.project,
        workflow_runstore_sqlite::bundle_bindings(&compiled)?,
        false,
    )
}

impl AuthenticatedService {
    /// Administrative preflight for a previous transaction-image schema. The
    /// conversion runs only in a volatile reducer and retains original history.
    pub fn plan_storage_upgrade(
        &mut self,
        token: &str,
        run: &str,
    ) -> Result<workflow_runstore_sqlite::StorageUpgrade> {
        self.transact(token, &[Role::Administrator], "plan_storage_upgrade", run, |tx, who| {
            let row = tx.query_opt("SELECT image,image_digest FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3 FOR SHARE", &[&who.tenant,&who.project,&run]).map_err(storage)?.ok_or_else(|| Error::new(ErrorCode::NotFound, "run not found"))?;
            let bytes: Vec<u8> = row.get(0);
            if hash(&bytes) != row.get::<_, String>(1) { return Err(corrupt("source image digest mismatch")); }
            let reader = artifact_catalog::load(tx, who, run)?;
            let (image, report) = RunImage::upgrade_previous(&bytes, reader)?;
            if image.run_id() != run { return Err(corrupt("source image identity mismatch")); }
            bind_versions(tx, &who.tenant, &who.project, image.bindings()?, false)?;
            Ok(report)
        })
    }

    /// One aggregate commits all-or-none. Reviewable source-byte CAS prevents
    /// a plan from upgrading a different snapshot during a rolling deployment.
    pub fn upgrade_storage(
        &mut self,
        token: &str,
        run: &str,
        plan: &workflow_runstore_sqlite::StorageUpgrade,
    ) -> Result<workflow_runstore_sqlite::StorageUpgrade> {
        self.transact(token, &[Role::Administrator], "upgrade_storage", run, |tx, who| {
            let row = tx.query_opt("SELECT image,image_digest FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3 FOR UPDATE", &[&who.tenant,&who.project,&run]).map_err(storage)?.ok_or_else(|| Error::new(ErrorCode::NotFound, "run not found"))?;
            let bytes: Vec<u8> = row.get(0);
            if hash(&bytes) != row.get::<_, String>(1) { return Err(corrupt("source image digest mismatch")); }
            let reader = artifact_catalog::load(tx, who, run)?;
            if let Ok(image) = RunImage::parse(&bytes) {
                if image.run_id() != run { return Err(corrupt("source image identity mismatch")); }
                let mut current = SqliteRunStore::from_image(&image, reader)?;
                bind_versions(tx, &who.tenant, &who.project, image.bindings()?, false)?;
                return current.storage_history()?.into_iter().find(|r| r == plan)
                    .ok_or_else(|| Error::new(ErrorCode::ReceiptConflict, "storage upgrade is already current under another plan"));
            }
            if hash(&bytes) != plan.source_digest { return Err(Error::new(ErrorCode::TransitionRejected, "storage upgrade plan is stale")); }
            let (image, report) = RunImage::upgrade_previous(&bytes, reader)?;
            if image.run_id() != run || &report != plan { return Err(corrupt("storage conversion differs from plan")); }
            bind_versions(tx, &who.tenant, &who.project, image.bindings()?, false)?;
            let bytes = image.bytes()?;
            tx.execute("UPDATE workflow_authority.runs SET image=$4,image_digest=$5,generation=generation+1 WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&who.tenant,&who.project,&run,&bytes,&hash(&bytes)]).map_err(storage)?;
            Ok(report)
        })
    }
    /// The target must already be published by a definition maintainer. Planning
    /// neither acquires ownership nor authorizes execution of the target.
    pub fn plan_migration(
        &mut self,
        token: &str,
        run: &str,
        request: &MigrationRequest,
    ) -> Result<MigrationPlan> {
        self.transact(
            token,
            &[Role::Administrator],
            "plan_migration",
            run,
            |tx, who| {
                published_target(tx, who, &request.target_bundle)?;
                who.read(tx, run, |s| s.plan_migration(run, request))
            },
        )
    }

    /// Administrative CAS under the administrator's own current lease. The
    /// authenticated actor, DB clock, bindings, run image and assignment fences
    /// commit together; a claimed actor in a client payload is never accepted.
    pub fn migrate_definition(
        &mut self,
        token: &str,
        lease: &Lease,
        plan: &MigrationPlan,
    ) -> Result<Committed> {
        self.transact(token, &[Role::Administrator], "migrate_definition", &plan.run_id, |tx, who| {
            if lease.owner != who.id || lease.run_id != plan.run_id { return Err(denied()); }
            published_target(tx, who, &plan.request.target_bundle)?;
            let result = who.change(tx, &plan.run_id, false, |s, c| s.migrate_definition(lease, plan, &who.actor, c))?;
            if !result.transition.duplicate {
                tx.execute("UPDATE workflow_access.assignments SET settled=true WHERE tenant=$1 AND project=$2 AND run_id=$3 AND NOT settled", &[&who.tenant,&who.project,&plan.run_id]).map_err(storage)?;
            }
            Ok(result)
        })
    }

    pub fn historical_snapshot(
        &mut self,
        token: &str,
        run: &str,
        revision: u64,
    ) -> Result<Snapshot> {
        let mut roles = operations::READ.to_vec();
        roles.push(Role::Administrator);
        self.transact(token, &roles, "historical_snapshot", run, |tx, who| {
            who.read(tx, run, |s| s.historical_snapshot(run, revision))
        })
    }
}
