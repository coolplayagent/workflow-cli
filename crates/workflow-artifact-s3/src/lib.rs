//! S3 payloads with an immutable PostgreSQL manifest catalog. Only trusted hosts
//! own these credentials. Worker downloads receive an exact, short-lived GET URL.
mod catalog;
mod s3;
use postgres::{Client, Transaction};
pub use s3::{S3Binding, S3Client, download};
use serde::Serialize;
use std::{cell::RefCell, collections::BTreeMap, io::Read};
use workflow_artifacts::*;

pub struct S3ArtifactStore {
    connection: RefCell<Client>,
    objects: S3Client,
    namespace: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct CleanupReport {
    pub expired_uploads: u64,
    pub orphan_delete_requests: u64,
    pub next_cursor: Option<String>,
}
/// Contains a bearer URL. Deliberately has no Debug implementation.
#[derive(Serialize)]
pub struct Download {
    pub artifact: ArtifactRef,
    pub url: String,
    pub expires_at_unix_ms: u64,
}
impl S3ArtifactStore {
    pub fn create(mut connection: Client, objects: S3Client, namespace: &str) -> Result<Self> {
        validate_namespace(namespace)?;
        let mut tx = connection.transaction().map_err(sql)?;
        tx.batch_execute("SET LOCAL synchronous_commit=on")
            .map_err(sql)?;
        tx.query_one("SELECT pg_advisory_xact_lock(57465330)", &[])
            .map_err(sql)?;
        let exists: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_objects')",
                &[],
            )
            .map_err(sql)?
            .get(0);
        if !exists {
            tx.batch_execute(include_str!("schema.sql")).map_err(sql)?;
        }
        catalog::version(&mut tx)?;
        tx.execute("INSERT INTO workflow_objects.catalogs(namespace,binding,sequence,chain) VALUES($1,$2,0,$3) ON CONFLICT DO NOTHING",
            &[&namespace, &objects.identity()?, &digest(&"empty S3 artifact catalog")?]).map_err(sql)?;
        catalog::lock(&mut tx, namespace, &objects)?;
        catalog::read(&mut tx, namespace)?;
        tx.commit().map_err(sql)?;
        Self::open(connection, objects, namespace)
    }
    pub fn open(connection: Client, objects: S3Client, namespace: &str) -> Result<Self> {
        validate_namespace(namespace)?;
        let store = Self {
            connection: RefCell::new(connection),
            objects,
            namespace: namespace.into(),
        };
        store.with_catalog(|_, _| Ok(()))?;
        Ok(store)
    }
    fn with_catalog<T>(
        &self,
        action: impl FnOnce(&mut Transaction<'_>, &catalog::Records) -> Result<T>,
    ) -> Result<T> {
        let mut client = self.connection.borrow_mut();
        let mut tx = client.transaction().map_err(sql)?;
        catalog::lock(&mut tx, &self.namespace, &self.objects)?;
        let records = catalog::read(&mut tx, &self.namespace)?;
        let value = action(&mut tx, &records)?;
        tx.commit().map_err(sql)?;
        Ok(value)
    }
    pub fn lineage(&self, link: &ArtifactLink) -> Result<Vec<ArtifactRef>> {
        self.with_catalog(|_, records| self.graph(records, link))
    }
    pub fn retained_manifests(&self) -> Result<Vec<ArtifactRef>> {
        self.with_catalog(|_, records| Ok(records.values().map(|r| r.reference.clone()).collect()))
    }
    fn graph(&self, records: &catalog::Records, link: &ArtifactLink) -> Result<Vec<ArtifactRef>> {
        validate_link(link)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut pending = vec![(link.clone(), false)];
        let mut values = vec![];
        while let Some((link, done)) = pending.pop() {
            let record = records
                .get(&link.artifact_id)
                .ok_or_else(|| Error::new(ErrorCode::NotFound, "artifact manifest missing"))?;
            if record.reference.link() != link {
                return Err(corrupt("artifact link mismatch"));
            }
            if done {
                self.content(record)?;
                values.push(record.reference.clone());
            } else if seen.insert(link.artifact_id.clone()) {
                if seen.len() > MAX_LINEAGE {
                    return Err(Error::new(
                        ErrorCode::Budget,
                        "lineage exceeds 512 artifacts",
                    ));
                }
                pending.push((link, true));
                pending.extend(
                    record
                        .reference
                        .manifest
                        .spec
                        .inputs
                        .iter()
                        .rev()
                        .map(|l| (l.clone(), false)),
                );
            }
        }
        Ok(values)
    }
    fn content(&self, record: &catalog::Record) -> Result<Vec<u8>> {
        let bytes = self.objects.get(&record.key)?;
        if reference(&record.reference.manifest.spec, &bytes)? != record.reference {
            return Err(corrupt("S3 content differs from immutable manifest"));
        }
        Ok(bytes)
    }
    pub(crate) fn publish_internal(
        &mut self,
        spec: &PublishSpec,
        reader: &mut dyn Read,
        hook: impl Fn(&str),
    ) -> Result<ArtifactRef> {
        validate_spec(spec)?;
        let mut bytes = vec![];
        reader
            .take(MAX_CONTENT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        let reference = reference(spec, &bytes)?;
        let key = random_key(&self.namespace)?;
        // Reserve the key durably before any HTTP request. An interrupted PUT
        // cannot leave an undiscoverable payload. This key is never reused.
        let existing = self.with_catalog(|tx, records| {
            catalog::validate_append(records, &reference)?;
            for link in &spec.inputs { self.graph(records, link)?; }
            if let Some(old) = records.get(&reference.artifact_id) {
                self.content(old)?;
                return Ok(Some(old.reference.clone()));
            }
            let count: i64 = tx.query_one("SELECT count(*) FROM workflow_objects.uploads WHERE namespace=$1", &[&self.namespace]).map_err(sql)?.get(0);
            if count >= 10000 { return Err(Error::new(ErrorCode::Budget, "orphan reservation catalog is full")); }
            let expires = catalog::now(tx)?.checked_add(300_000).ok_or_else(unavailable)?;
            tx.execute("INSERT INTO workflow_objects.uploads(namespace,object_key,expires_at) VALUES($1,$2,$3)", &[&self.namespace,&key,&expires]).map_err(sql)?;
            Ok(None)
        })?;
        if let Some(old) = existing {
            return Ok(old);
        }
        hook("reserved");
        self.with_catalog(|tx, records| {
            // Serializes cooperating publication/cleanup, including independent hosts.
            let row = tx.query_one("SELECT expires_at,abandoned FROM workflow_objects.uploads WHERE namespace=$1 AND object_key=$2 FOR UPDATE", &[&self.namespace,&key]).map_err(sql)?;
            let expires: i64 = row.get(0);
            if row.get::<_,bool>(1) || catalog::now(tx)? >= expires { return Err(Error::new(ErrorCode::Busy, "upload reservation expired")); }
            catalog::validate_append(records, &reference)?;
            for input in &spec.inputs { self.graph(records, input)?; }
            if let Some(old) = records.get(&reference.artifact_id) {
                self.content(old)?;
                tx.execute("UPDATE workflow_objects.uploads SET abandoned=true WHERE object_key=$1", &[&key]).map_err(sql)?;
                return Ok(old.reference.clone());
            }
            self.objects.put(&key, &bytes)?;
            hook("object_uploaded");
            if self.objects.get(&key)? != bytes { return Err(corrupt("S3 did not retain the uploaded bytes")); }
            if catalog::now(tx)? >= expires { return Err(Error::new(ErrorCode::Busy, "upload expired before commit")); }
            catalog::append(tx, &self.namespace, records, &reference, &key)?;
            tx.execute("DELETE FROM workflow_objects.uploads WHERE object_key=$1", &[&key]).map_err(sql)?;
            hook("manifest_written");
            Ok(reference.clone())
        })?;
        hook("after_commit");
        Ok(reference)
    }
    /// Host authorization must precede this call. A worker gets one exact GET resource,
    /// no list/write/catalog credentials. Every ancestor must separately be granted.
    pub fn grant_download(&self, link: &ArtifactLink, seconds: u32) -> Result<Download> {
        self.with_catalog(|_, records| {
            let artifact = self.graph(records, link)?.pop().expect("root");
            let (url, expires_at_unix_ms) = self
                .objects
                .presign(&records[&link.artifact_id].key, seconds)?;
            Ok(Download {
                artifact,
                url,
                expires_at_unix_ms,
            })
        })
    }
    /// Abandoned reservation tombstones stay queryable. Repeated deletion catches
    /// a PUT accepted after its client's process/DB connection died. Keys are
    /// unique per publication and are never adopted by a later attempt.
    pub fn cleanup_orphans(&mut self, after: Option<&str>, limit: u32) -> Result<CleanupReport> {
        if !(1..=100).contains(&limit)
            || after.is_some_and(|k| !catalog::valid_key(k, &self.namespace))
        {
            return Err(invalid("invalid cleanup cursor or page size"));
        }
        self.with_catalog(|tx, records| {
            for record in records.values() { self.content(record)?; }
            let at = catalog::now(tx)?;
            let keys = tx.query("SELECT object_key,abandoned FROM workflow_objects.uploads WHERE namespace=$1 AND object_key>$2 AND (abandoned OR expires_at<=$3) ORDER BY object_key COLLATE \"C\" LIMIT $4 FOR UPDATE", &[&self.namespace,&after.unwrap_or(""),&at,&(i64::from(limit)+1)]).map_err(sql)?;
            let mut report = CleanupReport { expired_uploads: 0, orphan_delete_requests: 0, next_cursor: None };
            for row in keys.iter().take(limit as usize) {
                let key: String = row.get(0);
                if !catalog::valid_key(&key,&self.namespace) || records.values().any(|r| r.key == key) { return Err(corrupt("orphan reservation overlaps retained content")); }
                if !row.get::<_,bool>(1) {
                    tx.execute("UPDATE workflow_objects.uploads SET abandoned=true WHERE object_key=$1", &[&key]).map_err(sql)?;
                    report.expired_uploads += 1;
                }
                self.objects.delete(&key)?;
                report.orphan_delete_requests += 1;
                if keys.len() > limit as usize { report.next_cursor = Some(key); }
            }
            Ok(report)
        })
    }
}
impl ArtifactReader for S3ArtifactStore {
    fn verify(&self, link: &ArtifactLink) -> Result<ArtifactRef> {
        Ok(self.lineage(link)?.pop().expect("root"))
    }
}
impl ArtifactInventory for S3ArtifactStore {
    fn retained_manifests(&self) -> Result<Vec<ArtifactRef>> {
        S3ArtifactStore::retained_manifests(self)
    }
}
impl ArtifactStore for S3ArtifactStore {
    fn publish(&mut self, spec: &PublishSpec, content: &mut dyn Read) -> Result<ArtifactRef> {
        self.publish_internal(spec, content, |_| {})
    }
    fn read(&self, link: &ArtifactLink) -> Result<Vec<u8>> {
        self.with_catalog(|_, records| {
            self.graph(records, link)?;
            self.content(&records[&link.artifact_id])
        })
    }
}
fn random_key(namespace: &str) -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| unavailable())?;
    Ok(format!(
        "{namespace}/upload-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
fn validate_namespace(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(invalid(
            "namespace requires 1..64 ASCII letters, digits or hyphens",
        ));
    }
    Ok(())
}
fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidContract, message)
}
fn corrupt(message: &str) -> Error {
    Error::new(ErrorCode::CorruptStorage, message)
}
fn unavailable() -> Error {
    Error::new(ErrorCode::Storage, "object storage unavailable")
}
fn sql(_: postgres::Error) -> Error {
    Error::new(ErrorCode::Storage, "object manifest catalog unavailable")
}

#[cfg(test)]
mod tests;
