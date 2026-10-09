//! Storage upgrades preserve recorded execution; they never migrate definitions.
use crate::*;
use rusqlite::{
    backup::{Backup, StepResult},
    types::ValueRef,
};
use workflow_artifacts::ArtifactReader;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS storage_migrations (
 sequence INTEGER PRIMARY KEY CHECK(sequence>0), document TEXT NOT NULL, digest TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS immutable_storage_migrations_update BEFORE UPDATE ON storage_migrations BEGIN SELECT RAISE(ABORT,'immutable storage migration'); END;
CREATE TRIGGER IF NOT EXISTS immutable_storage_migrations_delete BEFORE DELETE ON storage_migrations BEGIN SELECT RAISE(ABORT,'immutable storage migration'); END;";

pub use workflow_runstore::StorageUpgrade;

fn source_version(c: &Connection) -> Result<i64> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(storage)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if app != APPLICATION_ID || !(1..=STORAGE_VERSION).contains(&version) {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "only run store schemas 1 through 12 can migrate",
        ));
    }
    Ok(version)
}

// Canonical contents, not database page layout. The journal, ownership records,
// bindings and schema SQL all participate. Bounded backup/preflight rejects huge
// databases before acquiring their contents.
pub(crate) fn logical_digest(c: &Connection) -> Result<String> {
    let mut chain = digest(&source_version(c)?)?;
    let mut schema = c.prepare("SELECT type,name,sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY type,name").map_err(storage)?;
    let rows = schema
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(storage)?;
    for row in rows {
        chain = digest(&(chain, row.map_err(storage)?))?;
    }
    let mut q = c.prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").map_err(storage)?;
    let tables = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(storage)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(storage)?;
    for (table, sql) in tables {
        chain = digest(&(chain, &table, sql))?;
        let quoted = table.replace('"', "\"\"");
        let mut q = c
            .prepare(&format!("SELECT * FROM \"{quoted}\" ORDER BY rowid"))
            .map_err(storage)?;
        let count = q.column_count();
        let mut rows = q.query([]).map_err(storage)?;
        while let Some(row) = rows.next().map_err(storage)? {
            for column in 0..count {
                let value = match row.get_ref(column).map_err(storage)? {
                    ValueRef::Integer(n) => digest(&("integer", n))?,
                    ValueRef::Text(v) => digest(&("text", workflow_artifacts::content_digest(v)))?,
                    _ => return Err(corrupt("unsupported storage cell")),
                };
                chain = digest(&(chain, column, value))?;
            }
        }
    }
    Ok(chain)
}

fn ids(c: &Connection) -> Result<Vec<String>> {
    let ids = c
        .prepare("SELECT run_id FROM runs ORDER BY run_id LIMIT 10001")
        .map_err(storage)?
        .query_map([], |r| r.get(0))
        .map_err(storage)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(storage)?;
    if ids.len() > 10000 {
        return Err(Error::new(
            ErrorCode::Storage,
            "storage upgrade supports at most 10000 runs",
        ));
    }
    Ok(ids)
}

pub(crate) fn record(
    c: &Connection,
    source_version: i64,
    source_digest: String,
    artifacts: Option<&dyn ArtifactReader>,
) -> Result<StorageUpgrade> {
    let mut verified = vec![];
    for id in ids(c)? {
        let r = crate::recovery::recover(c, &id, artifacts)?;
        let (_, records) = crate::execution::read(c, &r, artifacts)?;
        if source_version < 11
            && (r.events.iter().any(|e| {
                matches!(
                    e.event.kind,
                    workflow_kernel::EventKind::MigrateDefinition { .. }
                )
            }) || records
                .iter()
                .any(|r| matches!(r.action, ExecutionAction::Migrated { .. })))
        {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "previous storage schema contains unsupported definition migration events",
            ));
        }
        let mut execution = digest(&"execution history")?;
        for record in records {
            execution = digest(&(execution, digest(&record)?))?;
        }
        verified.push((id, digest(r.engine.snapshot())?, execution));
    }
    let report = StorageUpgrade {
        source_version,
        target_version: STORAGE_VERSION,
        source_digest,
        verified_runs: verified.len() as u64,
        verified_history_digest: digest(&verified)?,
    };
    c.execute("INSERT INTO storage_migrations(sequence,document,digest) SELECT COALESCE(MAX(sequence),0)+1,?1,?2 FROM storage_migrations", rusqlite::params![document(&report)?, digest(&report)?]).map_err(storage)?;
    Ok(report)
}

