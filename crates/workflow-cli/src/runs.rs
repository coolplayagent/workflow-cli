use crate::write;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::io::{Read, Write};
use workflow_runstore::*;
use workflow_runstore_sqlite::SqliteRunStore;

pub const HELP: &str = "DURABLE RUN STORAGE\n  workflow run init <db>\n  workflow run migrate <db>\n  workflow run start <db> <start.json>\n  workflow run drive <db> <run-id> <owner> <max-commands>\n  workflow run drive-models <db> <run-id> <owner> <max-commands> <bindings.json>\n  workflow run execution-history <db> <run-id> <after-sequence> <limit>\n  workflow run status <db> <run-id>\n  workflow run event <db> <event.json>\n  workflow run cancel <db> <run-id> <event-id> <expected-revision> <at-unix-ms>\n  workflow run list <db> <after-id|-> <limit>\n  workflow run history <db> <run-id> <after-revision> <limit>\n  workflow run outbox <db> <run-id> <after-sequence> <limit> <all|pending>\n  workflow run acknowledge <db> <receipt.json>\n  workflow run verify <db> <run-id>\n  workflow run acquire <db> <lease-request.json>\n  workflow run renew <db> <lease.json> <ttl-ms>\n  workflow run release <db> <lease.json>\n  workflow run claim <db> <lease.json>\n  workflow run tick-due <db> <lease.json>\n  workflow run retry-gate <db> <run-id> <instance-id> <event-id> <expected-revision>\n  workflow run finish <db> <lease.json> <attempt-id> <result.json>\n  workflow run attempt-failed <db> <lease.json> <attempt-id> <worker-error.json>\n  workflow run --artifacts <store> <operation> ...\n  workflow schema <run-start|run-receipt|run-lease|run-execution-record>\n\nOnly init creates a database. Mutations acknowledge after SQLite commit.\ndrive executes local read-only builtins with a durable run lease; max-commands is 1..100.\nTimers advance on drive; there is no background daemon. migrate explicitly upgrades v1/v2/v3/v4 storage to v5.\nRuns with artifact evidence require --artifacts on reads and mutations; this location is not persisted.\nclaim/drive checks frozen postconditions; UNKNOWN waits for explicit retry-gate.\nRaw gate events/manual gate receipts and raw successes in gated runs are refused.\nOther events and delivery receipts are trusted host facts; delivery is not task success.\nExit 0 means committed/read successfully; inspect result.snapshot.status (mutations) or result.status (status).\nExit 1 means rejected request/transition/storage/execution; 2 means usage/input I/O/output failure.\n";
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
fn open(db: &str, artifacts: Option<&str>) -> Result<SqliteRunStore> {
    let store = SqliteRunStore::open(db)?;
    Ok(if let Some(root) = artifacts {
        store.with_artifacts(Box::new(workflow_artifact_local::LocalArtifactStore::open(
            root,
        )?))
    } else {
        store
    })
}
fn execute(args: &[&str], artifacts: Option<&str>) -> Result<Value> {
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
            let reader = artifacts
                .map(workflow_artifact_local::LocalArtifactStore::open)
                .transpose()?;
            SqliteRunStore::migrate_with_artifacts(
                db,
                reader.map(|r| Box::new(r) as Box<dyn workflow_artifacts::ArtifactReader>),
            )?;
            Ok(json!({"migrated":true,"storage_version":5}))
        }
        ["run", "execution-history", db, id, after, limit] => {
            report(open(db, artifacts)?.execution_history(id, number(after)?, number(limit)?)?)
        }
        ["run", "acquire", db, file] => {
            report(open(db, artifacts)?.acquire(&read(file)?, &workflow_worker::SystemClock)?)
        }
        ["run", "renew", db, file, ttl] => report(open(db, artifacts)?.renew(
            &read(file)?,
            number(ttl)?,
            &workflow_worker::SystemClock,
        )?),
        ["run", "release", db, file] => {
            open(db, artifacts)?.release(&read(file)?, &workflow_worker::SystemClock)?;
            Ok(json!({"released":true}))
        }
        ["run", "claim", db, file] => {
            report(open(db, artifacts)?.claim_next(&read(file)?, &workflow_worker::SystemClock)?)
        }
        ["run", "retry-gate", db, id, instance, event_id, revision] => {
            let mut store = open(db, artifacts)?;
            let instance_id = number(instance)?;
            let expected_revision = number(revision)?;
            // A lost reply must not create another intent or change its timestamp.
            if let Some(record) = store.history(id, expected_revision, 1)?.items.first()
                && record.event.event_id == *event_id
            {
                if !matches!(record.event.kind, workflow_kernel::EventKind::RetryGate { instance_id: old, .. } if old == instance_id)
                {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "retry identity belongs to another event",
                    ));
                }
                return report(store.apply(&record.event)?);
            }
            let snapshot = store.get(id)?;
            let node = snapshot
                .frames
                .values()
                .flat_map(|f| f.nodes.values())
                .find(|n| n.instance_id == instance_id)
                .ok_or_else(|| Error::new(ErrorCode::InvalidRequest, "unknown gate instance"))?;
            let workflow_kernel::NodeState::CheckingGate {
                context,
                awaiting: false,
            } = &node.state
            else {
                return Err(Error::new(
                    ErrorCode::InvalidRequest,
                    "only an observed UNKNOWN gate can be retried",
                ));
            };
            use workflow_worker::Clock;
            let event = Event {
                event_id: (*event_id).into(),
                run_id: (*id).into(),
                run_digest: snapshot.run_digest.clone(),
                expected_revision,
                at_unix_ms: workflow_worker::SystemClock.now_unix_ms()?,
                kind: workflow_kernel::EventKind::RetryGate {
                    instance_id,
                    context_digest: workflow_worker::digest(context)?,
                },
            };
            report(store.apply(&event)?)
        }
        ["run", "tick-due", db, file] => {
            report(open(db, artifacts)?.tick_due(&read(file)?, &workflow_worker::SystemClock)?)
        }
        ["run", "finish", db, lease, attempt, result] => report(open(db, artifacts)?.finish_task(
            &read(lease)?,
            attempt,
            &read(result)?,
            &workflow_worker::SystemClock,
        )?),
        ["run", "attempt-failed", db, lease, attempt, error] => {
            open(db, artifacts)?.fail_task(
                &read(lease)?,
                attempt,
                &read(error)?,
                &workflow_worker::SystemClock,
            )?;
            Ok(json!({"recorded":true}))
        }
        ["run", "start", db, file] => {
            let request = read(file)?;
            report(open(db, artifacts)?.start(&request)?)
        }
        ["run", "status", db, id] => report(open(db, artifacts)?.get(id)?),
        ["run", "event", db, file] => {
            let event = read(file)?;
            report(open(db, artifacts)?.apply(&event)?)
        }
        ["run", "cancel", db, id, event_id, revision, at] => {
            let mut store = open(db, artifacts)?;
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
        ["run", "list", db, after, limit] => report(open(db, artifacts)?.list(
            if *after == "-" { None } else { Some(after) },
            number(limit)?,
        )?),
        ["run", "history", db, id, after, limit] => {
            report(open(db, artifacts)?.history(id, number(after)?, number(limit)?)?)
        }
        [
            "run",
            "outbox",
            db,
            id,
            after,
            limit,
            mode @ ("all" | "pending"),
        ] => report(open(db, artifacts)?.outbox(
            id,
            number(after)?,
            number(limit)?,
            *mode == "pending",
        )?),
        ["run", "acknowledge", db, file] => {
            let receipt = read(file)?;
            report(open(db, artifacts)?.acknowledge(&receipt)?)
        }
        ["run", "verify", db, id] => report(open(db, artifacts)?.verify(id)?),
        _ => Err(Error::new(ErrorCode::InvalidRequest, "usage")),
    }
}
fn drive(
    db: &str,
    id: &str,
    owner: &str,
    budget: &str,
    artifacts: Option<&str>,
    models: Option<&str>,
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
    let mut store = open(db, artifacts)?;
    let worker = if let Some(path) = models {
        crate::models::worker(&store.bundle(id)?, path)?
    } else {
        workflow_builtin_capabilities::worker()?
    };
    let result = workflow_runtime::drive(
        &mut store,
        &worker,
        id,
        &options,
        &workflow_worker::SystemClock,
    )?;
    Ok(report(result)?)
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let mut normalized = vec![];
    let (args, artifacts) = if let ["run", "--artifacts", root, tail @ ..] = args {
        normalized.push("run");
        normalized.extend_from_slice(tail);
        (normalized.as_slice(), Some(*root))
    } else {
        (args, None)
    };
    let outcome = if let ["run", "drive", db, id, owner, budget] = args {
        drive(db, id, owner, budget, artifacts, None).map_err(|e| (e.message.clone(), json!(e)))
    } else if let ["run", "drive-models", db, id, owner, budget, file] = args {
        drive(db, id, owner, budget, artifacts, Some(file))
            .map_err(|e| (e.message.clone(), json!(e)))
    } else {
        execute(args, artifacts).map_err(|e| (e.message.clone(), json!(e)))
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
