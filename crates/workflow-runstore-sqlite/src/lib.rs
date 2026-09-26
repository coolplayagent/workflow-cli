//! Transactional SQLite RunStore. No task dispatch, clock reads or model calls.
mod bindings;
mod db;
mod reads;
mod recovery;
mod writes;
use db::*;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::Path;
use workflow_runstore::*;

pub struct SqliteRunStore {
    connection: Connection,
}
impl SqliteRunStore {
    /// Initialize an empty database, or reopen this exact supported application schema.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let mut connection = connect(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(storage)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage)?;
        if app == 0 && version == 0 {
            let count: i64 = tx
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
                .map_err(storage)?;
            if count != 0 {
                return Err(Error::new(
                    ErrorCode::UnsupportedStorage,
                    "refusing a nonempty foreign database",
                ));
            }
            tx.execute_batch(SCHEMA).map_err(storage)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(storage)?;
            tx.pragma_update(None, "user_version", STORAGE_VERSION)
                .map_err(storage)?;
        } else {
            check_version(&tx)?;
        }
        tx.commit().map_err(storage)?;
        Ok(Self { connection })
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = connect(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        check_version(&connection)?;
        Ok(Self { connection })
    }
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let connection = connect(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        check_version(&connection)?;
        Ok(Self { connection })
    }
}
impl RunStore for SqliteRunStore {
    fn start(&mut self, r: &StartRun) -> Result<Committed> {
        self.start_internal(r, |_| {})
    }
    fn apply(&mut self, e: &Event) -> Result<Committed> {
        self.apply_internal(e, |_| {})
    }
    fn get(&mut self, id: &str) -> Result<Snapshot> {
        self.read(id, |r| Ok(r.engine.snapshot().clone()))
    }
    fn list(&mut self, after: Option<&str>, limit: u32) -> Result<Page<RunSummary, String>> {
        self.list_runs(after, limit)
    }
    fn history(&mut self, id: &str, after: u64, limit: u32) -> Result<Page<RecordedEvent, u64>> {
        self.read_history(id, after, limit)
    }
    fn outbox(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
        pending: bool,
    ) -> Result<Page<OutboxEntry, u64>> {
        self.read_outbox(id, after, limit, pending)
    }
    fn acknowledge(&mut self, r: &DeliveryReceipt) -> Result<OutboxEntry> {
        self.acknowledge_delivery(r)
    }
    fn verify(&mut self, id: &str) -> Result<Verification> {
        self.read(id, |r| {
            Ok(Verification {
                run_id: id.into(),
                revision: r.engine.snapshot().revision,
                checkpoint_revision: r.checkpoint_revision,
                events_checked: r.events.len() as u64,
                commands_checked: r.outbox.len() as u64,
                state_digest: digest(r.engine.snapshot())?,
            })
        })
    }
}
#[cfg(test)]
mod tests;
