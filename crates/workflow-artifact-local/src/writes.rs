use crate::*;
use serde::Serialize;
use std::{
    io::{Read, Write},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
#[derive(Clone, Debug, Serialize)]
pub struct CleanupReport {
    pub removed_uploads: u64,
    pub removed_orphan_objects: u64,
}
impl LocalArtifactStore {
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
            .map_err(io)?;
        let r = reference(spec, &bytes)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let records = catalog::read(&tx)?;
        catalog::dependencies(&records, &r)?;
        for input in &spec.inputs {
            verify_graph(&self.root, &records, input)?;
        }
        if let Some(old) = records.get(&r.artifact_id) {
            if old != &r {
                return Err(corrupt("artifact ID conflict"));
            }
            files::content(&self.root, old)?;
            tx.commit().map_err(sql)?;
            return Ok(old.clone());
        }
        files::directory(&self.root.join("uploads"))?;
        files::directory(&self.root.join("objects"))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::new(ErrorCode::Storage, e.to_string()))?
            .as_nanos();
        let temp = self.root.join("uploads").join(format!(
            "upload-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = files::create_file(&temp)?;
        f.write_all(&bytes[..bytes.len() / 2]).map_err(io)?;
        hook("upload_half");
        f.write_all(&bytes[bytes.len() / 2..]).map_err(io)?;
        f.sync_all().map_err(io)?;
        hook("upload_synced");
        let object = files::object(&self.root, &r);
        match std::fs::hard_link(&temp, &object) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                files::content(&self.root, &r)?;
            }
            Err(e) => return Err(io(e)),
        }
        std::fs::remove_file(&temp).map_err(io)?;
        files::sync_dir(&self.root.join("objects"))?;
        files::sync_dir(&self.root.join("uploads"))?;
        hook("object_published");
        catalog::append(&tx, &records, &r)?;
        hook("manifest_written");
        tx.commit().map_err(sql)?;
        hook("after_commit");
        Ok(r)
    }
    /// Serializes with publication, verifies the complete catalog and its dependencies,
    /// and removes only files not referenced by any committed manifest. Never deletes a manifest.
    pub fn cleanup_orphans(&mut self) -> Result<CleanupReport> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let records = catalog::read(&tx)?;
        let mut retained = BTreeSet::new();
        for r in records.values() {
            files::content(&self.root, r)?;
            retained.insert(r.manifest.content_digest[7..].to_string());
        }
        let mut report = CleanupReport {
            removed_uploads: 0,
            removed_orphan_objects: 0,
        };
        for name in ["uploads", "objects"] {
            let dir = self.root.join(name);
            files::directory(&dir)?;
            let mut remove = vec![];
            for entry in std::fs::read_dir(&dir).map_err(io)? {
                let e = entry.map_err(io)?;
                files::regular(&e.path())?;
                let file = e
                    .file_name()
                    .into_string()
                    .map_err(|_| corrupt("non-UTF8 artifact filename"))?;
                let valid = if name == "uploads" {
                    file.starts_with("upload-")
                        && file[7..].bytes().all(|b| b.is_ascii_digit() || b == b'-')
                } else {
                    file.len() == 64
                        && file
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                };
                if !valid {
                    return Err(corrupt("unexpected file in artifact directory"));
                }
                if name == "uploads" || !retained.contains(&file) {
                    remove.push(e.path());
                }
            }
            for path in remove {
                std::fs::remove_file(path).map_err(io)?;
                if name == "uploads" {
                    report.removed_uploads += 1
                } else {
                    report.removed_orphan_objects += 1
                }
            }
            files::sync_dir(&dir)?;
        }
        tx.commit().map_err(sql)?;
        Ok(report)
    }
}
