//! Local immutable artifacts: durable files plus a transactional manifest catalog.
mod catalog;
mod files;
mod writes;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};
use workflow_artifacts::*;
pub use writes::CleanupReport;
pub struct LocalArtifactStore {
    root: PathBuf,
    connection: Connection,
}
impl LocalArtifactStore {
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.exists() {
            files::create_directory(root)?;
            files::sync_dir(root.parent().unwrap_or(Path::new(".")))?;
        }
        files::directory(root)?;
        for entry in std::fs::read_dir(root).map_err(io)? {
            let e = entry.map_err(io)?;
            if !matches!(
                e.file_name().to_str(),
                Some("catalog.sqlite" | "catalog.sqlite-journal" | "objects" | "uploads")
            ) {
                return Err(Error::new(
                    ErrorCode::UnsupportedStorage,
                    "refusing a foreign nonempty artifact directory",
                ));
            }
        }
        for name in ["objects", "uploads"] {
            let path = root.join(name);
            if !path.exists() {
                files::create_directory(&path)?;
            }
            files::directory(&path)?;
        }
        files::sync_dir(root)?;
        let path = root.join("catalog.sqlite");
        if path.exists() {
            files::regular(&path)?;
        } else {
            files::create_file(&path)?.sync_all().map_err(io)?;
        }
        let mut connection = connect(&path, true)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(sql)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(sql)?;
        if app == 0 && version == 0 {
            let count: i64 = tx
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
                .map_err(sql)?;
            if count != 0 {
                return Err(Error::new(
                    ErrorCode::UnsupportedStorage,
                    "foreign artifact catalog",
                ));
            }
            if std::fs::read_dir(root.join("objects"))
                .map_err(io)?
                .next()
                .is_some()
                || std::fs::read_dir(root.join("uploads"))
                    .map_err(io)?
                    .next()
                    .is_some()
            {
                return Err(Error::new(
                    ErrorCode::UnsupportedStorage,
                    "uninitialized catalog with existing artifact data",
                ));
            }
            tx.execute_batch(catalog::SCHEMA).map_err(sql)?;
            tx.execute(
                "INSERT INTO head(id,sequence,chain) VALUES(1,0,?1)",
                [digest(&"empty artifact catalog")?],
            )
            .map_err(sql)?;
            tx.pragma_update(None, "application_id", catalog::APPLICATION_ID)
                .map_err(sql)?;
            tx.pragma_update(None, "user_version", 1).map_err(sql)?;
        } else {
            catalog::version(&tx)?;
        }
        tx.commit().map_err(sql)?;
        files::sync_dir(root)?;
        Ok(Self {
            root: std::fs::canonicalize(root).map_err(io)?,
            connection,
        })
    }
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        files::directory(root)?;
        for name in ["objects", "uploads"] {
            files::directory(&root.join(name))?;
        }
        files::regular(&root.join("catalog.sqlite"))?;
        let connection = connect(&root.join("catalog.sqlite"), false)?;
        catalog::version(&connection)?;
        Ok(Self {
            root: std::fs::canonicalize(root).map_err(io)?,
            connection,
        })
    }
    /// Catalog-verified manifests from one snapshot. Call `verify`/`read` for
    /// payload verification; metadata alone does not prove content availability.
    pub fn retained_manifests(&self) -> Result<Vec<ArtifactRef>> {
        let tx = self.connection.unchecked_transaction().map_err(sql)?;
        let records = catalog::read(&tx)?;
        tx.commit().map_err(sql)?;
        Ok(records.into_values().collect())
    }
    pub fn resolve(&self, id: &str) -> Result<ArtifactRef> {
        self.verify(&link_for_id(id)?)
    }
    /// Return retained downstream artifacts affected by replacing this immutable input.
    /// This is a revalidation projection, not permission to rewrite committed run history.
    pub fn impact(&self, link: &ArtifactLink) -> Result<Vec<ArtifactRef>> {
        let tx = self.connection.unchecked_transaction().map_err(sql)?;
        let records = catalog::read(&tx)?;
        verify_graph(&self.root, &records, link)?;
        let mut pending = vec![link.artifact_id.clone()];
        let mut affected = BTreeSet::new();
        while let Some(id) = pending.pop() {
            for r in records
                .values()
                .filter(|r| r.manifest.spec.inputs.iter().any(|i| i.artifact_id == id))
            {
                if affected.insert(r.artifact_id.clone()) {
                    pending.push(r.artifact_id.clone());
                }
                if affected.len() > MAX_LINEAGE {
                    return Err(Error::new(
                        ErrorCode::Budget,
                        "impact exceeds 512 artifacts",
                    ));
                }
            }
        }
        let mut result = vec![];
        for id in affected {
            let r = &records[&id];
            verify_graph(&self.root, &records, &r.link())?;
            result.push(r.clone());
        }
        tx.commit().map_err(sql)?;
        Ok(result)
    }
    pub fn lineage(&self, link: &ArtifactLink) -> Result<Vec<ArtifactRef>> {
        let tx = self.connection.unchecked_transaction().map_err(sql)?;
        let records = catalog::read(&tx)?;
        let result = verify_graph(&self.root, &records, link)?;
        tx.commit().map_err(sql)?;
        Ok(result)
    }
}
impl ArtifactReader for LocalArtifactStore {
    fn verify(&self, r: &ArtifactLink) -> Result<ArtifactRef> {
        let graph = self.lineage(r)?;
        Ok(graph.last().expect("root artifact").clone())
    }
}
impl ArtifactInventory for LocalArtifactStore {
    fn retained_manifests(&self) -> Result<Vec<ArtifactRef>> {
        LocalArtifactStore::retained_manifests(self)
    }
}
impl ArtifactStore for LocalArtifactStore {
    fn publish(
        &mut self,
        spec: &PublishSpec,
        content: &mut dyn std::io::Read,
    ) -> Result<ArtifactRef> {
        self.publish_internal(spec, content, |_| {})
    }
    fn read(&self, link: &ArtifactLink) -> Result<Vec<u8>> {
        let r = self.verify(link)?;
        files::content(&self.root, &r)
    }
}
pub fn link_for_id(id: &str) -> Result<ArtifactLink> {
    let hash = id
        .strip_prefix("artifact-")
        .ok_or_else(|| Error::new(ErrorCode::InvalidReference, "artifact ID required"))?;
    let link = ArtifactLink {
        artifact_id: id.into(),
        digest: format!("sha256:{hash}"),
    };
    validate_link(&link)?;
    Ok(link)
}
fn verify_graph(
    root: &Path,
    records: &BTreeMap<String, ArtifactRef>,
    link: &ArtifactLink,
) -> Result<Vec<ArtifactRef>> {
    validate_link(link)?;
    let mut pending = vec![(link.clone(), false)];
    let mut visited = BTreeSet::new();
    let mut result = vec![];
    while let Some((l, done)) = pending.pop() {
        let r = records
            .get(&l.artifact_id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "artifact manifest is missing"))?;
        if r.link() != l {
            return Err(corrupt("artifact link differs from catalog"));
        }
        if done {
            files::content(root, r)?;
            result.push(r.clone());
            continue;
        }
        if !visited.insert(l.artifact_id.clone()) {
            continue;
        }
        if visited.len() > MAX_LINEAGE {
            return Err(Error::new(
                ErrorCode::Budget,
                "artifact lineage exceeds 512 unique objects",
            ));
        }
        pending.push((l, true));
        for input in r.manifest.spec.inputs.iter().rev() {
            pending.push((input.clone(), false));
        }
    }
    Ok(result)
}
fn connect(path: &Path, create: bool) -> Result<Connection> {
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let c = Connection::open_with_flags(path, flags).map_err(sql)?;
    c.busy_timeout(Duration::from_secs(5)).map_err(sql)?;
    c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(sql)?;
    Ok(c)
}
fn io(e: std::io::Error) -> Error {
    Error::new(
        if e.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::NotFound
        } else {
            ErrorCode::Storage
        },
        e.to_string(),
    )
}
fn sql(e: rusqlite::Error) -> Error {
    let code = match &e {
        rusqlite::Error::SqliteFailure(c, _)
            if matches!(
                c.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            ErrorCode::Busy
        }
        _ => ErrorCode::Storage,
    };
    Error::new(code, e.to_string())
}
fn corrupt(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::CorruptStorage, message)
}
#[cfg(test)]
mod tests;
