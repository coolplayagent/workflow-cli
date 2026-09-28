use crate::recovery::{Recovered, recover};
use crate::*;
use rusqlite::params;
impl SqliteRunStore {
    pub(crate) fn read<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(Recovered) -> Result<T>,
    ) -> Result<T> {
        let tx = self.connection.transaction().map_err(storage)?;
        let result = f(recover(&tx, id, self.artifacts.as_deref())?)?;
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    pub(crate) fn list_runs(
        &mut self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Page<RunSummary, String>> {
        validate_limit(limit)?;
        if let Some(id) = after {
            validate_id(id)?;
        }
        let tx = self.connection.transaction().map_err(storage)?;
        check_version(&tx)?;
        let ids = {
            let mut q=tx.prepare("SELECT run_id FROM runs WHERE (?1 IS NULL OR run_id>?1) ORDER BY run_id LIMIT ?2").map_err(storage)?;
            q.query_map(params![after, i64::from(limit) + 1], |r| {
                r.get::<_, String>(0)
            })
            .map_err(storage)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage)?
        };
        let next_cursor = if ids.len() > limit as usize {
            ids.get(limit as usize - 1).cloned()
        } else {
            None
        };
        let mut items = vec![];
        for id in ids.into_iter().take(limit as usize) {
            let r = recover(&tx, &id, self.artifacts.as_deref())?;
            let s = r.engine.snapshot();
            items.push(RunSummary {
                run_id: s.run_id.clone(),
                revision: s.revision,
                status: s.status.clone(),
                pause: s.pause.clone(),
                bundle_digest: s.bundle_digest.clone(),
            });
        }
        tx.commit().map_err(storage)?;
        Ok(Page { items, next_cursor })
    }
    pub(crate) fn read_history(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Page<RecordedEvent, u64>> {
        validate_limit(limit)?;
        number(after)?;
        self.read(id, |r| {
            let mut items: Vec<_> = r
                .events
                .into_iter()
                .filter(|e| e.revision > after)
                .take(limit as usize + 1)
                .collect();
            let next_cursor = if items.len() > limit as usize {
                items.pop();
                items.last().map(|e| e.revision)
            } else {
                None
            };
            Ok(Page { items, next_cursor })
        })
    }
    pub(crate) fn read_outbox(
        &mut self,
        id: &str,
        after: u64,
        limit: u32,
        pending: bool,
    ) -> Result<Page<OutboxEntry, u64>> {
        validate_limit(limit)?;
        number(after)?;
        self.read(id, |r| {
            let mut items: Vec<_> = r
                .outbox
                .into_iter()
                .filter(|e| e.sequence > after && (!pending || e.receipt.is_none()))
                .take(limit as usize + 1)
                .collect();
            let next_cursor = if items.len() > limit as usize {
                items.pop();
                items.last().map(|e| e.sequence)
            } else {
                None
            };
            Ok(Page { items, next_cursor })
        })
    }
}
