use super::*;

fn busy() -> Error {
    Error::new(ErrorCode::Busy, "cluster execution admission deferred")
}
pub(in crate::access) fn worker_ready(
    tx: &mut Transaction<'_>,
    who: &Identity,
    worker: &Identity,
) -> Result<()> {
    let Some(p) = policy(tx, &who.tenant, false)? else {
        return Ok(());
    };
    let row=tx.query_opt("SELECT runtime_version,heartbeat_at,draining FROM workflow_scheduling.workers WHERE tenant=$1 AND project=$2 AND worker_id=$3 FOR SHARE", &[&who.tenant,&who.project,&worker.id]).map_err(storage)?.ok_or_else(busy)?;
    let at = now(tx)?;
    let heartbeat: i64 = row.get(1);
    let version: String = row.get(0);
    if row.get::<_, bool>(2)
        || !p.allowed_worker_versions.contains(&version)
        || heartbeat > at
        || at - heartbeat >= p.heartbeat_ms as i64
    {
        return Err(busy());
    }
    // Recheck the heartbeat window after the run/assignment transaction too.
    who.fence_execution(heartbeat, heartbeat + p.heartbeat_ms as i64);
    Ok(())
}
pub(in crate::access) struct AdmissionSubject<'a> {
    pub run: &'a str,
    pub capability: &'a workflow_ir::VersionRef,
    pub model: Option<&'a workflow_worker::ModelPolicyBinding>,
}
pub(in crate::access) fn reserve(
    tx: &mut Transaction<'_>,
    who: &Identity,
    worker: &Identity,
    id: &str,
    subject: AdmissionSubject<'_>,
    expires: i64,
) -> Result<()> {
    let AdmissionSubject {
        run,
        capability,
        model,
    } = subject;
    let Some(p) = policy(tx, &who.tenant, true)? else {
        return Ok(());
    };
    // Re-read version/liveness under the policy lock so a concurrently changed
    // allowlist cannot authorize a grant from an earlier policy revision.
    worker_ready(tx, who, worker)?;
    let at = now(tx)?;
    let cap = reference_key(&capability.id, &capability.version);
    let pool = model.map(|m| {
        let key = reference_key(&m.policy.id, &m.policy.version);
        p.model_pools.get(&key).cloned().unwrap_or(key)
    });
    let project = p.project_limits.get(&who.project).unwrap_or(&p.project);
    let cap_limit = p.capability_limits.get(&cap).unwrap_or(&p.capability);
    let pool_limit = pool
        .as_ref()
        .and_then(|m| p.model_limits.get(m))
        .unwrap_or(&p.model);
    // A tenant row serializes every API replica's admission decision. A sliding
    // sixty-second count also includes already completed attempts; finishing
    // early never refunds rate tokens. No global lock couples separate tenants.
    let rows=tx.query("SELECT project,worker_id,capability,model_pool,count(*) FILTER(WHERE NOT finished AND expires_at>$2),count(*) FILTER(WHERE created_at>$3) FROM workflow_scheduling.admissions WHERE tenant=$1 AND ((NOT finished AND expires_at>$2) OR created_at>$3) GROUP BY project,worker_id,capability,model_pool", &[&who.tenant,&at,&(at-60000)]).map_err(storage)?;
    let mut counts = [(0i64, 0i64); 5];
    for row in rows {
        let scopes = [
            true,
            row.get::<_, String>(0) == who.project,
            row.get::<_, String>(2) == cap,
            pool.is_some() && row.get::<_, Option<String>>(3) == pool,
            row.get::<_, String>(1) == worker.id,
        ];
        for (i, yes) in scopes.into_iter().enumerate() {
            if yes {
                counts[i].0 += row.get::<_, i64>(4);
                counts[i].1 += row.get::<_, i64>(5);
            }
        }
    }
    for (i, limit) in [&p.tenant, project, cap_limit, pool_limit, &p.worker]
        .into_iter()
        .enumerate()
    {
        if i == 3 && pool.is_none() {
            continue;
        }
        if counts[i].0 >= i64::from(limit.concurrent) || counts[i].1 >= i64::from(limit.per_minute)
        {
            return Err(busy());
        }
    }
    if tx.query_one("SELECT EXISTS(SELECT 1 FROM workflow_scheduling.dead_letters WHERE tenant=$1 AND project=$2 AND run_id=$3 AND NOT resolved)", &[&who.tenant,&who.project,&run]).map_err(storage)?.get::<_,bool>(0) {
        return Err(Error::new(ErrorCode::ManualReconciliation,"run has an unresolved dispatch dead letter"));
    }
    tx.execute("INSERT INTO workflow_scheduling.admissions(id,tenant,project,run_id,worker_id,capability,model_pool,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)", &[&id,&who.tenant,&who.project,&run,&worker.id,&cap,&pool,&at,&expires]).map_err(storage)?;
    Ok(())
}
pub(in crate::access) fn finish(tx: &mut Transaction<'_>, id: &str) -> Result<()> {
    if exists(tx)? {
        tx.execute(
            "UPDATE workflow_scheduling.admissions SET finished=true WHERE id=$1",
            &[&id],
        )
        .map_err(storage)?;
    }
    Ok(())
}
