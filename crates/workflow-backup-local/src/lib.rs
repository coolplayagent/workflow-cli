//! Consistent local SQLite and immutable artifact backup adapter (Linux).
mod files;
mod sqlite;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use workflow_artifact_local::LocalArtifactStore;
use workflow_artifacts::{ArtifactReader, ArtifactStore};
pub use workflow_backups::*;
use workflow_runstore::{RecoveryBarrier, RunStore};
use workflow_runstore_sqlite::SqliteRunStore;
use workflow_worker::Clock;
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackupSources {
    pub runs: PathBuf,
    pub artifacts: Option<PathBuf>,
    pub registry: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub actor: String,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreReport {
    pub source_backup_digest: String,
    pub generation: String,
    pub restored_at_unix_ms: u64,
    pub recovery: Vec<(String, RecoveryBarrier)>,
}
pub struct LocalBackup {
    root: PathBuf,
    index: BackupIndex,
}
impl LocalBackup {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        files::directory(root.as_ref())?;
        let root = std::fs::canonicalize(root).map_err(files::io)?;
        let index: BackupIndex = serde_json::from_slice(&files::read(
            &root.join("backup.json"),
            MAX_MANIFEST_BYTES as u64,
        )?)
        .map_err(files::corrupt)?;
        index.validate()?;
        let archive = Self { root, index };
        archive.verify()?;
        Ok(archive)
    }
    pub fn verify(&self) -> Result<()> {
        self.index.validate()?;
        if files::inventory(&self.root)? != self.index.manifest.files {
            return Err(files::corrupt(
                "backup inventory has missing, extra or changed bytes",
            ));
        }
        let m = &self.index.manifest;
        if sqlite::verify(&self.root.join("runs.sqlite"))? != m.run_storage_version {
            return Err(files::corrupt("run storage version differs from inventory"));
        }
        if self.root.join("artifacts").exists() != m.artifact_storage_version.is_some() {
            return Err(files::corrupt(
                "artifact directory does not match inventory",
            ));
        }
        if let Some(version) = m.artifact_storage_version {
            let artifacts = self.root.join("artifacts");
            if sqlite::verify(&artifacts.join("catalog.sqlite"))? != version {
                return Err(files::corrupt(
                    "artifact storage version differs from inventory",
                ));
            }
            let store = LocalArtifactStore::open(&artifacts).map_err(files::corrupt)?;
            let manifests = store.retained_manifests().map_err(files::corrupt)?;
            if manifests.len() != m.artifact_manifests as usize {
                return Err(files::corrupt(
                    "artifact manifest count differs from inventory",
                ));
            }
            for r in manifests {
                store.verify(&r.link()).map_err(files::corrupt)?;
            }
        }
        if let Some(version) = m.registry_storage_version {
            let path = self.root.join("registry.sqlite");
            if sqlite::verify(&path)? != version {
                return Err(files::corrupt(
                    "registry storage version differs from inventory",
                ));
            }
            workflow_registry_sqlite::SqliteRegistry::open_readonly(path)
                .map_err(files::corrupt)?
                .verify_all()
                .map_err(files::corrupt)?;
        }
        if run_images(&self.root, m.artifact_storage_version.is_some())? != m.runs {
            return Err(files::corrupt("run recovery differs from backup inventory"));
        }
        Ok(())
    }
}
impl BackupReader for LocalBackup {
    fn index(&self) -> Result<BackupIndex> {
        Ok(self.index.clone())
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let image = self
            .index
            .manifest
            .files
            .get(path)
            .ok_or_else(|| files::corrupt("file is not in backup inventory"))?;
        let bytes = files::read(&self.root.join(path), image.bytes)?;
        if bytes.len() as u64 != image.bytes || bytes_digest(&bytes) != image.digest {
            return Err(files::corrupt("backup file changed after verification"));
        }
        Ok(bytes)
    }
}
fn open_runs(root: &Path, artifacts: bool) -> Result<SqliteRunStore> {
    let mut store = SqliteRunStore::open(root.join("runs.sqlite")).map_err(files::corrupt)?;
    if artifacts {
        store = store.with_artifacts(Box::new(
            LocalArtifactStore::open(root.join("artifacts")).map_err(files::corrupt)?,
        ));
    }
    Ok(store)
}
fn run_images(root: &Path, artifacts: bool) -> Result<Vec<RunImage>> {
    let mut store = open_runs(root, artifacts)?;
    let mut cursor = None;
    let mut result = vec![];
    loop {
        let page = store.list(cursor.as_deref(), 100).map_err(files::corrupt)?;
        for r in page.items {
            let snapshot = store.get(&r.run_id).map_err(files::corrupt)?;
            let proof = store.verify(&r.run_id).map_err(files::corrupt)?;
            result.push(RunImage {
                run_id: r.run_id,
                run_digest: snapshot.run_digest,
                bundle_digest: snapshot.bundle_digest,
                revision: proof.revision,
                state_digest: proof.state_digest,
            });
            if result.len() > MAX_RUNS {
                return Err(Error::new(ErrorCode::Budget, "backup exceeds 10000 runs"));
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(result)
}
fn artifact_directories(root: &Path) -> Result<()> {
    for dir in ["artifacts", "artifacts/objects", "artifacts/uploads"] {
        files::create_dir(&root.join(dir))?;
    }
    Ok(())
}
pub fn create(
    sources: &BackupSources,
    destination: impl AsRef<Path>,
    clock: &dyn Clock,
) -> Result<BackupIndex> {
    create_internal(sources, destination.as_ref(), clock, |_| {})
}
fn create_internal(
    sources: &BackupSources,
    destination: &Path,
    clock: &dyn Clock,
    hook: impl Fn(&str),
) -> Result<BackupIndex> {
    let started = clock.now_unix_ms().map_err(files::io)?;
    let staging = files::staging(destination)?;
    let version = sqlite::snapshot(&sources.runs, &staging.join("runs.sqlite"))?;
    hook("run_snapshot");
    let mut artifact_manifests = 0;
    let artifact_version = if let Some(source) = &sources.artifacts {
        artifact_directories(&staging)?;
        let version = sqlite::snapshot(
            &source.join("catalog.sqlite"),
            &staging.join("artifacts/catalog.sqlite"),
        )?;
        let catalog =
            LocalArtifactStore::open(staging.join("artifacts")).map_err(files::corrupt)?;
        let source = LocalArtifactStore::open(source).map_err(files::corrupt)?;
        let manifests = catalog.retained_manifests().map_err(files::corrupt)?;
        artifact_manifests = manifests.len() as u32;
        let mut copied = std::collections::BTreeSet::new();
        let mut bytes_total = 0u64;
        for r in manifests {
            if copied.insert(r.manifest.content_digest.clone()) {
                bytes_total = bytes_total
                    .checked_add(r.manifest.bytes)
                    .ok_or_else(|| files::corrupt("artifact byte count overflow"))?;
                if bytes_total > MAX_ARCHIVE_BYTES {
                    return Err(Error::new(
                        ErrorCode::Budget,
                        "artifact backup exceeds 1 GiB",
                    ));
                }
                let bytes = source.read(&r.link()).map_err(files::corrupt)?;
                files::write(
                    &staging
                        .join("artifacts/objects")
                        .join(&r.manifest.content_digest[7..]),
                    &bytes,
                )?;
            }
        }
        files::sync_dir(&staging.join("artifacts/objects"))?;
        files::sync_dir(&staging.join("artifacts"))?;
        Some(version)
    } else {
        None
    };
    hook("artifacts_copied");
    let registry_version = sources
        .registry
        .as_ref()
        .map(|source| sqlite::snapshot(source, &staging.join("registry.sqlite")))
        .transpose()?;
    let runs = run_images(&staging, artifact_version.is_some())?;
    let index = BackupIndex::seal(BackupManifest {
        schema_version: 1,
        created_at_unix_ms: started,
        completed_at_unix_ms: clock.now_unix_ms().map_err(files::io)?,
        run_storage_version: version,
        artifact_storage_version: artifact_version,
        registry_storage_version: registry_version,
        artifact_manifests,
        runs,
        files: files::inventory(&staging)?,
    })?;
    let candidate = LocalBackup {
        root: staging.clone(),
        index: index.clone(),
    };
    candidate.verify()?;
    hook("before_manifest");
    files::publish_json(&staging, "backup.json", &index)?;
    hook("before_publish");
    files::publish_directory(&staging, destination)?;
    hook("after_publish");
    Ok(index)
}
pub fn restore(
    backup: &LocalBackup,
    destination: impl AsRef<Path>,
    request: &RestoreRequest,
    clock: &dyn Clock,
) -> Result<RestoreReport> {
    restore_internal(backup, destination.as_ref(), request, clock, |_| {})
}
fn restore_internal(
    backup: &LocalBackup,
    destination: &Path,
    request: &RestoreRequest,
    clock: &dyn Clock,
    hook: impl Fn(&str),
) -> Result<RestoreReport> {
    if request.actor.trim().is_empty()
        || request.actor.len() > 128
        || request.reason.trim().is_empty()
        || request.reason.len() > 1024
    {
        return Err(Error::new(
            ErrorCode::InvalidArchive,
            "restore requires a bounded operator annotation and reason",
        ));
    }
    backup.verify()?;
    let staging = files::staging(destination)?;
    let artifacts = backup.index.manifest.artifact_storage_version.is_some();
    if artifacts {
        artifact_directories(&staging)?;
    }
    for path in backup.index.manifest.files.keys() {
        files::write(&staging.join(path), &backup.read_file(path)?)?;
    }
    let candidate = LocalBackup {
        root: staging.clone(),
        index: backup.index.clone(),
    };
    candidate.verify()?;
    let generation = files::generation()?;
    hook("before_fence");
    let recovery = open_runs(&staging, artifacts)?
        .fence_restored_runs(
            &backup.index.digest,
            &generation,
            &request.actor,
            &request.reason,
            clock,
        )
        .map_err(files::corrupt)?;
    hook("after_fence");
    run_images(&staging, artifacts)?;
    std::fs::File::open(staging.join("runs.sqlite"))
        .map_err(files::io)?
        .sync_all()
        .map_err(files::io)?;
    if artifacts {
        files::sync_dir(&staging.join("artifacts/objects"))?;
        files::sync_dir(&staging.join("artifacts/uploads"))?;
        files::sync_dir(&staging.join("artifacts"))?;
    }
    let report = RestoreReport {
        source_backup_digest: backup.index.digest.clone(),
        generation,
        restored_at_unix_ms: clock.now_unix_ms().map_err(files::io)?,
        recovery,
    };
    files::publish_json(&staging, "restore.json", &report)?;
    hook("before_publish");
    files::publish_directory(&staging, destination)?;
    hook("after_publish");
    Ok(report)
}

#[cfg(test)]
mod tests;
