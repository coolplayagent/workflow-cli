use crate::write;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::io::{Read, Write};
use workflow_backup_local::*;
pub const HELP: &str = "LOCAL BACKUP AND RECOVERY\n  workflow backup create <sources.json> <new-directory>\n  workflow backup inspect <backup-directory>\n  workflow backup verify <backup-directory>\n  workflow backup restore <backup-directory> <new-directory> <restore-request.json>\n  workflow schema <backup-index|backup-sources|backup-restore-request>\n\nSources name the run database and optional artifact/definition stores. Paths resolve from the current directory.\nBackup contains a verified run snapshot and immutable artifact dependencies. It does not stop source workers.\nRestore keeps original effects/identity and fences old leases; running runs start paused.\nInspect run recovery before resuming. External activity since the backup requires actual provider reconciliation.\nUse effect-import only with the original provider-audited intent and actual receipt; never invent history.\nrecovery-acknowledge records the local operator's source-retirement/provider audit, not remote authentication.\nLinux atomic directory publication never overwrites existing paths. Incomplete staging is not a backup.\nLimits: 256 MiB per SQLite image, 1 GiB total, 10000 runs/artifact manifests, 8 MiB index.\n";
fn err(code: ErrorCode, e: impl std::fmt::Display) -> Error {
    Error::new(code, e.to_string())
}
fn read<T: DeserializeOwned>(path: &str) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(path)
        .map_err(|e| err(ErrorCode::Storage, e))?
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| err(ErrorCode::Storage, e))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Error::new(ErrorCode::Budget, "input exceeds 8 MiB"));
    }
    serde_json::from_slice(&bytes).map_err(|e| err(ErrorCode::InvalidArchive, e))
}
fn value(v: impl Serialize) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| err(ErrorCode::InvalidArchive, e))
}
pub(crate) fn summary(index: &BackupIndex) -> Value {
    json!({"verified": true, "digest": index.digest, "runs": index.manifest.runs.len(), "artifact_manifests": index.manifest.artifact_manifests, "files": index.manifest.files.len(), "bytes": index.manifest.files.values().map(|f| f.bytes).sum::<u64>(), "snapshot_started_at_unix_ms": index.manifest.created_at_unix_ms, "snapshot_completed_at_unix_ms": index.manifest.completed_at_unix_ms})
}
fn execute(args: &[&str]) -> Result<Value> {
    match args {
        ["schema", "backup-index"] => value(schemars::schema_for!(BackupIndex)),
        ["schema", "backup-sources"] => value(schemars::schema_for!(BackupSources)),
        ["schema", "backup-restore-request"] => value(schemars::schema_for!(RestoreRequest)),
        ["backup", "create", source, destination] => Ok(summary(&create(
            &read(source)?,
            destination,
            &workflow_worker::SystemClock,
        )?)),
        ["backup", "inspect", directory] => value(LocalBackup::open(directory)?.index()?),
        ["backup", "verify", directory] => Ok(summary(&LocalBackup::open(directory)?.index()?)),
        ["backup", "restore", directory, destination, request] => value(restore(
            &LocalBackup::open(directory)?,
            destination,
            &read(request)?,
            &workflow_worker::SystemClock,
        )?),
        _ => Err(Error::new(ErrorCode::InvalidArchive, "usage")),
    }
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let (result, code) = match execute(args) {
        Ok(result) => (json!({"ok": true, "result": result}), 0),
        Err(error) if error.message == "usage" => return write(stderr, HELP, 2),
        Err(error) => (json!({"ok": false, "error": error}), 1),
    };
    match serde_json::to_string(&result) {
        Ok(s) if s.len() <= MAX_MANIFEST_BYTES + 1024 => write(stdout, &s, code),
        _ => write(
            stdout,
            "{\"ok\":false,\"error\":\"backup output exceeds budget\",\"commit_may_have_succeeded\":true}",
            1,
        ),
    }
}

#[cfg(test)]
mod tests;