pub(crate) fn upgrade_connection(
    c: &mut Connection,
    artifacts: Option<&dyn ArtifactReader>,
    expected: Option<&StorageUpgrade>,
    hook: impl Fn(&str),
) -> Result<Option<StorageUpgrade>> {
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let version = source_version(&tx)?;
    let source = logical_digest(&tx)?;
    if expected.is_some_and(|p| {
        p.source_version != version
            || p.source_digest != source
            || p.target_version != STORAGE_VERSION
    }) {
        return Err(Error::new(
            ErrorCode::TransitionRejected,
            "storage changed after preflight; retain backup and plan again",
        ));
    }
    if version == STORAGE_VERSION {
        for id in ids(&tx)? {
            let r = crate::recovery::recover(&tx, &id, artifacts)?;
            crate::execution::read(&tx, &r, artifacts)?;
        }
        tx.commit().map_err(storage)?;
        return Ok(None);
    }
    if version == 1 {
        tx.execute_batch(crate::execution::SCHEMA)
            .map_err(storage)?;
        for id in ids(&tx)? {
            crate::execution::init_head(&tx, &id)?;
        }
    }
    tx.execute_batch(SCHEMA).map_err(storage)?;
    tx.execute_batch(STATE_SCHEMA).map_err(storage)?;
    hook("schema_written");
    tx.pragma_update(None, "user_version", STORAGE_VERSION)
        .map_err(storage)?;
    let report = record(&tx, version, source, artifacts)?;
    for id in ids(&tx)? {
        let r = crate::recovery::audit(&tx, &id, artifacts)?;
        if tx
            .query_row(
                "SELECT count(*) FROM state_checkpoints WHERE run_id=?1 AND revision=?2",
                rusqlite::params![id, number(r.engine.snapshot().revision)?],
                |r| r.get::<_, i64>(0),
            )
            .map_err(storage)?
            == 0
        {
            crate::recovery::write_state_checkpoint(&tx, &r.engine)?;
        }
    }
    if expected.is_some_and(|p| p != &report) {
        return Err(corrupt("storage conversion differs from verified plan"));
    }
    hook("verified");
    hook("before_commit");
    tx.commit().map_err(storage)?;
    hook("after_commit");
    Ok(Some(report))
}

fn copy_snapshot(source: &Connection, destination: &mut Connection) -> Result<()> {
    let pages: i64 = source
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .map_err(storage)?;
    let size: i64 = source
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(storage)?;
    if pages
        .checked_mul(size)
        .is_none_or(|n| n <= 0 || n > 256 * 1024 * 1024)
    {
        return Err(Error::new(
            ErrorCode::Storage,
            "storage upgrade snapshot exceeds 256 MiB",
        ));
    }
    if !matches!(
        Backup::new(source, destination)
            .map_err(storage)?
            .step(-1)
            .map_err(storage)?,
        StepResult::Done
    ) {
        return Err(Error::new(
            ErrorCode::Busy,
            "consistent storage snapshot was not completed",
        ));
    }
    Ok(())
}

