//! Authenticated application boundary for trusted service hosts. Database clients
//! stay on the host; callers provide opaque bearer credentials, never scope/actor.
//! Artifact authority is bound to exact assignments and capability policies.
//! Network TLS lives in workflow-service; external I/O stays on assigned workers.
use crate::*;
use serde::{Deserialize, Serialize};
mod artifact_catalog;
mod artifact_download;
mod artifact_policy;
mod artifact_upload;
mod effects;
mod operations;
mod tasks;
pub use artifact_download::{ArtifactCleanup, ArtifactDownloadChunk, ArtifactDownloadGrant};
pub use artifact_policy::{ArtifactOutputPolicy, ArtifactPolicy};
pub use artifact_upload::{ARTIFACT_CHUNK_BYTES, ArtifactUploadRequest, ArtifactUploadStatus};
pub use effects::{EffectDispatch, EffectRule, OutstandingEffect};
pub use operations::{RunControl, RunControlRequest};
pub use tasks::{Dispatch, OutstandingAssignment, TaskReceipt};
#[cfg(test)]
mod tests;

const ACCESS_SCHEMA: &str = include_str!("schema.sql");
const MAX_TTL: u64 = 3_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Administrator,
    DefinitionMaintainer,
    Viewer,
    Runner,
    Approver,
    Scheduler,
    Worker,
    Recovery,
}
impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Administrator => "administrator",
            Self::DefinitionMaintainer => "definition_maintainer",
            Self::Viewer => "viewer",
            Self::Runner => "runner",
            Self::Approver => "approver",
            Self::Scheduler => "scheduler",
            Self::Worker => "worker",
            Self::Recovery => "recovery",
        }
    }
}

/// Exact capability contract allowlist for a worker identity, authored by an administrator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRule {
    pub id: String,
    pub version: String,
    pub contract_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<ArtifactPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<EffectRule>,
}
impl CapabilityRule {
    fn matches(&self, request: &workflow_worker::WorkRequest) -> bool {
        self.id == request.capability.id
            && self.version == request.capability.version
            && self.contract_digest == request.contract_digest
    }
}

