use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunControl {
    Pause { reason: String },
    Resume { reason: String },
    Cancel,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunControlRequest {
    pub run_id: String,
    pub event_id: String,
    pub expected_revision: u64,
    pub control: RunControl,
}
pub(super) const READ: &[Role] = &[
    Role::DefinitionMaintainer,
    Role::Viewer,
    Role::Runner,
    Role::Approver,
    Role::SignalSource,
    Role::Scheduler,
    Role::Recovery,
];
impl Identity {
    pub(super) fn fence(&self, lower: i64, upper: i64) {
        self.not_before.set(self.not_before.get().max(lower));
        self.deadline.set(self.deadline.get().min(upper));
    }
    pub(super) fn fence_execution(&self, lower: i64, upper: i64) {
        self.execution_not_before
            .set(self.execution_not_before.get().max(lower));
        self.execution_deadline
            .set(self.execution_deadline.get().min(upper));
    }
    pub(super) fn change<T>(
        &self,
        tx: &mut Transaction<'_>,
        id: &str,
        create: bool,
        f: impl FnOnce(&mut SqliteRunStore, &dyn Clock) -> Result<T>,
    ) -> Result<T> {
        validate_id(id)?;
        if create {
            // Establish the aggregate row before loading its artifact snapshot.
            // A concurrent first start must not leave us with a catalog captured
            // before the existing run (and its completed evidence) was visible.
            tx.execute("INSERT INTO workflow_authority.runs(tenant,project,run_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[&self.tenant,&self.project,&id]).map_err(storage)?;
        }
        tx.query_opt("SELECT run_id FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3 FOR UPDATE", &[&self.tenant,&self.project,&id]).map_err(storage)?;
        let reader = artifact_catalog::load(tx, self, id)?;
        let (result, lower, upper) =
            PostgresRunStore::change_in(tx, &self.tenant, &self.project, reader, id, create, f)?;
        self.fence_execution(lower, upper);
        Ok(result)
    }
    pub(super) fn read<T>(
        &self,
        tx: &mut Transaction<'_>,
        id: &str,
        f: impl FnOnce(&mut SqliteRunStore) -> Result<T>,
    ) -> Result<T> {
        validate_id(id)?;
        // Lock the aggregate through the read/audit transaction. Immutable global
        // bindings are verified against the same current aggregate version.
        let row=tx.query_opt("SELECT image,image_digest FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3 FOR SHARE", &[&self.tenant,&self.project,&id]).map_err(storage)?.ok_or_else(|| Error::new(ErrorCode::NotFound,"run not found"))?;
        let reader = artifact_catalog::load(tx, self, id)?;
        let mut store =
            PostgresRunStore::checked_row(tx, &self.tenant, &self.project, &row, id, reader)?;
        f(&mut store)
    }
}
impl AuthenticatedService {
    pub fn acceptance(&mut self, token: &str, run: &str) -> Result<AcceptanceManifest> {
        self.transact(token, READ, "acceptance", run, |tx, who| {
            who.read(tx, run, |s| s.acceptance(run))
        })
    }
    /// Bounded control commands, never arbitrary task outcomes or state writes.
    pub fn control(&mut self, token: &str, request: &RunControlRequest) -> Result<Committed> {
        self.transact(
            token,
            &[Role::Runner, Role::Recovery],
            "control",
            &request.run_id,
            |tx, who| {
                validate_id(&request.event_id)?;
                who.change(tx, &request.run_id, false, |s, c| {
                    let state = s.get(&request.run_id)?;
                    let kind = match &request.control {
                        RunControl::Pause { reason } => workflow_kernel::EventKind::Pause {
                            reason: reason.clone(),
                        },
                        RunControl::Resume { reason } => workflow_kernel::EventKind::Resume {
                            reason: reason.clone(),
                        },
                        RunControl::Cancel => workflow_kernel::EventKind::Cancel,
                    };
                    // Retry uses the original time; changed control/revision still
                    // conflicts with the event digest in the authoritative reducer.
                    let mut at = c.now_unix_ms()?;
                    let mut cursor = 0;
                    loop {
                        let page = s.history(&request.run_id, cursor, 100)?;
                        if let Some(old) = page
                            .items
                            .iter()
                            .find(|e| e.event.event_id == request.event_id)
                        {
                            at = old.event.at_unix_ms;
                            break;
                        }
                        match page.next_cursor {
                            Some(next) => cursor = next,
                            None => break,
                        }
                    }
                    s.apply(&Event {
                        event_id: request.event_id.clone(),
                        run_id: request.run_id.clone(),
                        run_digest: state.run_digest,
                        expected_revision: request.expected_revision,
                        at_unix_ms: at,
                        kind,
                    })
                })
            },
        )
    }
    /// A bounded scan for control-plane reconciliation. IDs are scoped by the
    /// authenticated credential; each returned aggregate is locked and verified.
    pub fn runs(
        &mut self,
        token: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Page<RunSummary, String>> {
        self.transact(token, READ, "runs", "runs", |tx, who| {
            validate_limit(limit)?;
            if let Some(id)=after {validate_id(id)?;}
            let rows=tx.query("SELECT run_id FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND ($3::text IS NULL OR run_id COLLATE \"C\">$3 COLLATE \"C\") ORDER BY run_id COLLATE \"C\" LIMIT $4", &[&who.tenant,&who.project,&after,&(i64::from(limit)+1)]).map_err(storage)?;
            let mut items=vec![];
            for row in rows {
                let id:String=row.get(0);
                let s=who.read(tx,&id,|s|s.get(&id))?;
                items.push(RunSummary{run_id:s.run_id,revision:s.revision,status:s.status,pause:s.pause,bundle_digest:s.bundle_digest});
            }
            let next_cursor=if items.len()>limit as usize {items.pop();items.last().map(|s|s.run_id.clone())}else{None};
            Ok(Page{items,next_cursor})
        })
    }
    /// Read-only authoring validation. The label is never opened as a path and
    /// no document bytes or diagnostic text are copied into the audit ledger.
    pub fn validate_definition(
        &mut self,
        token: &str,
        source: &str,
        format: workflow_ir::Format,
        file: &str,
    ) -> Result<workflow_validator::ValidationReport> {
        self.transact(
            token,
            &[Role::DefinitionMaintainer, Role::Runner],
            "validate_definition",
            "definition",
            |_tx, _who| Ok(workflow_validator::validate_source(source, format, file)),
        )
    }
    /// Administrator-created credentials cannot escape the administrator's scope.
    pub fn issue(
        &mut self,
        token: &str,
        actor: &str,
        role: Role,
        capabilities: &[CapabilityRule],
        ttl_ms: u64,
    ) -> Result<IssuedCredential> {
        self.transact(token, &[Role::Administrator], "issue", actor, |tx, who| {
            let issued = issue(
                tx,
                &who.tenant,
                &who.project,
                actor,
                role,
                capabilities,
                ttl_ms,
            )?;
            audit(tx, who, "credential_created", &issued.id, "accepted")?;
            Ok(issued)
        })
    }
    /// Rotation creates a new identity credential and revokes the old one in one
    /// transaction. Assignments remain bound to the old credential and must be
    /// reconciled/reissued; rotation cannot silently transfer running authority.
    pub fn rotate(&mut self, token: &str, target: &str, ttl_ms: u64) -> Result<IssuedCredential> {
        self.transact(token,&[Role::Administrator],"rotate",target,|tx,who| {
            let row=tx.query_opt("SELECT id,tenant,project,actor,role,capabilities,issued_at,expires_at FROM workflow_access.credentials WHERE tenant=$1 AND project=$2 AND id=$3 AND NOT revoked FOR UPDATE", &[&who.tenant,&who.project,&target]).map_err(storage)?.ok_or_else(denied)?;
            let old=identity(&row)?;
            let role:Role=serde_json::from_value(serde_json::Value::String(old.role)).map_err(|_| corrupt("credential role invalid"))?;
            let issued=issue(tx,&who.tenant,&who.project,&old.actor,role,&old.capabilities,ttl_ms)?;
            tx.execute("UPDATE workflow_access.credentials SET revoked=true WHERE id=$1", &[&target]).map_err(storage)?;
            audit(tx,who,"credential_created",&issued.id,"accepted")?;Ok(issued)
        })
    }
    pub fn revoke(&mut self, token: &str, target: &str) -> Result<()> {
        self.transact(token,&[Role::Administrator],"revoke",target,|tx,who| {
            let changed=tx.execute("UPDATE workflow_access.credentials SET revoked=true WHERE tenant=$1 AND project=$2 AND id=$3", &[&who.tenant,&who.project,&target]).map_err(storage)?;
            if changed!=1 {return Err(denied());} Ok(())
        })
    }
    pub fn audit(&mut self, token: &str, after: i64, limit: u32) -> Result<Page<AuditEntry, i64>> {
        self.transact(token,&[Role::Administrator,Role::Recovery],"audit","audit",|tx,who| {
            validate_limit(limit)?;
            if after<0 { return Err(Error::new(ErrorCode::InvalidRequest,"nonnegative cursor required")); }
            let rows=tx.query("SELECT sequence,actor,credential_id,operation,resource,outcome,at_unix_ms FROM workflow_access.audit WHERE tenant=$1 AND project=$2 AND sequence>$3 ORDER BY sequence LIMIT $4", &[&who.tenant,&who.project,&after,&(i64::from(limit)+1)]).map_err(storage)?;
            let mut items:Vec<_>=rows.iter().map(|r| AuditEntry {sequence:r.get(0),actor:r.get(1),credential_id:r.get(2),operation:r.get(3),resource:r.get(4),outcome:r.get(5),at_unix_ms:r.get(6)}).collect();
            let next_cursor=if items.len()>limit as usize {items.pop();items.last().map(|r|r.sequence)}else{None};
            Ok(Page {items,next_cursor})
        })
    }
    /// Publish a validated immutable bundle digest within this scope. Publishing
    /// does not start a run or grant execution authority to its author.
    pub fn publish(&mut self, token: &str, bundle: &BundleSpec) -> Result<String> {
        self.transact(token, &[Role::DefinitionMaintainer], "publish", "bundle", |tx, who| {
            let compiled = workflow_kernel::CompiledBundle::compile(bundle.clone())?;
            bind_versions(tx, &who.tenant, &who.project,
                workflow_runstore_sqlite::bundle_bindings(&compiled)?, true)?;
            let digest = compiled.digest().to_owned();
            tx.execute("INSERT INTO workflow_access.published_bundles(tenant,project,digest,published_by) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING", &[&who.tenant,&who.project,&digest,&who.id]).map_err(storage)?;
            audit(tx, who, "bundle_published", &digest, "accepted")?;
            Ok(digest)
        })
    }
    /// Server supplies start time. Repeated start uses the original durable time
    /// so transport retries retain the existing start/idempotency contract.
    pub fn start(&mut self, token: &str, request: &StartRun) -> Result<Committed> {
        self.transact(
            token,
            &[Role::Runner],
            "start",
            &request.run_id,
            |tx, who| {
                let compiled = workflow_kernel::CompiledBundle::compile(request.bundle.clone())?;
                scheduling::configuration_guard(tx,&who.tenant)?;
                if tx.query_opt("SELECT digest FROM workflow_access.published_bundles WHERE tenant=$1 AND project=$2 AND digest=$3 FOR SHARE", &[&who.tenant,&who.project,&compiled.digest()]).map_err(storage)?.is_none() { return Err(denied()); }
                who.change(tx, &request.run_id, true, |s, c| {
                    let mut request = request.clone();
                    request.started_at_unix_ms = match s.started_at(&request.run_id) {
                        Ok(old) => old,
                        Err(e) if e.code == ErrorCode::NotFound => c.now_unix_ms()?,
                        Err(e) => return Err(e),
                    };
                    s.start(&request)
                })
            },
        )
    }
    pub fn get(&mut self, token: &str, run: &str) -> Result<Snapshot> {
        self.transact(token, READ, "get", run, |tx, who| {
            who.read(tx, run, |s| s.get(run))
        })
    }
    pub fn history(
        &mut self,
        token: &str,
        run: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<RecordedEvent, u64>> {
        self.transact(token, READ, "history", run, |tx, who| {
            who.read(tx, run, |s| s.history(run, after, limit))
        })
    }
    pub fn waits(
        &mut self,
        token: &str,
        run: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<WaitRegistration, u64>> {
        self.transact(token, READ, "waits", run, |tx, who| {
            who.read(tx, run, |s| s.waits(run, after, limit))
        })
    }
    pub fn inbox(
        &mut self,
        token: &str,
        run: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<workflow_kernel::InboxEntry, u64>> {
        self.transact(token, READ, "inbox", run, |tx, who| {
            who.read(tx, run, |s| s.inbox(run, after, limit))
        })
    }
    /// Identity is stamped here; an approval's self-declared source is never
    /// authority. Only approval decisions are exposed by this endpoint.
    pub fn approve(&mut self, token: &str, request: &SignalSubmission) -> Result<SignalReceipt> {
        self.receive_callback(
            token,
            request,
            Role::Approver,
            workflow_kernel::WaitKind::HumanApproval,
        )
    }
    /// Authenticated external events have a separate role and a frozen event policy.
    pub fn signal(&mut self, token: &str, request: &SignalSubmission) -> Result<SignalReceipt> {
        self.receive_callback(
            token,
            request,
            Role::SignalSource,
            workflow_kernel::WaitKind::ExternalEvent,
        )
    }
    fn receive_callback(
        &mut self,
        token: &str,
        request: &SignalSubmission,
        role: Role,
        channel: workflow_kernel::WaitKind,
    ) -> Result<SignalReceipt> {
        self.transact(
            token,
            &[role],
            if channel == workflow_kernel::WaitKind::HumanApproval {
                "approve"
            } else {
                "signal"
            },
            &request.run_id,
            |tx, who| {
                let mut request = request.clone();
                request.message.source = who.actor.clone();
                who.change(tx, &request.run_id, false, |s, c| {
                    let bundle = s.bundle(&request.run_id)?;
                    let state = s.get(&request.run_id)?;
                    let binding = state.frames.values().find_map(|f| {
                        f.nodes
                            .iter()
                            .find(|(_, n)| n.instance_id == request.message.target.instance_id)
                            .and_then(|(id, _)| {
                                bundle
                                    .wait_policies
                                    .iter()
                                    .find(|b| b.workflow == f.workflow && b.node_id == *id)
                            })
                    });
                    if !binding.is_some_and(|b| b.policy.kind == channel) {
                        return Err(denied());
                    }
                    s.receive_signal(&request, c)
                })
            },
        )
    }
    pub fn acquire(
        &mut self,
        token: &str,
        run: &str,
        acquisition: &str,
        ttl_ms: u64,
    ) -> Result<Lease> {
        self.transact(
            token,
            &[Role::Scheduler, Role::Recovery, Role::Administrator],
            "acquire",
            run,
            |tx, who| {
                let request = LeaseRequest {
                    run_id: run.into(),
                    owner: who.id.clone(),
                    acquisition_id: acquisition.into(),
                    ttl_ms,
                };
                who.change(tx, run, false, |s, c| s.acquire(&request, c))
            },
        )
    }
    pub fn release(&mut self, token: &str, lease: &Lease) -> Result<()> {
        self.transact(
            token,
            &[Role::Scheduler, Role::Recovery, Role::Administrator],
            "release",
            &lease.run_id,
            |tx, who| {
                if lease.owner != who.id {
                    return Err(denied());
                }
                who.change(tx, &lease.run_id, false, |s, c| s.release(lease, c))
            },
        )
    }
    pub fn tick(&mut self, token: &str, lease: &Lease) -> Result<Option<Committed>> {
        self.transact(
            token,
            &[Role::Scheduler],
            "tick",
            &lease.run_id,
            |tx, who| {
                if lease.owner != who.id {
                    return Err(denied());
                }
                who.change(tx, &lease.run_id, false, |s, c| s.tick_due(lease, c))
            },
        )
    }
    pub fn acknowledge_recovery(
        &mut self,
        token: &str,
        run: &str,
        request: &RecoveryAcknowledgement,
    ) -> Result<bool> {
        self.transact(
            token,
            &[Role::Recovery],
            "acknowledge_recovery",
            run,
            |tx, who| {
                let mut request = request.clone();
                request.actor = who.actor.clone();
                who.change(tx, run, false, |s, c| {
                    s.acknowledge_recovery(run, &request, c)
                })
            },
        )
    }
}
