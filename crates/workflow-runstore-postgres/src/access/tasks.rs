use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Dispatch {
    Task { assignment_id: String },
    Handled,
    Idle,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutstandingAssignment {
    pub assignment_id: String,
    pub worker_id: String,
    pub run_id: String,
    pub expires_at_unix_ms: i64,
    pub worker_revoked: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskReceipt {
    pub revision: u64,
    pub duplicate: bool,
}
pub(super) struct Assignment {
    pub(super) lease: Lease,
    pub(super) task: PreparedTask,
    pub(super) expires: i64,
    pub(super) settled: bool,
}
pub(super) fn load(tx: &mut Transaction<'_>, who: &Identity, id: &str) -> Result<Assignment> {
    let row=tx.query_opt("SELECT lease,task,expires_at,settled FROM workflow_access.assignments WHERE tenant=$1 AND project=$2 AND worker_id=$3 AND id=$4 FOR UPDATE", &[&who.tenant,&who.project,&who.id,&id]).map_err(storage)?.ok_or_else(denied)?;
    let a = Assignment {
        lease: serde_json::from_str(row.get(0)).map_err(|_| corrupt("assignment lease invalid"))?,
        task: serde_json::from_str(row.get(1)).map_err(|_| corrupt("assignment task invalid"))?,
        expires: row.get(2),
        settled: row.get(3),
    };
    who.fence(a.lease.issued_at_unix_ms as i64, a.expires);
    if !who.capabilities.iter().any(|c| c.matches(&a.task.request)) {
        return Err(denied());
    }
    live(tx, who)?;
    Ok(a)
}
pub(super) fn check_lease(
    store: &mut SqliteRunStore,
    lease: &Lease,
    task: &PreparedTask,
    now: u64,
) -> Result<()> {
    let mut authority = Authority::new(&lease.run_id, store.started_at(&lease.run_id)?);
    let mut cursor = 0;
    loop {
        let page = store.execution_history(&lease.run_id, cursor, 100)?;
        for record in page.items {
            authority.apply(&record.action)?;
        }
        match page.next_cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }
    authority.check_live(lease, now)?;
    if authority
        .attempts
        .get(&task.attempt_id)
        .is_none_or(|a| &a.prepared != task)
    {
        return Err(corrupt("assignment differs from committed attempt"));
    }
    Ok(())
}
impl AuthenticatedService {
    /// Rebuild delivery after notification/response loss. This returns only the
    /// authenticated worker's unexpired, unsettled assignment IDs. Retrieval
    /// still revalidates the exact current lease and committed attempt.
    pub fn pending(
        &mut self,
        token: &str,
        after: &str,
        limit: u32,
    ) -> Result<Page<String, String>> {
        self.transact(token, &[Role::Worker], "pending", "assignments", |tx, who| {
            validate_limit(limit)?;
            if !after.is_empty() { validate_id(after)?; }
            let at=now(tx)?;
            let rows=tx.query("SELECT id FROM workflow_access.assignments WHERE tenant=$1 AND project=$2 AND worker_id=$3 AND NOT settled AND expires_at>$4 AND id COLLATE \"C\">$5 COLLATE \"C\" ORDER BY id COLLATE \"C\" LIMIT $6", &[&who.tenant,&who.project,&who.id,&at,&after,&(i64::from(limit)+1)]).map_err(storage)?;
            let mut items:Vec<String>=rows.iter().map(|r|r.get(0)).collect();
            let next_cursor=if items.len()>limit as usize {items.pop();items.last().cloned()}else{None};
            Ok(Page{items,next_cursor})
        })
    }
    /// A scheduler may prepare work only for a live worker in its own scope and
    /// with an exact capability contract allowlist. Identity and assignment are
    /// committed atomically with the prepared attempt.
    pub fn dispatch(&mut self, token: &str, lease: &Lease, worker_id: &str) -> Result<Dispatch> {
        self.transact(token,&[Role::Scheduler],"dispatch",&lease.run_id,|tx,who| {
            if lease.owner!=who.id {return Err(denied());}
            validate_id(worker_id)?;
            let row=tx.query_opt("SELECT id,tenant,project,actor,role,capabilities,issued_at,expires_at FROM workflow_access.credentials WHERE tenant=$1 AND project=$2 AND id=$3 AND role='worker' AND NOT revoked FOR SHARE", &[&who.tenant,&who.project,&worker_id]).map_err(storage)?.ok_or_else(denied)?;
            let worker=identity(&row)?;live(tx,&worker)?;who.fence(worker.issued,worker.expires);
            let claimed=who.change(tx,&lease.run_id,false,|s,c|s.claim_next(lease,c))?;
            match claimed {
                Claimed::Task{attempt}=>{
                    if !worker.capabilities.iter().any(|c|c.matches(&attempt.request)) {return Err(denied());}
                    let expires=(lease.expires_at_unix_ms.min(attempt.request.deadline_unix_ms) as i64).min(worker.expires);
                    who.fence(attempt.request.issued_at_unix_ms as i64,expires);
                    let id=random("assignment-")?;
                    let lease_json=serde_json::to_string(lease).map_err(|_|corrupt("assignment serialization failed"))?;
                    let task_json=serde_json::to_string(&attempt).map_err(|_|corrupt("assignment serialization failed"))?;
                    tx.execute("INSERT INTO workflow_access.assignments(id,tenant,project,worker_id,run_id,lease,task,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8)", &[&id,&who.tenant,&who.project,&worker_id,&lease.run_id,&lease_json,&task_json,&expires]).map_err(storage)?;
                    audit(tx,who,"assignment_created",&id,"accepted")?;
                    Ok(Dispatch::Task{assignment_id:id})
                },
                Claimed::Handled{..}=>Ok(Dispatch::Handled),Claimed::Idle=>Ok(Dispatch::Idle),
            }
        })
    }
    /// Authenticated worker delivery. The host returns only its assigned request
    /// and execution contract, not the rest of the run or scheduler credential.
    pub fn assignment(&mut self, token: &str, assignment: &str) -> Result<PreparedTask> {
        self.transact(
            token,
            &[Role::Worker],
            "assignment",
            assignment,
            |tx, who| {
                let a = load(tx, who, assignment)?;
                if a.settled {
                    return Err(Error::new(
                        ErrorCode::ReceiptConflict,
                        "assignment already settled",
                    ));
                }
                let at = now(tx)? as u64;
                who.read(tx, &a.lease.run_id, |s| {
                    check_lease(s, &a.lease, &a.task, at)
                })?;
                Ok(a.task)
            },
        )
    }
    /// Workers cannot supply tenant, actor, lease, run or attempt authority. All
    /// are loaded from the signed-in identity and the server's assignment row.
    pub fn finish(
        &mut self,
        token: &str,
        assignment: &str,
        result: &workflow_worker::WorkResult,
    ) -> Result<TaskReceipt> {
        self.transact(token, &[Role::Worker], "finish", assignment, |tx, who| {
            let a = load(tx, who, assignment)?;
            let committed = who.change(tx, &a.lease.run_id, false, |s, c| {
                check_lease(s, &a.lease, &a.task, c.now_unix_ms()?)?;
                s.finish_task(&a.lease, &a.task.attempt_id, result, c)
            })?;
            tx.execute(
                "UPDATE workflow_access.assignments SET settled=true WHERE id=$1",
                &[&assignment],
            )
            .map_err(storage)?;
            Ok(TaskReceipt {
                revision: committed.snapshot.revision,
                duplicate: committed.transition.duplicate,
            })
        })
    }
    pub fn fail(
        &mut self,
        token: &str,
        assignment: &str,
        error: &workflow_worker::Error,
    ) -> Result<()> {
        self.transact(token, &[Role::Worker], "fail", assignment, |tx, who| {
            let a = load(tx, who, assignment)?;
            if a.settled {
                return Err(Error::new(
                    ErrorCode::ReceiptConflict,
                    "assignment already settled",
                ));
            }
            // Transport failures never carry arbitrary exception text into the ledger.
            let bounded = workflow_worker::Error::new(
                error.code.clone(),
                "worker could not execute assigned contract",
            );
            who.change(tx, &a.lease.run_id, false, |s, c| {
                check_lease(s, &a.lease, &a.task, c.now_unix_ms()?)?;
                s.fail_task(&a.lease, &a.task.attempt_id, &bounded, c)
            })?;
            tx.execute(
                "UPDATE workflow_access.assignments SET settled=true WHERE id=$1",
                &[&assignment],
            )
            .map_err(storage)?;
            Ok(())
        })
    }
    /// Retain unresolved work after worker revocation/rotation/expiry for operator
    /// inspection. This protocol admits read-only tasks; it does not authorize
    /// external effects or claim to cancel work already running in a process.
    pub fn outstanding(
        &mut self,
        token: &str,
        after: &str,
        limit: u32,
    ) -> Result<Page<OutstandingAssignment, String>> {
        self.transact(token,&[Role::Administrator,Role::Recovery],"outstanding","assignments",|tx,who| {
            validate_limit(limit)?;
            if !after.is_empty(){validate_id(after)?;}
            let rows=tx.query("SELECT a.id,a.worker_id,a.run_id,a.expires_at,c.revoked FROM workflow_access.assignments a JOIN workflow_access.credentials c ON c.id=a.worker_id WHERE a.tenant=$1 AND a.project=$2 AND a.id COLLATE \"C\">$3 COLLATE \"C\" AND NOT a.settled ORDER BY a.id COLLATE \"C\" LIMIT $4", &[&who.tenant,&who.project,&after,&(i64::from(limit)+1)]).map_err(storage)?;
            let mut items:Vec<_>=rows.iter().map(|r|OutstandingAssignment{assignment_id:r.get(0),worker_id:r.get(1),run_id:r.get(2),expires_at_unix_ms:r.get(3),worker_revoked:r.get(4)}).collect();
            let next_cursor=if items.len()>limit as usize {items.pop();items.last().map(|r|r.assignment_id.clone())}else{None};
            Ok(Page{items,next_cursor})
        })
    }
}
