//! Portable backup inventory. Content integrity does not authenticate an operator.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_DATABASE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_FILES: usize = 10003;
pub const MAX_RUNS: usize = 10000;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileImage {
    pub bytes: u64,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunImage {
    pub run_id: String,
    pub run_digest: String,
    pub bundle_digest: String,
    pub revision: u64,
    pub state_digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub schema_version: u32,
    pub created_at_unix_ms: u64,
    pub completed_at_unix_ms: u64,
    pub run_storage_version: u32,
    pub artifact_storage_version: Option<u32>,
    pub registry_storage_version: Option<u32>,
    pub artifact_manifests: u32,
    pub runs: Vec<RunImage>,
    pub files: BTreeMap<String, FileImage>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackupIndex {
    pub manifest: BackupManifest,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArchive,
    CorruptArchive,
    UnsupportedArchive,
    Storage,
    Budget,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}
impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
pub fn bytes_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
pub fn digest<T: Serialize>(value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|e| Error::new(ErrorCode::InvalidArchive, e.to_string()))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Error::new(
            ErrorCode::Budget,
            "backup manifest exceeds 8 MiB",
        ));
    }
    Ok(bytes_digest(&bytes))
}
fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn hash(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(hex)
}
pub fn allowed_file(path: &str) -> bool {
    matches!(
        path,
        "runs.sqlite" | "registry.sqlite" | "artifacts/catalog.sqlite"
    ) || path.strip_prefix("artifacts/objects/").is_some_and(hex)
}
impl BackupIndex {
    pub fn seal(manifest: BackupManifest) -> Result<Self> {
        let index = Self {
            digest: digest(&manifest)?,
            manifest,
        };
        index.validate()?;
        Ok(index)
    }
    pub fn validate(&self) -> Result<()> {
        let m = &self.manifest;
        if m.schema_version != 1
            || m.created_at_unix_ms == 0
            || m.completed_at_unix_ms < m.created_at_unix_ms
            || m.run_storage_version == 0
            || self.digest != digest(m)?
        {
            return Err(Error::new(
                ErrorCode::InvalidArchive,
                "backup schema, time or inventory digest mismatch",
            ));
        }
        if m.files.is_empty()
            || m.files.len() > MAX_FILES
            || m.runs.len() > MAX_RUNS
            || m.artifact_manifests > 10000
        {
            return Err(Error::new(
                ErrorCode::Budget,
                "backup inventory count exceeds its bounds",
            ));
        }
        if !m.files.contains_key("runs.sqlite")
            || m.files.contains_key("registry.sqlite") != m.registry_storage_version.is_some()
            || m.files.contains_key("artifacts/catalog.sqlite")
                != m.artifact_storage_version.is_some()
            || m.registry_storage_version == Some(0)
            || m.artifact_storage_version == Some(0)
            || (m.artifact_storage_version.is_none()
                && (m.artifact_manifests != 0
                    || m.files.keys().any(|p| p.starts_with("artifacts/"))))
        {
            return Err(Error::new(
                ErrorCode::InvalidArchive,
                "backup is missing a required database or has unbound artifacts",
            ));
        }
        let mut total = 0u64;
        for (path, file) in &m.files {
            if !allowed_file(path)
                || !hash(&file.digest)
                || (!path.ends_with(".sqlite") && file.bytes > 64 * 1024 * 1024)
                || (path.ends_with(".sqlite")
                    && (file.bytes == 0 || file.bytes > MAX_DATABASE_BYTES))
            {
                return Err(Error::new(
                    ErrorCode::InvalidArchive,
                    "invalid backup path, digest or database size",
                ));
            }
            total = total
                .checked_add(file.bytes)
                .ok_or_else(|| Error::new(ErrorCode::Budget, "backup size overflow"))?;
            if total > MAX_ARCHIVE_BYTES {
                return Err(Error::new(ErrorCode::Budget, "backup exceeds 1 GiB"));
            }
        }
        let mut seen = BTreeSet::new();
        for r in &m.runs {
            if r.run_id.is_empty()
                || r.run_id.len() > 128
                || !seen.insert(&r.run_id)
                || r.revision == 0
                || !hash(&r.run_digest)
                || !hash(&r.bundle_digest)
                || !hash(&r.state_digest)
            {
                return Err(Error::new(
                    ErrorCode::InvalidArchive,
                    "invalid backup run identity",
                ));
            }
        }
        Ok(())
    }
}
/// Readers must enforce inventory integrity and access scope before returning bytes.
pub trait BackupReader {
    fn index(&self) -> Result<BackupIndex>;
    fn read_file(&self, path: &str) -> Result<Vec<u8>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> BackupManifest {
        BackupManifest {
            schema_version: 1,
            created_at_unix_ms: 1,
            completed_at_unix_ms: 1,
            run_storage_version: 10,
            artifact_storage_version: None,
            registry_storage_version: None,
            artifact_manifests: 0,
            runs: vec![],
            files: [(
                "runs.sqlite".into(),
                FileImage {
                    bytes: 4096,
                    digest: bytes_digest(b"unit shape only"),
                },
            )]
            .into(),
        }
    }
    #[test]
    fn inventory_rejects_traversal_unbound_stores_oversize_and_changed_content() {
        let valid = BackupIndex::seal(manifest()).unwrap();
        valid.validate().unwrap();
        for path in [
            "../runs.sqlite",
            "/runs.sqlite",
            "artifacts/objects/../../secret",
            "artifacts/objects/not-a-digest",
            "registry.sqlite",
        ] {
            let mut m = manifest();
            m.files.insert(
                path.into(),
                FileImage {
                    bytes: 1,
                    digest: bytes_digest(b"x"),
                },
            );
            assert!(BackupIndex::seal(m).is_err(), "accepted {path}");
        }
        let mut m = manifest();
        m.files.get_mut("runs.sqlite").unwrap().bytes = MAX_DATABASE_BYTES + 1;
        assert!(BackupIndex::seal(m).is_err());
        let mut altered = valid;
        altered.manifest.created_at_unix_ms += 1;
        assert!(altered.validate().is_err());
        let mut m = manifest();
        m.registry_storage_version = Some(1);
        assert!(BackupIndex::seal(m).is_err());
    }
}