impl SqliteRunStore {
    pub fn storage_history(&mut self) -> Result<Vec<StorageUpgrade>> {
        let mut result = vec![];
        let mut q = self
            .connection
            .prepare("SELECT sequence,document,digest FROM storage_migrations ORDER BY sequence")
            .map_err(storage)?;
        let mut rows = q.query([]).map_err(storage)?;
        while let Some(row) = rows.next().map_err(storage)? {
            let sequence: i64 = row.get(0).map_err(storage)?;
            let value: String = row.get(1).map_err(storage)?;
            let hash: String = row.get(2).map_err(storage)?;
            let report: StorageUpgrade = decode(&value, &hash)?;
            if sequence != result.len() as i64 + 1
                || !(1..STORAGE_VERSION).contains(&report.source_version)
                || !(report.source_version + 1..=STORAGE_VERSION).contains(&report.target_version)
            {
                return Err(corrupt("invalid storage migration history"));
            }
            result.push(report);
        }
        Ok(result)
    }
    /// Verify an in-memory conversion of a consistent read snapshot. No source
    /// pages, execution events or definitions are changed by planning.
    pub fn plan_storage_upgrade(
        path: impl AsRef<Path>,
        artifacts: Option<&dyn ArtifactReader>,
    ) -> Result<Option<StorageUpgrade>> {
        let mut source = connect(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let tx = source.transaction().map_err(storage)?;
        source_version(&tx)?;
        let mut copy = Connection::open_in_memory().map_err(storage)?;
        copy_snapshot(&tx, &mut copy)?;
        tx.commit().map_err(storage)?;
        upgrade_connection(&mut copy, artifacts, None, |_| {})
    }

    /// Create an exclusive consistent backup, validate its complete conversion,
    /// then apply that exact plan with a source-content CAS. A failed conversion
    /// leaves the source version intact and the backup available to old binaries.
    pub fn upgrade_with_backup(
        path: impl AsRef<Path>,
        backup: impl AsRef<Path>,
        artifacts: Option<Box<dyn ArtifactReader>>,
    ) -> Result<(Self, StorageUpgrade)> {
        let mut source = connect(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let tx = source.transaction().map_err(storage)?;
        source_version(&tx)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(backup.as_ref())
            .map_err(|_| Error::new(ErrorCode::Storage, "backup must be a new writable file"))?;
        let mut destination = connect(backup.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        copy_snapshot(&tx, &mut destination)?;
        tx.commit().map_err(storage)?;
        drop(destination);
        file.sync_all()
            .map_err(|_| Error::new(ErrorCode::Storage, "backup sync failed"))?;
        {
            let parent = backup
                .as_ref()
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|_| Error::new(ErrorCode::Storage, "backup directory sync failed"))?;
        }
        let plan = Self::plan_storage_upgrade(&backup, artifacts.as_deref())?
            .ok_or_else(|| Error::new(ErrorCode::InvalidRequest, "storage is already current"))?;
        let report = upgrade_connection(&mut source, artifacts.as_deref(), Some(&plan), |_| {})?
            .ok_or_else(|| corrupt("missing storage conversion"))?;
        Ok((
            Self {
                connection: source,
                artifacts,
                admission: Default::default(),
            },
            report,
        ))
    }

    /// Verify and restore a retained pre-upgrade database into a new path. It
    /// stays at its original schema for the retained compatible binary. This
    /// restores storage only; it does not undo external business effects.
    pub fn restore_storage_backup(
        backup: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        expected: &StorageUpgrade,
        artifacts: Option<&dyn ArtifactReader>,
    ) -> Result<()> {
        if Self::plan_storage_upgrade(&backup, artifacts)?.as_ref() != Some(expected) {
            return Err(corrupt("backup differs from reviewed storage upgrade"));
        }
        let mut source = connect(backup.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let tx = source.transaction().map_err(storage)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(destination.as_ref())
            .map_err(|_| Error::new(ErrorCode::Storage, "restore destination must not exist"))?;
        let mut copy = connect(destination.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        copy_snapshot(&tx, &mut copy)?;
        tx.commit().map_err(storage)?;
        drop(copy);
        if Self::plan_storage_upgrade(&destination, artifacts)?.as_ref() != Some(expected) {
            return Err(corrupt("restored database verification failed"));
        }
        file.sync_all()
            .map_err(|_| Error::new(ErrorCode::Storage, "restored database sync failed"))?;
        let parent = destination
            .as_ref()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| Error::new(ErrorCode::Storage, "restore directory sync failed"))?;
        Ok(())
    }
}
