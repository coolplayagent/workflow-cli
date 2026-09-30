use crate::recovery::Recovered;
use crate::*;
use rusqlite::{OptionalExtension, params};
mod controls;
pub(crate) mod effects;
pub(crate) mod gates;
mod leases;
mod proof;
mod tasks;
use workflow_worker::Clock;
pub(crate) const SCHEMA:&str="
CREATE TABLE execution_heads (
 run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
 revision INTEGER NOT NULL CHECK(revision>=0), chain_digest TEXT NOT NULL
);
CREATE TABLE execution_events (
 run_id TEXT NOT NULL REFERENCES runs(run_id),
 sequence INTEGER NOT NULL CHECK(sequence>0), document TEXT NOT NULL, digest TEXT NOT NULL,
 PRIMARY KEY(run_id,sequence)
);
CREATE TRIGGER immutable_execution_update BEFORE UPDATE ON execution_events BEGIN SELECT RAISE(ABORT,'immutable execution event'); END;
CREATE TRIGGER immutable_execution_delete BEFORE DELETE ON execution_events BEGIN SELECT RAISE(ABORT,'immutable execution event'); END;
CREATE TRIGGER immutable_execution_head_delete BEFORE DELETE ON execution_heads BEGIN SELECT RAISE(ABORT,'execution deletion unsupported'); END;
CREATE TRIGGER monotonic_execution_head BEFORE UPDATE ON execution_heads WHEN NEW.run_id!=OLD.run_id OR NEW.revision!=OLD.revision+1 BEGIN SELECT RAISE(ABORT,'execution revision must advance by one'); END;
";
pub(crate) fn init_head(c: &Connection, id: &str) -> Result<()> {
    c.execute(
        "INSERT INTO execution_heads(run_id,revision,chain_digest) VALUES(?1,0,?2)",
        params![id, digest(&"empty execution journal")?],
    )
    .map_err(storage)?;
    Ok(())
}
pub(crate) fn read(
    c: &Connection,
    r: &Recovered,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<(Authority, Vec<ExecutionRecord>)> {
    let s = r.engine.snapshot();
    let id = &s.run_id;
    let (count, expected): (i64, String) = c
        .query_row(
            "SELECT revision,chain_digest FROM execution_heads WHERE run_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage)?
        .ok_or_else(|| corrupt("missing execution head"))?;
    if count < 0 || count as u64 > MAX_EXECUTION_RECORDS {
        return Err(corrupt("execution journal count out of bounds"));
    }
    let mut authority = Authority::new(id, r.engine.checkpoint()?.started_at_unix_ms);
    let mut chain = digest(&"empty execution journal")?;
    let mut records = vec![];
    let mut q=c.prepare("SELECT sequence,document,digest FROM execution_events WHERE run_id=?1 ORDER BY sequence").map_err(storage)?;
    let mut rows = q.query([id]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        let sequence: i64 = row.get(0).map_err(storage)?;
        let document: String = row.get(1).map_err(storage)?;
        let hash: String = row.get(2).map_err(storage)?;
        let record: ExecutionRecord = decode(&document, &hash)?;
        if sequence != number(authority.sequence + 1)? || record.sequence != authority.sequence + 1
        {
            return Err(corrupt("execution journal sequence gap"));
        }
        authority
            .apply(&record.action)
            .map_err(|e| corrupt(e.message))?;
        proof::verify(r, &authority, &record.action, artifacts).map_err(|e| {
            if matches!(
                e.code,
                ErrorCode::ArtifactUnavailable | ErrorCode::ArtifactRejected
            ) {
                e
            } else {
                corrupt(e.message)
            }
        })?;
        chain = digest(&(chain, hash))?;
        records.push(record);
    }
    // Every protected gate event needs exactly one fenced execution proof.
    let gate_events: std::collections::BTreeSet<_> = r
        .events
        .iter()
        .filter(|e| {
            matches!(
                e.event.kind,
                workflow_kernel::EventKind::GateEvaluated { .. }
            )
        })
        .map(|e| e.event.event_id.as_str())
        .collect();
    let gate_proofs: Vec<_> = records
        .iter()
        .filter_map(|r| match &r.action {
            ExecutionAction::GateChecked { event_id, .. } => Some(event_id.as_str()),
            _ => None,
        })
        .collect();
    if gate_proofs.len() != gate_events.len()
        || gate_proofs
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            != gate_events
    {
        return Err(corrupt("gate events and fenced execution proofs differ"));
    }
    effects::verify_coverage(r, &records)?;
    let migration_events: std::collections::BTreeSet<_> = r
        .events
        .iter()
        .filter(|e| {
            matches!(
                e.event.kind,
                workflow_kernel::EventKind::MigrateDefinition { .. }
            )
        })
        .map(|e| e.event.event_id.as_str())
        .collect();
    let migrations: Vec<_> = records
        .iter()
        .filter_map(|r| match &r.action {
            ExecutionAction::Migrated { migration } => Some(migration.event_id.as_str()),
            _ => None,
        })
        .collect();
    if migrations.len() != migration_events.len()
        || migrations
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            != migration_events
    {
        return Err(corrupt(
            "definition migration events and fenced authority proofs differ",
        ));
    }
    if count != number(authority.sequence)? || chain != expected {
        return Err(corrupt("execution journal disagrees with its head"));
    }
    Ok((authority, records))
}
pub(crate) fn append(c: &Connection, a: &mut Authority, action: ExecutionAction) -> Result<()> {
    let before = a.sequence;
    a.apply(&action)?;
    let record = ExecutionRecord {
        sequence: a.sequence,
        action,
    };
    c.execute(
        "INSERT INTO execution_events(run_id,sequence,document,digest) VALUES(?1,?2,?3,?4)",
        params![
            a.run_id,
            number(record.sequence)?,
            document(&record)?,
            digest(&record)?
        ],
    )
    .map_err(storage)?;
    let previous: String = c
        .query_row(
            "SELECT chain_digest FROM execution_heads WHERE run_id=?1",
            [&a.run_id],
            |row| row.get(0),
        )
        .map_err(storage)?;
    let changed=c.execute("UPDATE execution_heads SET revision=?2,chain_digest=?3 WHERE run_id=?1 AND revision=?4",params![a.run_id,number(record.sequence)?,digest(&(previous,digest(&record)?))?,number(before)?]).map_err(storage)?;
    if changed != 1 {
        return Err(corrupt("execution head CAS failed"));
    }
    Ok(())
}
fn live(a: &Authority, r: &Recovered, l: &Lease, now: u64) -> Result<()> {
    if now < r.engine.snapshot().now_unix_ms {
        return Err(Error::new(
            ErrorCode::LeaseConflict,
            "host time precedes committed run time",
        ));
    }
    a.check_live(l, now)
}
fn receipt(l: &Lease, e: &OutboxEntry) -> DeliveryReceipt {
    DeliveryReceipt {
        run_id: l.run_id.clone(),
        command_id: e.command_id.clone(),
        command_digest: e.command_digest.clone(),
        delivery_id: format!("runtime-{}-{}", l.epoch, e.sequence),
    }
}

fn commit_guard(
    admission: &std::cell::Cell<Option<AdmissionWindow>>,
    clock: &dyn Clock,
    lease: &Lease,
    admitted: u64,
    deadline: u64,
) -> Result<u64> {
    let now = clock.now_unix_ms()?;
    if now < admitted || now >= lease.expires_at_unix_ms || now >= deadline {
        return Err(Error::new(
            ErrorCode::LeaseConflict,
            "clock reversed or lease/request expired before commit admission",
        ));
    }
    image::record_admission(admission, now, deadline.min(lease.expires_at_unix_ms));
    Ok(now)
}
