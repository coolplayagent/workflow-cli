use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerStatus {
    pub worker_id: String,
    pub runtime_version: String,
    pub heartbeat_at_unix_ms: i64,
    pub draining: bool,
    pub active_assignments: u64,
    pub heartbeat_timeout_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeadLetter {
    pub id: String,
    pub run_id: String,
    pub revision: u64,
    pub snapshot_digest: String,
    pub reason: String,
    pub at_unix_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeadLetterResolution {
    pub id: String,
    pub expected_revision: u64,
    pub expected_snapshot_digest: String,
    /// Retry resumes ordinary dispatch; false archives without changing the run.
    pub retry: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayReceipt {
    pub actor: String,
    pub review: DeadLetterResolution,
    pub resolved_at_unix_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeadLetterRecord {
    pub letter: DeadLetter,
    pub resolution: Option<ReplayReceipt>,
}
fn row_letter(r: postgres::Row) -> DeadLetter {
    DeadLetter {
        id: r.get(0),
        run_id: r.get(1),
        revision: r.get::<_, i64>(2) as u64,
        snapshot_digest: r.get(3),
        reason: r.get(4),
        at_unix_ms: r.get(5),
    }
}
pub(super) fn active_letter(
    tx: &mut Transaction<'_>,
    who: &Identity,
    run: &str,
) -> Result<Option<DeadLetter>> {
    Ok(tx.query_opt("SELECT id,run_id,revision,snapshot_digest,reason,at_unix_ms FROM workflow_scheduling.dead_letters WHERE tenant=$1 AND project=$2 AND run_id=$3 AND NOT resolved", &[&who.tenant,&who.project,&run]).map_err(storage)?.map(row_letter))
}
pub(in crate::access) fn park(
    tx: &mut Transaction<'_>,
    who: &Identity,
    run: &str,
    reason: &str,
) -> Result<Option<DeadLetter>> {
    if policy(tx, &who.tenant, false)?.is_none() {
        return Ok(None);
    }
    let snapshot = who.read(tx, run, |s| s.get(run))?;
    if let Some(old) = active_letter(tx, who, run)? {
        return Ok(Some(old));
    }
    let letter = DeadLetter {
        id: random("dead-letter-")?,
        run_id: run.into(),
        revision: snapshot.revision,
        snapshot_digest: workflow_worker::digest(&snapshot)?,
        reason: reason.into(),
        at_unix_ms: now(tx)?,
    };
    tx.execute("INSERT INTO workflow_scheduling.dead_letters(id,tenant,project,run_id,revision,snapshot_digest,reason,at_unix_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8)", &[&letter.id,&who.tenant,&who.project,&run,&(letter.revision as i64),&letter.snapshot_digest,&letter.reason,&letter.at_unix_ms]).map_err(storage)?;
    audit(tx, who, "dead_letter_created", &letter.id, "accepted")?;
    Ok(Some(letter))
}
fn status(tx: &mut Transaction<'_>, who: &Identity, worker: &str) -> Result<WorkerStatus> {
    let p = required(tx, who)?;
    let r=tx.query_opt("SELECT runtime_version,heartbeat_at,draining FROM workflow_scheduling.workers WHERE tenant=$1 AND project=$2 AND worker_id=$3", &[&who.tenant,&who.project,&worker]).map_err(storage)?.ok_or_else(denied)?;
    let at = now(tx)?;
    let count:i64=tx.query_one("SELECT count(*) FROM workflow_scheduling.admissions WHERE tenant=$1 AND project=$2 AND worker_id=$3 AND NOT finished AND expires_at>$4", &[&who.tenant,&who.project,&worker,&at]).map_err(storage)?.get(0);
    Ok(WorkerStatus {
        worker_id: worker.into(),
        runtime_version: r.get(0),
        heartbeat_at_unix_ms: r.get(1),
        draining: r.get(2),
        active_assignments: count as u64,
        heartbeat_timeout_ms: p.heartbeat_ms,
    })
}
impl AuthenticatedService {
    pub fn dead_letter(&mut self, token: &str, id: &str) -> Result<DeadLetterRecord> {
        self.transact(token,&[Role::Administrator,Role::Scheduler,Role::Recovery],"dead_letter",id,|tx,who| {
            required(tx,who)?;
            let row=tx.query_opt("SELECT id,run_id,revision,snapshot_digest,reason,at_unix_ms,resolution_details,resolved,resolution FROM workflow_scheduling.dead_letters WHERE tenant=$1 AND project=$2 AND id=$3", &[&who.tenant,&who.project,&id]).map_err(storage)?.ok_or_else(denied)?;
            let detail:Option<String>=row.get(6);
            let resolution:Option<ReplayReceipt>=detail.map(|v|serde_json::from_str(&v)).transpose().map_err(|_|corrupt("dead letter receipt invalid"))?;
            if row.get::<_,bool>(7)!=resolution.is_some() || resolution.as_ref().is_some_and(|r| workflow_worker::digest(&(&r.review,&r.actor)).ok()!=row.get::<_,Option<String>>(8)) {
                return Err(corrupt("dead letter resolution binding differs"));
            }
            Ok(DeadLetterRecord{letter:row_letter(row),resolution})
        })
    }
    /// Renewal preserves the ownership epoch and updates stored assignment
    /// lease identities atomically. Frozen task/call deadlines never increase.
    pub fn renew(&mut self, token: &str, lease: &Lease, ttl_ms: u64) -> Result<Lease> {
        self.transact(token,&[Role::Scheduler],"renew",&lease.run_id,|tx,who| {
            required(tx,who)?;
            if lease.owner!=who.id {return Err(denied());}
            let old=serde_json::to_string(lease).map_err(|_|invalid_policy())?;
            // Match worker completion's assignment-before-run lock order.
            tx.query("SELECT id FROM workflow_access.assignments WHERE tenant=$1 AND project=$2 AND run_id=$3 AND lease::jsonb=$4::text::jsonb ORDER BY id FOR UPDATE", &[&who.tenant,&who.project,&lease.run_id,&old]).map_err(storage)?;
            tx.query("SELECT id FROM workflow_effect_dispatch.assignments WHERE tenant=$1 AND project=$2 AND run_id=$3 AND lease::jsonb=$4::text::jsonb ORDER BY id FOR UPDATE", &[&who.tenant,&who.project,&lease.run_id,&old]).map_err(storage)?;
            let renewed=who.change(tx,&lease.run_id,false,|s,c|s.renew(lease,ttl_ms,c))?;
            let new=serde_json::to_string(&renewed).map_err(|_|invalid_policy())?;
            tx.execute("UPDATE workflow_access.assignments SET lease=$5 WHERE tenant=$1 AND project=$2 AND run_id=$3 AND lease::jsonb=$4::text::jsonb", &[&who.tenant,&who.project,&lease.run_id,&old,&new]).map_err(storage)?;
            tx.execute("UPDATE workflow_effect_dispatch.assignments SET lease=$5 WHERE tenant=$1 AND project=$2 AND run_id=$3 AND lease::jsonb=$4::text::jsonb", &[&who.tenant,&who.project,&lease.run_id,&old,&new]).map_err(storage)?;
            Ok(renewed)
        })
    }
    /// Heartbeats never extend task deadlines or resurrect expired lease epochs.
    /// Drain is sticky until an administrator explicitly resumes this identity.
    pub fn worker_heartbeat(
        &mut self,
        token: &str,
        version: &str,
        drain: bool,
    ) -> Result<WorkerStatus> {
        self.transact(token,&[Role::Worker],"worker_heartbeat","worker",|tx,who| {
            let p=required(tx,who)?;
            if !p.allowed_worker_versions.contains(version) {return Err(denied());}
            let at=now(tx)?;
            tx.execute("INSERT INTO workflow_scheduling.workers VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(tenant,project,worker_id) DO UPDATE SET runtime_version=EXCLUDED.runtime_version,heartbeat_at=EXCLUDED.heartbeat_at,draining=workflow_scheduling.workers.draining OR EXCLUDED.draining", &[&who.tenant,&who.project,&who.id,&version,&at,&drain]).map_err(storage)?;
            status(tx,who,&who.id)
        })
    }
    pub fn control_worker(
        &mut self,
        token: &str,
        worker: &str,
        drain: bool,
        reason: &str,
    ) -> Result<WorkerStatus> {
        self.transact(token,&[Role::Administrator],"control_worker",worker,|tx,who| {
            required(tx,who)?;
            if reason.trim().is_empty() || reason.len()>1024 {return Err(invalid_policy());}
            let changed=tx.execute("UPDATE workflow_scheduling.workers SET draining=$4 WHERE tenant=$1 AND project=$2 AND worker_id=$3", &[&who.tenant,&who.project,&worker,&drain]).map_err(storage)?;
            if changed!=1 {return Err(denied());}
            // The reason is retained as a digest in audit, not arbitrary secret text.
            audit(tx,who,if drain {"worker_draining"}else{"worker_resumed"},worker,&workflow_worker::digest(&reason)?)?;
            status(tx,who,worker)
        })
    }
    pub fn worker_status(&mut self, token: &str, worker: &str) -> Result<WorkerStatus> {
        self.transact(
            token,
            &[Role::Administrator, Role::Scheduler, Role::Recovery],
            "worker_status",
            worker,
            |tx, who| {
                required(tx, who)?;
                status(tx, who, worker)
            },
        )
    }
    pub fn set_priority(&mut self, token: &str, run: &str, priority: u8) -> Result<()> {
        self.transact(token,&[Role::Runner,Role::Recovery],"set_priority",run,|tx,who| {
            required(tx,who)?;
            if priority>9 {return Err(invalid_policy());}
            let snapshot=who.read(tx,run,|s|s.get(run))?;
            sync(tx,&who.tenant,&who.project,&snapshot,false)?;
            tx.execute("UPDATE workflow_scheduling.queue SET priority=$4 WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&who.tenant,&who.project,&run,&i32::from(priority)]).map_err(storage)?;
            Ok(())
        })
    }
    /// Selection updates only a scheduling hint. Every actual dispatch still
    /// needs the run's database-time lease and exact current node instance.
    pub fn schedule_candidates(&mut self, token: &str, limit: u32) -> Result<Vec<RunSummary>> {
        self.transact(token,&[Role::Scheduler],"schedule_candidates","runs",|tx,who| {
            validate_limit(limit)?;
            let p=required(tx,who)?;
            let rows=tx.query("SELECT r.run_id FROM workflow_authority.runs r LEFT JOIN workflow_scheduling.queue q USING(tenant,project,run_id) WHERE r.tenant=$1 AND r.project=$2 AND (q.active IS NULL OR q.active) AND NOT EXISTS(SELECT 1 FROM workflow_scheduling.dead_letters d WHERE d.tenant=r.tenant AND d.project=r.project AND d.run_id=r.run_id AND NOT d.resolved) ORDER BY COALESCE(q.selected_at,0)+(9-COALESCE(q.priority,0))*$3::bigint,r.run_id COLLATE \"C\" LIMIT $4", &[&who.tenant,&who.project,&(p.priority_step_ms as i64),&i64::from(limit)]).map_err(storage)?;
            let mut results=vec![];
            for row in rows {
                let run:String=row.get(0);
                let s=who.read(tx,&run,|store|store.get(&run))?;
                sync(tx,&who.tenant,&who.project,&s,false)?;
                let at=now(tx)?;
                tx.execute("UPDATE workflow_scheduling.queue SET selected_at=$4 WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&who.tenant,&who.project,&run,&at]).map_err(storage)?;
                if matches!(s.status,RunStatus::Running|RunStatus::Cancelling) && s.pause.is_none() {
                    results.push(RunSummary{run_id:s.run_id,revision:s.revision,status:s.status,pause:s.pause,bundle_digest:s.bundle_digest});
                }
            }
            Ok(results)
        })
    }
    pub fn dead_letters(
        &mut self,
        token: &str,
        after: &str,
        limit: u32,
    ) -> Result<Page<DeadLetter, String>> {
        self.transact(token,&[Role::Administrator,Role::Scheduler,Role::Recovery],"dead_letters","dead-letters",|tx,who| {
            required(tx,who)?; validate_limit(limit)?;
            if !after.is_empty() {validate_id(after)?;}
            let rows=tx.query("SELECT id,run_id,revision,snapshot_digest,reason,at_unix_ms FROM workflow_scheduling.dead_letters WHERE tenant=$1 AND project=$2 AND NOT resolved AND id COLLATE \"C\">$3 COLLATE \"C\" ORDER BY id COLLATE \"C\" LIMIT $4", &[&who.tenant,&who.project,&after,&(i64::from(limit)+1)]).map_err(storage)?;
            let mut items:Vec<_>=rows.into_iter().map(row_letter).collect();
            let next_cursor=if items.len()>limit as usize {items.pop();items.last().map(|d|d.id.clone())}else{None};
            Ok(Page{items,next_cursor})
        })
    }
    pub fn resolve_dead_letter(
        &mut self,
        token: &str,
        resolution: &DeadLetterResolution,
    ) -> Result<()> {
        self.transact(token,&[Role::Recovery],"resolve_dead_letter",&resolution.id,|tx,who| {
            required(tx,who)?;
            if resolution.reason.trim().is_empty() || resolution.reason.len()>1024 {return Err(invalid_policy());}
            // Lock the run before the dead-letter row, matching dispatch order.
            let row=tx.query_opt("SELECT run_id FROM workflow_scheduling.dead_letters WHERE tenant=$1 AND project=$2 AND id=$3", &[&who.tenant,&who.project,&resolution.id]).map_err(storage)?.ok_or_else(denied)?;
            let run:String=row.get(0);
            let s=who.read(tx,&run,|store|store.get(&run))?;
            let row=tx.query_one("SELECT revision,snapshot_digest,resolved,resolution FROM workflow_scheduling.dead_letters WHERE id=$1 FOR UPDATE", &[&resolution.id]).map_err(storage)?;
            let digest=workflow_worker::digest(&(resolution,&who.actor))?;
            if row.get::<_,bool>(2) {
                return if row.get::<_,Option<String>>(3)==Some(digest) {Ok(())}else{Err(Error::new(ErrorCode::ReceiptConflict,"dead letter already resolved"))};
            }
            if s.revision!=resolution.expected_revision || resolution.expected_snapshot_digest!=workflow_worker::digest(&s)? || (resolution.retry && !matches!(s.status,RunStatus::Running|RunStatus::Cancelling)) {
                return Err(Error::new(ErrorCode::ReceiptConflict,"dead letter no longer matches current run"));
            }
            let receipt=ReplayReceipt{actor:who.actor.clone(),review:resolution.clone(),resolved_at_unix_ms:now(tx)?};
            let details=serde_json::to_string(&receipt).map_err(|_|invalid_policy())?;
            tx.execute("UPDATE workflow_scheduling.dead_letters SET resolved=true,resolution=$2,resolution_details=$3 WHERE id=$1", &[&resolution.id,&digest,&details]).map_err(storage)?;
            // Archiving a live letter must not accidentally authorize a retry.
            if !resolution.retry && matches!(s.status,RunStatus::Running|RunStatus::Cancelling) {
                return Err(Error::new(ErrorCode::InvalidRequest,"cancel or complete the run before archiving"));
            }
            audit(tx,who,if resolution.retry {"dead_letter_replayed"}else{"dead_letter_archived"},&resolution.id,&digest)?;
            Ok(())
        })
    }
}
