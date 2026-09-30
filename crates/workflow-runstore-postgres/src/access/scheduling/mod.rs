use super::*;
use std::collections::{BTreeMap, BTreeSet};
mod admission;
mod control;
mod routing;
pub(super) use admission::{AdmissionSubject, finish, reserve, worker_ready};
pub(super) use control::park;
pub use control::{
    DeadLetter, DeadLetterRecord, DeadLetterResolution, ReplayReceipt, WorkerStatus,
};
pub use routing::RoutedDispatch;

/// Limits count current execution grants and admissions in the preceding minute.
/// They cannot stop external I/O from a process that has lost its lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionLimit {
    pub concurrent: u32,
    pub per_minute: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulingPolicy {
    pub schema_version: u32,
    pub tenant: AdmissionLimit,
    pub project: AdmissionLimit,
    pub capability: AdmissionLimit,
    pub model: AdmissionLimit,
    pub worker: AdmissionLimit,
    #[serde(default)]
    pub project_limits: BTreeMap<String, AdmissionLimit>,
    /// Keys are exact capability ID@version identities.
    #[serde(default)]
    pub capability_limits: BTreeMap<String, AdmissionLimit>,
    #[serde(default)]
    pub model_limits: BTreeMap<String, AdmissionLimit>,
    /// Maps frozen model-policy ID@version to an operator-owned provider pool.
    /// Unmapped policies use their own exact identity as their quota pool.
    #[serde(default)]
    pub model_pools: BTreeMap<String, String>,
    pub allowed_worker_versions: BTreeSet<String>,
    pub heartbeat_ms: u64,
    pub priority_step_ms: u64,
    pub max_active_runs_per_project: u32,
}
fn invalid_policy() -> Error {
    Error::new(
        ErrorCode::InvalidRequest,
        "invalid cluster scheduling policy",
    )
}
fn reference_key(id: &str, version: &str) -> String {
    format!("{id}@{version}")
}
fn exact_key(key: &str) -> bool {
    key.split_once('@').is_some_and(|(id, version)| {
        workflow_validator::identifier(id) && workflow_validator::pinned_version(version)
    })
}
impl SchedulingPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || !(1000..=300000).contains(&self.heartbeat_ms)
            || !(1..=60000).contains(&self.priority_step_ms)
            || !(1..=10000).contains(&self.max_active_runs_per_project)
            || self.allowed_worker_versions.is_empty()
            || self.allowed_worker_versions.len() > 32
            || self
                .allowed_worker_versions
                .iter()
                .any(|v| !workflow_validator::pinned_version(v))
        {
            return Err(invalid_policy());
        }
        for limits in [
            &self.project_limits,
            &self.capability_limits,
            &self.model_limits,
        ] {
            if limits.len() > 256 {
                return Err(invalid_policy());
            }
        }
        for key in self.project_limits.keys() {
            validate_id(key)?;
        }
        for key in self.model_limits.keys() {
            if !exact_key(key) {
                validate_id(key)?;
            }
        }
        if self.capability_limits.keys().any(|k| !exact_key(k))
            || self.model_pools.len() > 256
            || self.model_pools.keys().any(|k| !exact_key(k))
        {
            return Err(invalid_policy());
        }
        for pool in self.model_pools.values() {
            validate_id(pool)?;
        }
        for limit in [
            &self.tenant,
            &self.project,
            &self.capability,
            &self.model,
            &self.worker,
        ]
        .into_iter()
        .chain(self.project_limits.values())
        .chain(self.capability_limits.values())
        .chain(self.model_limits.values())
        {
            if !(1..=10000).contains(&limit.concurrent) || !(1..=100000).contains(&limit.per_minute)
            {
                return Err(invalid_policy());
            }
        }
        Ok(())
    }
}
fn exists(tx: &mut Transaction<'_>) -> Result<bool> {
    Ok(tx
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_scheduling')",
            &[],
        )
        .map_err(storage)?
        .get(0))
}
pub(in crate::access) fn fence_restored(tx: &mut Transaction<'_>) -> Result<()> {
    if exists(tx)? {
        tx.batch_execute("UPDATE workflow_scheduling.admissions SET finished=true WHERE NOT finished; UPDATE workflow_scheduling.workers SET draining=true;").map_err(storage)?;
    }
    Ok(())
}
pub(in crate::access) fn configured(tx: &mut Transaction<'_>) -> Result<bool> {
    if !exists(tx)? {
        return Ok(false);
    }
    Ok(tx
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM workflow_scheduling.tenants)",
            &[],
        )
        .map_err(storage)?
        .get(0))
}
fn policy(tx: &mut Transaction<'_>, tenant: &str, lock: bool) -> Result<Option<SchedulingPolicy>> {
    if !exists(tx)? {
        return Ok(None);
    }
    let version: i32 = tx
        .query_one(
            "SELECT version FROM workflow_scheduling.schema_version WHERE singleton=true",
            &[],
        )
        .map_err(storage)?
        .get(0);
    if version != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "unsupported scheduling schema",
        ));
    }
    let sql = if lock {
        "SELECT policy FROM workflow_scheduling.tenants WHERE tenant=$1 FOR UPDATE"
    } else {
        "SELECT policy FROM workflow_scheduling.tenants WHERE tenant=$1"
    };
    tx.query_opt(sql, &[&tenant])
        .map_err(storage)?
        .map(|r| {
            let p: SchedulingPolicy =
                serde_json::from_str(r.get(0)).map_err(|_| corrupt("invalid scheduling policy"))?;
            p.validate()?;
            Ok(p)
        })
        .transpose()
}
fn required(tx: &mut Transaction<'_>, who: &Identity) -> Result<SchedulingPolicy> {
    policy(tx, &who.tenant, false)?.ok_or_else(|| {
        Error::new(
            ErrorCode::UnsupportedStorage,
            "cluster scheduling is not configured",
        )
    })
}
pub(in crate::access) fn configuration_guard(tx: &mut Transaction<'_>, tenant: &str) -> Result<()> {
    tx.query_one(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended($1,57465235))",
        &[&tenant],
    )
    .map_err(storage)?;
    Ok(())
}
pub(crate) fn sync(
    tx: &mut Transaction<'_>,
    tenant: &str,
    project: &str,
    snapshot: &Snapshot,
    activating: bool,
) -> Result<()> {
    let Some(_) = policy(tx, tenant, false)? else {
        return Ok(());
    };
    let at = now(tx)?;
    let active = matches!(snapshot.status, RunStatus::Running | RunStatus::Cancelling)
        && snapshot.pause.is_none();
    let present = tx.query_opt("SELECT active FROM workflow_scheduling.queue WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&tenant,&project,&snapshot.run_id]).map_err(storage)?;
    if activating && active && present.as_ref().is_none_or(|r| !r.get::<_, bool>(0)) {
        // Serialize admission of new active runs without taking any other run lock.
        let p = policy(tx, tenant, true)?.ok_or_else(invalid_policy)?;
        let count:i64=tx.query_one("SELECT count(*) FROM workflow_scheduling.queue WHERE tenant=$1 AND project=$2 AND active", &[&tenant,&project]).map_err(storage)?.get(0);
        if count >= i64::from(p.max_active_runs_per_project) {
            return Err(Error::new(ErrorCode::Busy, "project run queue is full"));
        }
    }
    tx.execute("INSERT INTO workflow_scheduling.queue(tenant,project,run_id,created_at,selected_at,revision,active) VALUES($1,$2,$3,$4,$4,$5,$6) ON CONFLICT(tenant,project,run_id) DO UPDATE SET revision=EXCLUDED.revision,active=EXCLUDED.active", &[&tenant,&project,&snapshot.run_id,&at,&(snapshot.revision as i64),&active]).map_err(storage)?;
    Ok(())
}
impl AuthenticatedService {
    /// Trusted-host configuration, absent from RPC. Compare-and-set prevents
    /// concurrent operators from silently replacing each other's quota policy.
    pub fn configure_scheduling(
        client: &mut Client,
        tenant: &str,
        expected_revision: Option<u64>,
        p: &SchedulingPolicy,
    ) -> Result<u64> {
        validate_id(tenant)?;
        p.validate()?;
        let mut tx = client.transaction().map_err(storage)?;
        PostgresRunStore::transaction_settings(&mut tx)?;
        check_schema(&mut tx)?;
        let access_version:i32=tx.query_one("SELECT version FROM workflow_access.schema_version WHERE singleton=true FOR UPDATE", &[]).map_err(storage)?.get(0);
        if access_version < 2 {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "migrate access roles before configuring cluster scheduling",
            ));
        }
        tx.query_one("SELECT pg_advisory_xact_lock(57465235)", &[])
            .map_err(storage)?;
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1,57465235))",
            &[&tenant],
        )
        .map_err(storage)?;
        effects::initialize(&mut tx)?;
        if !exists(&mut tx)? {
            tx.batch_execute(include_str!("schema.sql"))
                .map_err(storage)?;
        }
        let old: Option<i64> = tx
            .query_opt(
                "SELECT revision FROM workflow_scheduling.tenants WHERE tenant=$1 FOR UPDATE",
                &[&tenant],
            )
            .map_err(storage)?
            .map(|r| r.get(0));
        if old.map(|r| r as u64) != expected_revision {
            return Err(Error::new(
                ErrorCode::BindingConflict,
                "scheduling policy revision differs",
            ));
        }
        if old.is_none() && tx.query_one("SELECT EXISTS(SELECT 1 FROM workflow_access.assignments WHERE tenant=$1 AND NOT settled AND expires_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint) OR EXISTS(SELECT 1 FROM workflow_effect_dispatch.assignments WHERE tenant=$1 AND NOT settled AND expires_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint)", &[&tenant]).map_err(storage)?.get::<_,bool>(0) {
            return Err(Error::new(ErrorCode::Busy,"drain existing execution grants before activating scheduling policy"));
        }
        let revision = old.unwrap_or(0).checked_add(1).ok_or_else(invalid_policy)?;
        let json = serde_json::to_string(p).map_err(|_| invalid_policy())?;
        tx.execute("INSERT INTO workflow_scheduling.tenants VALUES($1,$2,$3) ON CONFLICT(tenant) DO UPDATE SET revision=EXCLUDED.revision,policy=EXCLUDED.policy", &[&tenant,&revision,&json]).map_err(storage)?;
        // Older network API binaries open/check the access schema per request.
        // They must fail closed instead of serving a tenant without its quotas.
        tx.batch_execute(
            "UPDATE workflow_access.schema_version SET version=3 WHERE singleton=true",
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(revision as u64)
    }
}