/// The only operation that exposes the new bearer is `expose_secret`. Neither
/// Debug nor audit/queue/state serialization contains it. Hosts must deliver it
/// over their authenticated private provisioning channel, never a command line.
pub struct IssuedCredential {
    pub id: String,
    pub expires_at_unix_ms: u64,
    secret: String,
}
impl IssuedCredential {
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }
}
impl std::fmt::Debug for IssuedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedCredential")
            .field("id", &self.id)
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    pub sequence: i64,
    pub actor: String,
    pub credential_id: String,
    pub operation: String,
    pub resource: String,
    pub outcome: String,
    pub at_unix_ms: i64,
}
struct Identity {
    id: String,
    tenant: String,
    project: String,
    actor: String,
    role: String,
    capabilities: Vec<CapabilityRule>,
    issued: i64,
    expires: i64,
    not_before: std::cell::Cell<i64>,
    deadline: std::cell::Cell<i64>,
}
fn denied() -> Error {
    Error::new(
        ErrorCode::Unauthorized,
        "identity or operation is not authorized",
    )
}
fn now(tx: &mut Transaction<'_>) -> Result<i64> {
    Ok(tx.query_one(NOW, &[]).map_err(storage)?.get(0))
}
fn random(prefix: &str) -> Result<String> {
    use std::fmt::Write;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| Error::new(ErrorCode::Storage, "credential entropy unavailable"))?;
    let mut s = String::from(prefix);
    for b in bytes {
        write!(s, "{b:02x}").expect("String formatting");
    }
    Ok(s)
}
fn token_digest(token: &str) -> Result<String> {
    if token.len() != 68
        || !token.starts_with("wf1_")
        || !token[4..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(denied());
    }
    Ok(hash(token.as_bytes()))
}
fn identity(row: &postgres::Row) -> Result<Identity> {
    Ok(Identity {
        id: row.get(0),
        tenant: row.get(1),
        project: row.get(2),
        actor: row.get(3),
        role: row.get(4),
        capabilities: serde_json::from_str(row.get(5))
            .map_err(|_| corrupt("credential policy invalid"))?,
        issued: row.get(6),
        expires: row.get(7),
        not_before: std::cell::Cell::new(row.get(6)),
        deadline: std::cell::Cell::new(row.get(7)),
    })
}
fn authenticate(tx: &mut Transaction<'_>, token: &str) -> Result<Identity> {
    let digest = token_digest(token)?;
    let row=tx.query_opt("SELECT id,tenant,project,actor,role,capabilities,issued_at,expires_at,revoked FROM workflow_access.credentials WHERE token_digest=$1 FOR SHARE", &[&digest]).map_err(storage)?.ok_or_else(denied)?;
    if row.get::<_, bool>(8) {
        return Err(denied());
    }
    let identity = identity(&row)?;
    live(tx, &identity)?;
    Ok(identity)
}
fn live(tx: &mut Transaction<'_>, identity: &Identity) -> Result<()> {
    let at = now(tx)?;
    if at <= 0 || at < identity.not_before.get() || at >= identity.deadline.get() {
        return Err(denied());
    }
    identity.not_before.set(at);
    Ok(())
}
fn audit(
    tx: &mut Transaction<'_>,
    identity: &Identity,
    operation: &str,
    resource: &str,
    outcome: &str,
) -> Result<()> {
    // No payload, raw error, token, token digest, SQL or provider secret is copied.
    tx.execute("INSERT INTO workflow_access.audit(tenant,project,actor,credential_id,operation,resource,outcome) VALUES($1,$2,$3,$4,$5,$6,$7)", &[&identity.tenant,&identity.project,&identity.actor,&identity.id,&operation,&resource,&outcome]).map_err(storage)?;
    Ok(())
}
fn issue(
    tx: &mut Transaction<'_>,
    tenant: &str,
    project: &str,
    actor: &str,
    role: Role,
    capabilities: &[CapabilityRule],
    ttl: u64,
) -> Result<IssuedCredential> {
    for id in [tenant, project, actor] {
        validate_id(id)?;
    }
    if !(1..=MAX_TTL).contains(&ttl)
        || capabilities.len() > 100
        || (role == Role::Worker) == capabilities.is_empty()
    {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "bounded TTL and role-specific capability allowlist required",
        ));
    }
    let mut unique = std::collections::BTreeSet::new();
    for c in capabilities {
        if let Some(effect) = &c.effect {
            effect.validate()?;
        }
        if let Some(policy) = &c.artifacts {
            policy.validate()?;
        }
        validate_id(&c.id)?;
        if c.version.is_empty()
            || c.version.len() > 128
            || !unique.insert((&c.id, &c.version))
            || !c.contract_digest.strip_prefix("sha256:").is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "unique exact capability contract required",
            ));
        }
    }
    let issued = now(tx)?;
    let expires = issued.checked_add(ttl as i64).ok_or_else(denied)?;
    let secret = random("wf1_")?;
    let id = random("credential-")?;
    let policy =
        serde_json::to_string(capabilities).map_err(|_| corrupt("credential policy invalid"))?;
    tx.execute("INSERT INTO workflow_access.credentials(id,token_digest,tenant,project,actor,role,capabilities,issued_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)", &[&id,&hash(secret.as_bytes()),&tenant,&project,&actor,&role.name(),&policy,&issued,&expires]).map_err(storage)?;
    Ok(IssuedCredential {
        id,
        secret,
        expires_at_unix_ms: expires as u64,
    })
}

