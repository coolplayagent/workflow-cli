use crate::write;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::io::{Read, Write};
use workflow_runstore::*;
use workflow_runstore_sqlite::SqliteRunStore;

pub const HELP: &str = "DURABLE RUN STORAGE\n  workflow run init <db>\n  workflow run migrate <db>\n  workflow run start <db> <start.json>\n  workflow run drive <db> <run-id> <owner> <max-commands>\n  workflow run execution-history <db> <run-id> <after-sequence> <limit>\n  workflow run status <db> <run-id>\n  workflow run event <db> <event.json>\n  workflow run cancel <db> <run-id> <event-id> <expected-revision> <at-unix-ms>\n  workflow run list <db> <after-id|-> <limit>\n  workflow run history <db> <run-id> <after-revision> <limit>\n  workflow run outbox <db> <run-id> <after-sequence> <limit> <all|pending>\n  workflow run acknowledge <db> <receipt.json>\n  workflow run verify <db> <run-id>\n  workflow schema <run-start|run-receipt|run-lease|run-execution-record>\n\nOnly init creates a database. Mutations acknowledge after SQLite commit.\ndrive executes local read-only builtins with a durable run lease; max-commands is 1..100.\nTimers advance on drive; there is no background daemon. migrate explicitly upgrades v1 storage.\nEvents and delivery receipts are trusted host facts; delivery is not task success.\nExit 0 means committed/read successfully; inspect result.snapshot.status (mutations) or result.status (status).\nExit 1 means rejected request/transition/storage/execution; 2 means usage/input I/O/output failure.\n";
fn read<T: DeserializeOwned>(p: &str) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(p)
        .and_then(|f| {
            f.take(workflow_worker::MAX_MESSAGE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| Error::new(ErrorCode::InvalidRequest, format!("I/O {p}: {e}")))?;
    Ok(workflow_worker::parse_message(&bytes)?)
}
fn report(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| Error::new(ErrorCode::InvalidRequest, e.to_string()))
}
fn number<T: std::str::FromStr>(s: &str) -> Result<T> {
    s.parse()
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "unsigned integer required"))
}
fn execute(args: &[&str]) -> Result<Value> {
    match args {
        [
            "schema",
            kind @ ("run-start" | "run-receipt" | "run-lease" | "run-execution-record"),
        ] => Ok(workflow_worker::parse_message(
            schema(&kind[4..])?.as_bytes(),
        )?),
        ["run", "init", db] => {
            SqliteRunStore::create(db)?;
            Ok(json!({"initialized":true}))
        }
        ["run", "migrate", db] => {
            SqliteRunStore::migrate(db)?;
            Ok(json!({"migrated":true,"storage_version":2}))
        }
        ["run", "execution-history", db, id, after, limit] => report(
            SqliteRunStore::open(db)?.execution_history(id, number(after)?, number(limit)?)?,
        ),
        ["run", "start", db, file] => {
            let request = read(file)?;
            report(SqliteRunStore::open(db)?.start(&request)?)
        }
        ["run", "status", db, id] => report(SqliteRunStore::open(db)?.get(id)?),
        ["run", "event", db, file] => {
            let event = read(file)?;
            report(SqliteRunStore::open(db)?.apply(&event)?)
        }
        ["run", "cancel", db, id, event_id, revision, at] => {
            let mut store = SqliteRunStore::open(db)?;
            let snapshot = store.get(id)?;
            let event = Event {
                event_id: (*event_id).into(),
                run_id: (*id).into(),
                run_digest: snapshot.run_digest,
                expected_revision: number(revision)?,
                at_unix_ms: number(at)?,
                kind: workflow_kernel::EventKind::Cancel,
            };
            report(store.apply(&event)?)
        }
        ["run", "list", db, after, limit] => report(SqliteRunStore::open(db)?.list(
            if *after == "-" { None } else { Some(after) },
            number(limit)?,
        )?),
        ["run", "history", db, id, after, limit] => {
            report(SqliteRunStore::open(db)?.history(id, number(after)?, number(limit)?)?)
        }
        [
            "run",
            "outbox",
            db,
            id,
            after,
            limit,
            mode @ ("all" | "pending"),
        ] => report(SqliteRunStore::open(db)?.outbox(
            id,
            number(after)?,
            number(limit)?,
            *mode == "pending",
        )?),
        ["run", "acknowledge", db, file] => {
            let receipt = read(file)?;
            report(SqliteRunStore::open(db)?.acknowledge(&receipt)?)
        }
        ["run", "verify", db, id] => report(SqliteRunStore::open(db)?.verify(id)?),
        _ => Err(Error::new(ErrorCode::InvalidRequest, "usage")),
    }
}
fn drive(
    db: &str,
    id: &str,
    owner: &str,
    budget: &str,
) -> std::result::Result<Value, workflow_runtime::Error> {
    let max_commands = number(budget)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| Error::new(ErrorCode::InvalidRequest, e.to_string()))?
        .as_nanos();
    let options = workflow_runtime::DriveOptions {
        owner: owner.into(),
        acquisition_id: format!("cli-{}-{nonce}", std::process::id()),
        lease_ms: 120_000,
        max_commands,
    };
    let result = workflow_runtime::drive(
        &mut SqliteRunStore::open(db)?,
        &workflow_builtin_capabilities::worker()?,
        id,
        &options,
        &workflow_worker::SystemClock,
    )?;
    Ok(report(result)?)
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let outcome = if let ["run", "drive", db, id, owner, budget] = args {
        drive(db, id, owner, budget).map_err(|e| (e.message.clone(), json!(e)))
    } else {
        execute(args).map_err(|e| (e.message.clone(), json!(e)))
    };
    match outcome {
        Ok(value) => match workflow_worker::to_message(&json!({"ok":true,"result":value})) {
            Ok(bytes) => write(stdout, std::str::from_utf8(&bytes).expect("JSON UTF-8"), 0),
            Err(e) => write(
                stdout,
                &json!({"ok":false,"error":e,"commit_may_have_succeeded":true}).to_string(),
                1,
            ),
        },
        Err((message, _)) if message == "usage" => write(stderr, HELP, 2),
        Err((message, e)) => {
            let code = if message.starts_with("I/O ") { 2 } else { 1 };
            write(stdout, &json!({"ok":false,"error":e}).to_string(), code)
        }
    }
}
#[cfg(test)]
mod tests;
