//! PostgreSQL is the durable authority; SQLite is a volatile transaction reducer.
//! This adapter accepts trusted host operations. Scope labels are not authentication.
use postgres::{Client, Transaction};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, rc::Rc};
use workflow_artifacts::ArtifactReader;
use workflow_runstore::*;
use workflow_runstore_sqlite::{ImageBinding, RunImage, SqliteRunStore};
use workflow_worker::Clock;
pub mod access;
mod ports;

const SCHEMA: &str = include_str!("schema.sql");
const NOW: &str = "SELECT floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint";
fn storage(error: postgres::Error) -> Error {
    let code = match error.code().map(|c| c.code()) {
        Some("55P03" | "40P01" | "40001" | "57014") => ErrorCode::Busy,
        _ => ErrorCode::Storage,
    };
    // Never return the server's SQL, bound parameters, connection URI or payload.
    Error::new(
        code,
        "PostgreSQL transaction failed; no durable success confirmed",
    )
}
fn corrupt(message: &str) -> Error {
    Error::new(ErrorCode::CorruptStorage, message)
}
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
/// Sorted immutable identities are shared by publication and run commits.
fn bind_versions(
    tx: &mut Transaction<'_>,
    tenant: &str,
    project: &str,
    bindings: Vec<ImageBinding>,
    create: bool,
) -> Result<()> {
    for binding in bindings {
        if create {
            tx.execute("INSERT INTO workflow_authority.bindings(tenant,project,kind,id,version,digest) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING", &[&tenant,&project,&binding.kind,&binding.id,&binding.version,&binding.digest]).map_err(storage)?;
        }
        let row = tx.query_opt("SELECT digest FROM workflow_authority.bindings WHERE tenant=$1 AND project=$2 AND kind=$3 AND id=$4 AND version=$5 FOR SHARE", &[&tenant,&project,&binding.kind,&binding.id,&binding.version]).map_err(storage)?;
        if row.as_ref().map(|r| r.get::<_, &str>(0)) != Some(binding.digest.as_str()) {
            return Err(Error::new(
                ErrorCode::BindingConflict,
                "immutable shared version differs or is missing",
            ));
        }
    }
    Ok(())
}
struct Reader(Rc<dyn ArtifactReader>);
impl ArtifactReader for Reader {
    fn verify(
        &self,
        link: &workflow_artifacts::ArtifactLink,
    ) -> workflow_artifacts::Result<workflow_artifacts::ArtifactRef> {
        self.0.verify(link)
    }
}
struct DbClock<'a, 'b> {
    transaction: RefCell<&'a mut Transaction<'b>>,
    last: std::cell::Cell<u64>,
}
impl Clock for DbClock<'_, '_> {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        let row = self
            .transaction
            .borrow_mut()
            .query_one(NOW, &[])
            .map_err(|_| {
                workflow_worker::Error::new(
                    workflow_worker::ErrorCode::ClockError,
                    "authoritative database clock unavailable",
                )
            })?;
        let now = u64::try_from(row.get::<_, i64>(0)).map_err(|_| {
            workflow_worker::Error::new(
                workflow_worker::ErrorCode::ClockError,
                "invalid database time",
            )
        })?;
        if now < self.last.get() {
            return Err(workflow_worker::Error::new(
                workflow_worker::ErrorCode::ClockError,
                "database clock reversed",
            ));
        }
        self.last.set(now);
        Ok(now)
    }
}
pub struct PostgresRunStore {
    client: Client,
    tenant: String,
    project: String,
    artifacts: Option<Rc<dyn ArtifactReader>>,
}
impl PostgresRunStore {
    /// Explicit initialization; does not adopt or overwrite a foreign schema.
    pub fn initialize(client: &mut Client) -> Result<()> {
        let mut tx = client.transaction().map_err(storage)?;
        tx.batch_execute("SET LOCAL lock_timeout = '5s'; SET LOCAL statement_timeout = '30s'; SELECT pg_advisory_xact_lock(57465231)").map_err(storage)?;
        let exists: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_authority')",
                &[],
            )
            .map_err(storage)?
            .get(0);
        if !exists {
            tx.batch_execute(SCHEMA).map_err(storage)?;
        }
        let version: i32 = tx
            .query_one(
                "SELECT version FROM workflow_authority.schema_version WHERE singleton=true",
                &[],
            )
            .map_err(storage)?
            .get(0);
        if version != 1 {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "unsupported PostgreSQL authority version",
            ));
        }
        tx.commit().map_err(storage)
    }
    /// The authenticated host derives tenant/project; never copy them from a
    /// worker result or model output. The supplied client owns TLS/auth settings.
    pub fn open(mut client: Client, tenant: &str, project: &str) -> Result<Self> {
        validate_id(tenant)?;
        validate_id(project)?;
        let version: i32 = client
            .query_one(
                "SELECT version FROM workflow_authority.schema_version WHERE singleton=true",
                &[],
            )
            .map_err(storage)?
            .get(0);
        if version != 1 {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "unsupported PostgreSQL authority version",
            ));
        }
        Ok(Self {
            client,
            tenant: tenant.into(),
            project: project.into(),
            artifacts: None,
        })
    }
    pub fn with_artifacts(mut self, artifacts: Rc<dyn ArtifactReader>) -> Self {
        self.artifacts = Some(artifacts);
        self
    }
    fn reader(&self) -> Option<Box<dyn ArtifactReader>> {
        self.artifacts
            .as_ref()
            .map(|r| Box::new(Reader(r.clone())) as Box<dyn ArtifactReader>)
    }
    fn decode(
        bytes: &[u8],
        digest: &str,
        id: &str,
        reader: Option<Box<dyn ArtifactReader>>,
    ) -> Result<SqliteRunStore> {
        if hash(bytes) != digest {
            return Err(corrupt("run image content digest mismatch"));
        }
        let image = RunImage::parse(bytes)?;
        if image.run_id() != id {
            return Err(corrupt("run image identity mismatch"));
        }
        SqliteRunStore::from_image(&image, reader)
    }
    fn read<T>(&mut self, id: &str, f: impl FnOnce(&mut SqliteRunStore) -> Result<T>) -> Result<T> {
        validate_id(id)?;
        let reader = self.reader();
        let mut tx = self
            .client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(storage)?;
        tx.batch_execute("SET LOCAL statement_timeout='30s'; SET LOCAL idle_in_transaction_session_timeout='30s'").map_err(storage)?;
        let row = tx.query_opt("SELECT image,image_digest FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&self.tenant, &self.project, &id]).map_err(storage)?.ok_or_else(|| Error::new(ErrorCode::NotFound, "run not found"))?;
        let mut reducer =
            Self::checked_row(&mut tx, &self.tenant, &self.project, &row, id, reader)?;
        let result = f(&mut reducer)?;
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    fn checked_row(
        tx: &mut Transaction<'_>,
        tenant: &str,
        project: &str,
        row: &postgres::Row,
        id: &str,
        reader: Option<Box<dyn ArtifactReader>>,
    ) -> Result<SqliteRunStore> {
        let bytes: &[u8] = row
            .get::<_, Option<&[u8]>>(0)
            .ok_or_else(|| corrupt("missing committed image"))?;
        let digest: &str = row
            .get::<_, Option<&str>>(1)
            .ok_or_else(|| corrupt("missing committed image digest"))?;
        let reducer = Self::decode(bytes, digest, id, reader)?;
        for b in RunImage::parse(bytes)?.bindings()? {
            let row = tx.query_opt("SELECT digest FROM workflow_authority.bindings WHERE tenant=$1 AND project=$2 AND kind=$3 AND id=$4 AND version=$5", &[&tenant,&project,&b.kind,&b.id,&b.version]).map_err(storage)?;
            if row.as_ref().map(|r| r.get::<_, &str>(0)) != Some(b.digest.as_str()) {
                return Err(corrupt("shared immutable binding is missing or changed"));
            }
        }
        Ok(reducer)
    }
    fn change<T>(
        &mut self,
        id: &str,
        create: bool,
        f: impl FnOnce(&mut SqliteRunStore, &dyn Clock) -> Result<T>,
    ) -> Result<T> {
        validate_id(id)?;
        let reader = self.reader();
        let mut tx = self.client.transaction().map_err(storage)?;
        Self::transaction_settings(&mut tx)?;
        let (result, _, _) =
            Self::change_in(&mut tx, &self.tenant, &self.project, reader, id, create, f)?;
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    fn transaction_settings(tx: &mut Transaction<'_>) -> Result<()> {
        tx.batch_execute("SET LOCAL lock_timeout = '5s'; SET LOCAL statement_timeout = '30s'; SET LOCAL idle_in_transaction_session_timeout = '30s'; SET LOCAL synchronous_commit = on").map_err(storage)
    }
    fn change_in<T>(
        tx: &mut Transaction<'_>,
        tenant: &str,
        project: &str,
        reader: Option<Box<dyn ArtifactReader>>,
        id: &str,
        create: bool,
        f: impl FnOnce(&mut SqliteRunStore, &dyn Clock) -> Result<T>,
    ) -> Result<(T, i64, i64)> {
        validate_id(id)?;
        if create {
            tx.execute("INSERT INTO workflow_authority.runs(tenant,project,run_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[&tenant,&project,&id]).map_err(storage)?;
        }
        let row = tx.query_opt("SELECT image,image_digest,generation FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3 FOR UPDATE", &[&tenant,&project,&id]).map_err(storage)?.ok_or_else(|| Error::new(ErrorCode::NotFound,"run not found"))?;
        let generation: i64 = row.get(2);
        let bytes: Option<&[u8]> = row.get(0);
        let mut reducer = match bytes {
            Some(_) => Self::checked_row(tx, tenant, project, &row, id, reader)?,
            None if create && generation == 0 => SqliteRunStore::image_reducer(reader)?,
            None => return Err(corrupt("incomplete committed run image")),
        };
        let before = bytes.map(|_| reducer.get(id)).transpose()?;
        let (result, observed_at) = {
            let clock = DbClock {
                transaction: RefCell::new(tx),
                last: std::cell::Cell::new(0),
            };
            let result = f(&mut reducer, &clock)?;
            (result, clock.last.get())
        };
        let after = reducer.get(id)?;
        let image = reducer.export_image(id)?;
        // Freeze versions across all runs in this tenant/project in the same transaction.
        // Globally sorted keys give competing starts a consistent lock order.
        bind_versions(tx, tenant, project, image.bindings()?, create)?;
        let bytes = image.bytes()?;
        let digest = hash(&bytes);
        let window = reducer.take_admission_window();
        let lower = window
            .map_or(after.now_unix_ms, |w| {
                w.not_before_unix_ms.max(after.now_unix_ms)
            })
            .max(observed_at);
        let mut upper = window.map_or(i64::MAX as u64, |w| w.expires_at_unix_ms);
        if before.as_ref().is_none_or(|s| s.revision != after.revision)
            && let Some(deadline) = workflow_kernel::signal_admission_deadline(&after)
        {
            upper = upper.min(deadline);
        }
        let lower = i64::try_from(lower).map_err(|_| corrupt("image time out of range"))?;
        let upper = i64::try_from(upper).map_err(|_| corrupt("image deadline out of range"))?;
        // The deadline check occurs in PostgreSQL after serialization and binding
        // locks. No caller clock can extend the exclusive admission deadline.
        let changed = tx.execute("WITH admission AS MATERIALIZED (SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint AS at) UPDATE workflow_authority.runs SET image=$4,image_digest=$5,generation=generation+1 FROM admission WHERE tenant=$1 AND project=$2 AND run_id=$3 AND generation=$6 AND admission.at >= $7 AND admission.at < $8", &[&tenant,&project,&id,&bytes,&digest,&generation,&lower,&upper]).map_err(storage)?;
        if changed != 1 {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "authority changed or admission expired before database write",
            ));
        }
        let activating = before.as_ref().is_none_or(|s| {
            !matches!(s.status, RunStatus::Running | RunStatus::Cancelling) || s.pause.is_some()
        });
        access::scheduling::sync(tx, tenant, project, &after, activating)?;
        Ok((result, lower, upper))
    }
}

#[cfg(test)]
mod tests;