pub struct AuthenticatedService {
    client: Client,
}
impl AuthenticatedService {
    /// Trusted deployment bootstrap only. One initial administrator per scope;
    /// revoking/expiring all administrators never silently reopens bootstrap.
    pub fn bootstrap(
        client: &mut Client,
        tenant: &str,
        project: &str,
        actor: &str,
        ttl_ms: u64,
    ) -> Result<IssuedCredential> {
        PostgresRunStore::initialize(client)?;
        let mut tx = client.transaction().map_err(storage)?;
        PostgresRunStore::transaction_settings(&mut tx)?;
        tx.query_one("SELECT pg_advisory_xact_lock(57465232)", &[])
            .map_err(storage)?;
        let exists: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_access')",
                &[],
            )
            .map_err(storage)?
            .get(0);
        if !exists {
            tx.batch_execute(ACCESS_SCHEMA).map_err(storage)?;
        }
        check_schema(&mut tx)?;
        artifact_catalog::initialize(&mut tx)?;
        effects::initialize(&mut tx)?;
        if tx.query_one("SELECT EXISTS(SELECT 1 FROM workflow_access.credentials WHERE tenant=$1 AND project=$2)", &[&tenant,&project]).map_err(storage)?.get::<_,bool>(0) { return Err(denied()); }
        let issued = issue(
            &mut tx,
            tenant,
            project,
            actor,
            Role::Administrator,
            &[],
            ttl_ms,
        )?;
        let identity = authenticate(&mut tx, issued.expose_secret())?;
        audit(&mut tx, &identity, "bootstrap", &issued.id, "accepted")?;
        tx.commit().map_err(storage)?;
        Ok(issued)
    }
    /// Explicit trusted-host initialization for an existing access deployment.
    /// Public artifact requests never create or upgrade database schemas.
    pub fn initialize_artifacts(client: &mut Client) -> Result<()> {
        let mut tx = client.transaction().map_err(storage)?;
        PostgresRunStore::transaction_settings(&mut tx)?;
        check_schema(&mut tx)?;
        artifact_catalog::initialize(&mut tx)?;
        tx.commit().map_err(storage)
    }
    /// Explicit, additive initialization for authenticated effect assignments.
    pub fn initialize_effects(client: &mut Client) -> Result<()> {
        let mut tx = client.transaction().map_err(storage)?;
        PostgresRunStore::transaction_settings(&mut tx)?;
        check_schema(&mut tx)?;
        effects::initialize(&mut tx)?;
        tx.commit().map_err(storage)
    }
    pub fn open(mut client: Client) -> Result<Self> {
        let mut tx = client.transaction().map_err(storage)?;
        check_schema(&mut tx)?;
        tx.commit().map_err(storage)?;
        Ok(Self { client })
    }
    fn transact<T>(
        &mut self,
        token: &str,
        roles: &[Role],
        operation: &str,
        resource: &str,
        f: impl FnOnce(&mut Transaction<'_>, &Identity) -> Result<T>,
    ) -> Result<T> {
        validate_id(resource)?;
        let mut tx = self.client.transaction().map_err(storage)?;
        PostgresRunStore::transaction_settings(&mut tx)?;
        let who = authenticate(&mut tx, token)?;
        if !roles.iter().any(|r| r.name() == who.role) {
            audit(&mut tx, &who, operation, resource, "denied")?;
            tx.commit().map_err(storage)?;
            return Err(denied());
        }
        // Keep credential locks outside the savepoint. Rejected application writes
        // roll back, while their bounded denial audit can commit independently.
        let result = {
            let mut save = tx.savepoint("authorized_operation").map_err(storage)?;
            match f(&mut save, &who).and_then(|r| {
                live(&mut save, &who)?;
                Ok(r)
            }) {
                Ok(r) => {
                    save.commit().map_err(storage)?;
                    Ok(r)
                }
                Err(e) => {
                    save.rollback().map_err(storage)?;
                    Err(e)
                }
            }
        };
        audit(
            &mut tx,
            &who,
            operation,
            resource,
            if result.is_ok() {
                "accepted"
            } else {
                "rejected"
            },
        )?;
        // Final DB-time check after audit locks; failed authority cannot leak a read
        // or a speculative write. This failure rolls back audit and operation.
        live(&mut tx, &who)?;
        tx.commit().map_err(storage)?;
        result.map_err(|e| Error::new(e.code, "authorized operation rejected"))
    }
}
fn check_schema(tx: &mut Transaction<'_>) -> Result<()> {
    let v: i32 = tx
        .query_one(
            "SELECT version FROM workflow_access.schema_version WHERE singleton=true",
            &[],
        )
        .map_err(storage)?
        .get(0);
    if v != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "unsupported access schema",
        ));
    }
    Ok(())
}
