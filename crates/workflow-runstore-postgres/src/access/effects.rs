use super::*;
use workflow_effects::{EffectAttempt, EffectPolicy, EffectState, ManualResolution, Observation};

/// An exact frozen effect policy, in addition to the capability contract rule.
/// Naming a write capability alone never grants a target or service principal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRule {
    pub policy: EffectPolicy,
}
impl EffectRule {
    pub(super) fn validate(&self) -> Result<()> {
        workflow_effects::validate_policy(&self.policy)?;
        Ok(())
    }
}
fn permitted(who: &Identity, attempt: &EffectAttempt) -> Result<()> {
    let capability = workflow_worker::Capability::new(attempt.intent.capability.clone())?;
    if who.capabilities.iter().any(|c| {
        c.id == capability.descriptor().capability.id
            && c.version == capability.descriptor().capability.version
            && c.contract_digest == capability.digest()
            && c.effect
                .as_ref()
                .is_some_and(|e| e.policy == attempt.intent.policy)
    }) {
        Ok(())
    } else {
        Err(denied())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectDispatch {
    Call { assignment_id: String },
    Waiting { not_before_unix_ms: u64 },
    Manual { operation_key: String },
    Handled,
    Idle,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutstandingEffect {
    pub assignment_id: String,
    pub worker_id: String,
    pub run_id: String,
    pub delivered: bool,
    pub expires_at_unix_ms: i64,
    pub worker_revoked: bool,
}
pub(super) fn initialize(tx: &mut Transaction<'_>) -> Result<()> {
    tx.query_one("SELECT pg_advisory_xact_lock(57465234)", &[])
        .map_err(storage)?;
    let exists: bool = tx
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_effect_dispatch')",
            &[],
        )
        .map_err(storage)?
        .get(0);
    if !exists {
        tx.batch_execute(include_str!("effect_schema.sql"))
            .map_err(storage)?;
    }
    check(tx)
}
pub(super) fn check(tx: &mut Transaction<'_>) -> Result<()> {
    let version: i32 = tx
        .query_one(
            "SELECT version FROM workflow_effect_dispatch.schema_version WHERE singleton=true",
            &[],
        )
        .map_err(storage)?
        .get(0);
    if version != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "unsupported effect dispatch schema",
        ));
    }
    Ok(())
}
struct Assignment {
    lease: Lease,
    attempt: EffectAttempt,
    delivered: bool,
    settled: bool,
}
fn load(tx: &mut Transaction<'_>, who: &Identity, id: &str) -> Result<Assignment> {
    check(tx)?;
    let row = tx.query_opt("SELECT lease,attempt,expires_at,delivered,settled FROM workflow_effect_dispatch.assignments WHERE tenant=$1 AND project=$2 AND worker_id=$3 AND id=$4 FOR UPDATE", &[&who.tenant,&who.project,&who.id,&id]).map_err(storage)?.ok_or_else(denied)?;
    let a = Assignment {
        lease: serde_json::from_str(row.get(0))
            .map_err(|_| corrupt("effect assignment lease invalid"))?,
        attempt: serde_json::from_str(row.get(1))
            .map_err(|_| corrupt("effect assignment attempt invalid"))?,
        delivered: row.get(3),
        settled: row.get(4),
    };
    let at = now(tx)?;
    if at < a.lease.issued_at_unix_ms as i64 || at >= a.lease.expires_at_unix_ms as i64 {
        return Err(Error::new(
            ErrorCode::LeaseConflict,
            "effect assignment lease expired",
        ));
    }
    who.fence_execution(a.lease.issued_at_unix_ms as i64, row.get(2));
    permitted(who, &a.attempt)?;
    live(tx, who)?;
    Ok(a)
}
pub(super) fn authority(store: &mut SqliteRunStore, run: &str) -> Result<Authority> {
    let mut authority = Authority::new(run, store.started_at(run)?);
    let mut cursor = 0;
    loop {
        let page = store.execution_history(run, cursor, 100)?;
        for record in page.items {
            authority.apply(&record.action)?;
        }
        match page.next_cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }
    Ok(authority)
}
fn check_attempt(store: &mut SqliteRunStore, a: &Assignment, at: u64) -> Result<()> {
    let authority = authority(store, &a.lease.run_id)?;
    authority.check_live(&a.lease, at)?;
    if !authority
        .effects
        .get(&a.attempt.intent.operation_key)
        .is_some_and(|s| s.calls.iter().any(|c| c.attempt == a.attempt))
    {
        return Err(corrupt("effect assignment differs from committed intent"));
    }
    Ok(())
}
impl AuthenticatedService {
    /// Prepare an effect and its scoped worker assignment in one transaction.
    /// No provider call occurs here. A rejected policy rolls back the intent.
    pub fn dispatch_effect(
        &mut self,
        token: &str,
        lease: &Lease,
        worker_id: &str,
    ) -> Result<EffectDispatch> {
        self.transact(token, &[Role::Scheduler], "dispatch_effect", &lease.run_id, |tx, who| {
            check(tx)?;
            if lease.owner != who.id { return Err(denied()); }
            validate_id(worker_id)?;
            let row = tx.query_opt("SELECT id,tenant,project,actor,role,capabilities,issued_at,expires_at FROM workflow_access.credentials WHERE tenant=$1 AND project=$2 AND id=$3 AND role='worker' AND NOT revoked FOR SHARE", &[&who.tenant,&who.project,&worker_id]).map_err(storage)?.ok_or_else(denied)?;
            let worker = identity(&row)?;
            live(tx, &worker)?;
            who.fence(worker.issued, worker.expires);
            match who.change(tx, &lease.run_id, false, |s,c| s.claim_effect(lease,c))? {
                EffectClaim::Call { attempt } => {
                    permitted(&worker, &attempt)?;
                    if worker.expires < attempt.deadline_unix_ms as i64 { return Err(denied()); }
                    // Observation can retain a late truthful receipt, but no later
                    // than the live lease and worker credential permit.
                    let expires = (lease.expires_at_unix_ms as i64).min(worker.expires);
                    who.fence_execution(attempt.issued_at_unix_ms as i64, (attempt.deadline_unix_ms as i64).min(expires));
                    let id = random("effect-assignment-")?;
                    let lease_json = serde_json::to_string(lease).map_err(|_| corrupt("effect lease serialization failed"))?;
                    let attempt_json = serde_json::to_string(&attempt).map_err(|_| corrupt("effect attempt serialization failed"))?;
                    tx.execute("INSERT INTO workflow_effect_dispatch.assignments(id,tenant,project,worker_id,run_id,lease,attempt,expires_at,deliver_before) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)", &[&id,&who.tenant,&who.project,&worker_id,&lease.run_id,&lease_json,&attempt_json,&expires,&(attempt.deadline_unix_ms as i64)]).map_err(storage)?;
                    audit(tx,who,"effect_assignment_created",&id,"accepted")?;
                    Ok(EffectDispatch::Call { assignment_id: id })
                }
                EffectClaim::Waiting { not_before_unix_ms } => Ok(EffectDispatch::Waiting { not_before_unix_ms }),
                EffectClaim::Manual { operation_key, .. } => Ok(EffectDispatch::Manual { operation_key }),
                EffectClaim::Handled => Ok(EffectDispatch::Handled),
                EffectClaim::Idle => Ok(EffectDispatch::Idle),
            }
        })
    }
    pub fn pending_effects(
        &mut self,
        token: &str,
        after: &str,
        limit: u32,
    ) -> Result<Page<String, String>> {
        self.transact(token, &[Role::Worker], "pending_effects", "effects", |tx,who| {
            check(tx)?;
            validate_limit(limit)?;
            if !after.is_empty() { validate_id(after)?; }
            let at = now(tx)?;
            let rows=tx.query("SELECT id FROM workflow_effect_dispatch.assignments WHERE tenant=$1 AND project=$2 AND worker_id=$3 AND NOT delivered AND NOT settled AND expires_at>$4 AND deliver_before>$4 AND id COLLATE \"C\">$5 COLLATE \"C\" ORDER BY id COLLATE \"C\" LIMIT $6", &[&who.tenant,&who.project,&who.id,&at,&after,&(i64::from(limit)+1)]).map_err(storage)?;
            let mut items: Vec<String> = rows.iter().map(|r|r.get(0)).collect();
            let next_cursor = if items.len()>limit as usize { items.pop(); items.last().cloned() } else { None };
            Ok(Page { items, next_cursor })
        })
    }
    /// Single delivery even when two processes share one worker credential. If
    /// the response is lost, a successor lease must reconcile the original key.
    pub fn effect_assignment(&mut self, token: &str, assignment: &str) -> Result<EffectAttempt> {
        self.transact(
            token,
            &[Role::Worker],
            "effect_assignment",
            assignment,
            |tx, who| {
                let a = load(tx, who, assignment)?;
                if a.delivered || a.settled {
                    return Err(Error::new(
                        ErrorCode::ReceiptConflict,
                        "effect assignment already delivered",
                    ));
                }
                who.fence_execution(
                    a.attempt.issued_at_unix_ms as i64,
                    a.attempt.deadline_unix_ms as i64,
                );
                let at = now(tx)? as u64;
                who.read(tx, &a.lease.run_id, |s| {
                    check_attempt(s, &a, at)?;
                    let snapshot = s.get(&a.lease.run_id)?;
                    let node = snapshot
                        .frames
                        .values()
                        .flat_map(|f| f.nodes.values())
                        .find(|n| n.instance_id == a.attempt.intent.instance_id)
                        .ok_or_else(|| corrupt("effect node missing"))?;
                    if snapshot.pause.is_some()
                        || (a.attempt.kind == workflow_effects::CallKind::Write
                            && (node.state != workflow_kernel::NodeState::TaskReady
                                || node.cancel_requested))
                    {
                        return Err(Error::new(
                            ErrorCode::TransitionRejected,
                            "effect delivery is paused or cancelled",
                        ));
                    }
                    Ok(())
                })?;
                tx.execute(
                    "UPDATE workflow_effect_dispatch.assignments SET delivered=true WHERE id=$1",
                    &[&assignment],
                )
                .map_err(storage)?;
                Ok(a.attempt)
            },
        )
    }
    pub fn observe_assigned_effect(
        &mut self,
        token: &str,
        assignment: &str,
        observation: &Observation,
    ) -> Result<TaskReceipt> {
        self.transact(
            token,
            &[Role::Worker],
            "observe_effect",
            assignment,
            |tx, who| {
                let a = load(tx, who, assignment)?;
                if !a.delivered {
                    return Err(denied());
                }
                // Arbitrary provider exception text must never enter shared history.
                let bounded = match observation {
                    Observation::Unknown { .. } => Observation::Unknown {
                        reason: "worker returned no verified effect observation".into(),
                    },
                    Observation::NotApplied { code, class, .. } => Observation::NotApplied {
                        code: code.clone(),
                        class: class.clone(),
                        message: "provider reported no effect".into(),
                    },
                    other => other.clone(),
                };
                let committed = who.change(tx, &a.lease.run_id, false, |s, c| {
                    check_attempt(s, &a, c.now_unix_ms()?)?;
                    s.observe_effect(&a.lease, &a.attempt.attempt_id, &bounded, c)
                })?;
                tx.execute(
                    "UPDATE workflow_effect_dispatch.assignments SET settled=true WHERE id=$1",
                    &[&assignment],
                )
                .map_err(storage)?;
                Ok(TaskReceipt {
                    revision: committed.snapshot.revision,
                    duplicate: committed.transition.duplicate,
                })
            },
        )
    }
    pub fn effects(
        &mut self,
        token: &str,
        run: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<EffectState, u64>> {
        self.transact(token, operations::READ, "effects", run, |tx, who| {
            who.read(tx, run, |s| s.effects(run, after, limit))
        })
    }
    pub fn resolve_effect(
        &mut self,
        token: &str,
        lease: &Lease,
        key: &str,
        resolution: &ManualResolution,
    ) -> Result<TaskReceipt> {
        self.transact(
            token,
            &[Role::Recovery],
            "resolve_effect",
            &lease.run_id,
            |tx, who| {
                if lease.owner != who.id {
                    return Err(denied());
                }
                let mut resolution = resolution.clone();
                resolution.actor = who.actor.clone();
                who.fence_execution(
                    lease.issued_at_unix_ms as i64,
                    lease.expires_at_unix_ms as i64,
                );
                let committed = who.change(tx, &lease.run_id, false, |s, c| {
                    authority(s, &lease.run_id)?.check_live(lease, c.now_unix_ms()?)?;
                    s.resolve_effect(lease, key, &resolution, c)
                })?;
                Ok(TaskReceipt {
                    revision: committed.snapshot.revision,
                    duplicate: committed.transition.duplicate,
                })
            },
        )
    }
    pub fn outstanding_effects(
        &mut self,
        token: &str,
        after: &str,
        limit: u32,
    ) -> Result<Page<OutstandingEffect, String>> {
        self.transact(token, &[Role::Administrator,Role::Recovery], "outstanding_effects", "effects", |tx,who| {
            check(tx)?;
            validate_limit(limit)?;
            if !after.is_empty() {validate_id(after)?;}
            let rows=tx.query("SELECT a.id,a.worker_id,a.run_id,a.delivered,a.expires_at,c.revoked FROM workflow_effect_dispatch.assignments a JOIN workflow_access.credentials c ON c.id=a.worker_id WHERE a.tenant=$1 AND a.project=$2 AND a.id COLLATE \"C\">$3 COLLATE \"C\" AND NOT a.settled ORDER BY a.id COLLATE \"C\" LIMIT $4", &[&who.tenant,&who.project,&after,&(i64::from(limit)+1)]).map_err(storage)?;
            let mut items:Vec<_>=rows.iter().map(|r| OutstandingEffect { assignment_id:r.get(0),worker_id:r.get(1),run_id:r.get(2),delivered:r.get(3),expires_at_unix_ms:r.get(4),worker_revoked:r.get(5) }).collect();
            let next_cursor=if items.len()>limit as usize {items.pop();items.last().map(|r|r.assignment_id.clone())}else{None};
            Ok(Page {items,next_cursor})
        })
    }
}
