use crate::write;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::io::{Read, Write};
use workflow_runstore::*;
use workflow_runstore_sqlite::SqliteRunStore;

pub const HELP: &str = "DURABLE RUN STORAGE\n  workflow run init <db>\n  workflow run storage-plan <db>\n  workflow run migrate <db> <new-backup-file>\n  workflow run storage-history <db>\n  workflow run storage-restore <backup-file> <new-db> <storage-plan.json>\n  workflow run migration-plan <db> <run-id> <request.json>\n  workflow run migration-apply <db> <lease.json> <reviewed-plan.json> <actor>\n  workflow run history-at <db> <run-id> <revision>\n  workflow run export <db> <new-directory>\n  workflow run start <db> <start.json>\n  workflow run continue <db> <lease.json> <continuation-plan.json>\n  workflow run continuation <db> <run-id>\n  workflow run history-usage <db> <run-id>\n  workflow run drive <db> <run-id> <owner> <max-commands>\n  workflow run drive-workspaces <db> <run-id> <owner> <max-commands> <workspace-binding.json>\n  workflow run drive-models <db> <run-id> <owner> <max-commands> <bindings.json>\n  workflow run drive-effects <db> <run-id> <owner> <max-commands> <bindings.json> [model-bindings.json]\n  workflow run recovery <db> <run-id>\n  workflow run recovery-acknowledge <db> <run-id> <audit.json>\n  workflow run effect-import <db> <lease.json> <import.json>\n  workflow run effects <db> <run-id> <after-instance> <limit>\n  workflow run effect-claim <db> <lease.json>\n  workflow run effect-observe <db> <lease.json> <attempt-id> <observation.json>\n  workflow run effect-resolve <db> <lease.json> <operation-key> <resolution.json>\n  workflow run execution-history <db> <run-id> <after-sequence> <limit>\n  workflow run status <db> <run-id>\n  workflow run receive <db> <signal.json>\n  workflow run inbox <db> <run-id> <after-revision> <limit>\n  workflow run waits <db> <run-id> <after-instance> <limit>\n  workflow run event <db> <event.json>\n  workflow run cancel <db> <run-id> <event-id> <expected-revision> <at-unix-ms>\n  workflow run <pause|resume> <db> <run-id> <event-id> <expected-revision> <at-unix-ms> <reason>\n  workflow run list <db> <after-id|-> <limit>\n  workflow run history <db> <run-id> <after-revision> <limit>\n  workflow run outbox <db> <run-id> <after-sequence> <limit> <all|pending>\n  workflow run acknowledge <db> <receipt.json>\n  workflow run verify <db> <run-id>\n  workflow run acceptance <db> <run-id>\n  workflow run acquire <db> <lease-request.json>\n  workflow run renew <db> <lease.json> <ttl-ms>\n  workflow run release <db> <lease.json>\n  workflow run claim <db> <lease.json>\n  workflow run tick-due <db> <lease.json>\n  workflow run retry-gate <db> <run-id> <instance-id> <event-id> <expected-revision>\n  workflow run finish <db> <lease.json> <attempt-id> <result.json>\n  workflow run attempt-failed <db> <lease.json> <attempt-id> <worker-error.json>\n  workflow run --artifacts <store> <operation> ...\n  workflow run --object-artifacts <binding.json> <operation> ...\n  workflow run --revalidated-artifacts <store> <plan-artifact-id> <operation> ...\n  workflow run --revalidated-object-artifacts <binding.json> <plan-artifact-id> <operation> ...\n  workflow schema <run-start|run-handoff|run-continuation|run-receipt|run-lease|run-execution-record|run-signal|run-migration-request|run-migration-plan|run-storage-upgrade>\n\nOnly init creates a database. Mutations acknowledge after SQLite commit.\ndrive executes local read-only builtins with a durable run lease; max-commands is 1..100.\nPause persists admission state; in-flight results may commit; resume preserves original deadlines.\nTimers advance on drive or optional daemon serve; use daemon status to query live scheduling. migrate explicitly upgrades v1..v11 storage to v12 after a verified consistent backup and source CAS.\nRuns with artifact evidence require --artifacts on reads and mutations; this location is not persisted.\nclaim/drive checks frozen postconditions; UNKNOWN waits for explicit retry-gate.\nRaw gate events/manual gate receipts and raw successes in gated runs are refused.\nInbox ingress is trusted host input; inspect entry.status: committed receipt does not mean applied approval.\nOther events and delivery receipts are trusted host facts; delivery is not task success.\nExit 0 means committed/read successfully; inspect result.snapshot.status (mutations) or result.status (status).\nExit 1 means rejected request/transition/storage/execution; 2 means usage/input I/O/output failure.\n";
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
#[derive(Clone, Copy)]
pub(crate) enum ArtifactLocation<'a> {
    Local(&'a str),
    S3(&'a str),
    InvalidatedLocal(&'a str, &'a str),
    InvalidatedS3(&'a str, &'a str),
}
fn invalidated(
    store: impl workflow_artifacts::ArtifactStore + 'static,
    id: &str,
) -> Result<Box<dyn workflow_artifacts::ArtifactReader>> {
    use workflow_artifacts::*;
    let link = workflow_artifact_local::link_for_id(id)?;
    verify_expected(&store, &link, &revalidation_type())?;
    let plan: RevalidationPlan = parse_message(&store.read(&link)?)?;
    Ok(Box::new(InvalidatedReader::new(Box::new(store), &plan)?))
}
fn reader(location: ArtifactLocation<'_>) -> Result<Box<dyn workflow_artifacts::ArtifactReader>> {
    Ok(match location {
        ArtifactLocation::Local(root) => {
            Box::new(workflow_artifact_local::LocalArtifactStore::open(root)?)
        }
        ArtifactLocation::S3(binding) => Box::new(crate::artifact_objects::open(binding, false)?),
        ArtifactLocation::InvalidatedLocal(root, id) => {
            return invalidated(workflow_artifact_local::LocalArtifactStore::open(root)?, id);
        }
        ArtifactLocation::InvalidatedS3(binding, id) => {
            return invalidated(crate::artifact_objects::open(binding, false)?, id);
        }
    })
}
pub(crate) fn open(db: &str, artifacts: Option<&str>) -> Result<SqliteRunStore> {
    open_with(db, artifacts.map(ArtifactLocation::Local))
}
pub(crate) fn open_with(
    db: &str,
    artifacts: Option<ArtifactLocation<'_>>,
) -> Result<SqliteRunStore> {
    let store = SqliteRunStore::open(db)?;
    Ok(if let Some(location) = artifacts {
        store.with_artifacts(reader(location)?)
    } else {
        store
    })
}
fn execute(args: &[&str], artifacts: Option<ArtifactLocation<'_>>) -> Result<Value> {
    match args {
        ["run", "continuation", db, id] => report(open_with(db, artifacts)?.continuation(id)?),
        ["run", "continue", db, lease, file] => {
            let mut store = open_with(db, artifacts)?;
            let plan = store.prepare_continuation(
                &read(lease)?,
                &read(file)?,
                &workflow_worker::SystemClock,
            )?;
            report(store.start(&plan.successor)?)
        }
        ["run", "acceptance", db, id] => report(open_with(db, artifacts)?.acceptance(id)?),
        ["schema", "run-effect-http-binding"] => report(schemars::schema_for!(
            Vec<workflow_effect_http::HttpEffectBinding>
        )),
        [
            "schema",
            kind @ ("run-start"
            | "run-continuation"
            | "run-handoff"
            | "run-receipt"
            | "run-lease"
            | "run-execution-record"
            | "run-signal"
            | "run-migration-request"
            | "run-migration-plan"
            | "run-storage-upgrade"
            | "run-effect-attempt"
            | "run-effect-reply"
            | "run-effect-observation"
            | "run-effect-resolution"
            | "run-recovery-acknowledgement"
            | "run-restored-effect"),
        ] => Ok(workflow_worker::parse_message(
            schema(&kind[4..])?.as_bytes(),
        )?),
        ["run", "init", db] => {
            SqliteRunStore::create(db)?;
            Ok(json!({"initialized":true}))
        }
        ["run", "migration-plan", db, id, file] => {
            report(open_with(db, artifacts)?.plan_migration(id, &read(file)?)?)
        }
        ["run", "migration-apply", db, lease, plan, actor] => {
            report(open_with(db, artifacts)?.migrate_definition(
                &read(lease)?,
                &read(plan)?,
                actor,
                &workflow_worker::SystemClock,
            )?)
        }
        ["run", "history-at", db, id, revision] => {
            report(open_with(db, artifacts)?.historical_snapshot(id, number(revision)?)?)
        }
        ["run", "export", db, destination] => {
            let index = workflow_backup_local::create(
                &workflow_backup_local::BackupSources {
                    runs: db.into(),
                    artifacts: match artifacts {
                        Some(ArtifactLocation::Local(root)) => Some(root.into()),
                        Some(ArtifactLocation::S3(_) | ArtifactLocation::InvalidatedLocal(..) | ArtifactLocation::InvalidatedS3(..)) => return Err(Error::new(ErrorCode::InvalidRequest,"use the historical local artifact view for a local archive; object-backed archives also require their catalog and retained bucket")),
                        None => None,
                    },
                    registry: None,
                },
                destination,
                &workflow_worker::SystemClock,
            )
            .map_err(|e| Error::new(ErrorCode::Storage, e.to_string()))?;
            Ok(crate::backups::summary(&index))
        }
        ["run", "migrate", db] => {
            let mut current = open_with(db, artifacts)?;
            report(
                json!({"migrated":false,"storage_version":workflow_runstore_sqlite::STORAGE_VERSION,"history":current.storage_history()?}),
            )
        }
        ["run", "storage-plan", db] => report(SqliteRunStore::plan_storage_upgrade(
            db,
            artifacts.map(reader).transpose()?.as_deref(),
        )?),
        ["run", "migrate", db, backup] => {
            let (_, upgraded) = SqliteRunStore::upgrade_with_backup(
                db,
                backup,
                artifacts.map(reader).transpose()?,
            )?;
            report(upgraded)
        }
        ["run", "storage-history", db] => report(open_with(db, artifacts)?.storage_history()?),
        ["run", "storage-restore", backup, target, plan] => {
            let plan: workflow_runstore_sqlite::StorageUpgrade = read(plan)?;
            SqliteRunStore::restore_storage_backup(
                backup,
                target,
                &plan,
                artifacts.map(reader).transpose()?.as_deref(),
            )?;
            Ok(
                json!({"restored":true,"storage_version":plan.source_version,"verified_history_digest":plan.verified_history_digest}),
            )
        }
        ["run", "recovery", db, id] => report(open_with(db, artifacts)?.recovery_barrier(id)?),
        ["run", "recovery-acknowledge", db, id, file] => {
            let resolution: RecoveryAcknowledgement = read(file)?;
            let mut store = open_with(db, artifacts)?;
            let duplicate =
                store.acknowledge_recovery(id, &resolution, &workflow_worker::SystemClock)?;
            Ok(
                json!({"acknowledged_generation": resolution.generation, "duplicate": duplicate, "pending_recovery": store.recovery_barrier(id)?}),
            )
        }
        ["run", "effect-import", db, lease, file] => {
            report(open_with(db, artifacts)?.import_restored_effect(
                &read(lease)?,
                &read(file)?,
                &workflow_worker::SystemClock,
            )?)
        }
        ["run", "effects", db, id, after, limit] => {
            report(open_with(db, artifacts)?.effects(id, number(after)?, number(limit)?)?)
        }
        ["run", "effect-claim", db, lease] => report(
            open_with(db, artifacts)?.claim_effect(&read(lease)?, &workflow_worker::SystemClock)?,
        ),
        ["run", "effect-observe", db, lease, attempt, file] => {
            report(open_with(db, artifacts)?.observe_effect(
                &read(lease)?,
                attempt,
                &read(file)?,
                &workflow_worker::SystemClock,
            )?)
        }
        ["run", "effect-resolve", db, lease, key, file] => {
            report(open_with(db, artifacts)?.resolve_effect(
                &read(lease)?,
                key,
                &read(file)?,
                &workflow_worker::SystemClock,
            )?)
        }
        ["run", "execution-history", db, id, after, limit] => report(
            open_with(db, artifacts)?.execution_history(id, number(after)?, number(limit)?)?,
        ),
        ["run", "acquire", db, file] => {
            report(open_with(db, artifacts)?.acquire(&read(file)?, &workflow_worker::SystemClock)?)
        }
        ["run", "renew", db, file, ttl] => report(open_with(db, artifacts)?.renew(
            &read(file)?,
            number(ttl)?,
            &workflow_worker::SystemClock,
        )?),
        ["run", "release", db, file] => {
            open_with(db, artifacts)?.release(&read(file)?, &workflow_worker::SystemClock)?;
            Ok(json!({"released":true}))
        }
        ["run", "claim", db, file] => report(
            open_with(db, artifacts)?.claim_next(&read(file)?, &workflow_worker::SystemClock)?,
        ),
        ["run", "retry-gate", db, id, instance, event_id, revision] => {
            let mut store = open_with(db, artifacts)?;
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
            report(open_with(db, artifacts)?.tick_due(&read(file)?, &workflow_worker::SystemClock)?)
        }
        ["run", "finish", db, lease, attempt, result] => {
            report(open_with(db, artifacts)?.finish_task(
                &read(lease)?,
                attempt,
                &read(result)?,
                &workflow_worker::SystemClock,
            )?)
        }
        ["run", "attempt-failed", db, lease, attempt, error] => {
            open_with(db, artifacts)?.fail_task(
                &read(lease)?,
                attempt,
                &read(error)?,
                &workflow_worker::SystemClock,
            )?;
            Ok(json!({"recorded":true}))
        }
        ["run", "start", db, file] => {
            let request = read(file)?;
            report(open_with(db, artifacts)?.start(&request)?)
        }
        ["run", "status", db, id] => report(open_with(db, artifacts)?.get(id)?),
        ["run", "receive", db, file] => report(
            open_with(db, artifacts)?
                .receive_signal(&read(file)?, &workflow_worker::SystemClock)?,
        ),
        ["run", "inbox", db, id, after, limit] => {
            report(open_with(db, artifacts)?.inbox(id, number(after)?, number(limit)?)?)
        }
        ["run", "waits", db, id, after, limit] => {
            report(open_with(db, artifacts)?.waits(id, number(after)?, number(limit)?)?)
        }
        ["run", "event", db, file] => {
            let event = read(file)?;
            report(open_with(db, artifacts)?.apply(&event)?)
        }
        ["run", "cancel", db, id, event_id, revision, at] => {
            let mut store = open_with(db, artifacts)?;
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
        [
            "run",
            control @ ("pause" | "resume"),
            db,
            id,
            event_id,
            revision,
            at,
            reason,
        ] => {
            let mut store = open_with(db, artifacts)?;
            let snapshot = store.get(id)?;
            let kind = if *control == "pause" {
                workflow_kernel::EventKind::Pause {
                    reason: (*reason).into(),
                }
            } else {
                workflow_kernel::EventKind::Resume {
                    reason: (*reason).into(),
                }
            };
            report(store.apply(&Event {
                event_id: (*event_id).into(),
                run_id: (*id).into(),
                run_digest: snapshot.run_digest,
                expected_revision: number(revision)?,
                at_unix_ms: number(at)?,
                kind,
            })?)
        }
        ["run", "list", db, after, limit] => report(open_with(db, artifacts)?.list(
            if *after == "-" { None } else { Some(after) },
            number(limit)?,
        )?),
        ["run", "history", db, id, after, limit] => {
            report(open_with(db, artifacts)?.history(id, number(after)?, number(limit)?)?)
        }
        ["run", "history-usage", db, id] => report(open_with(db, artifacts)?.history_usage(id)?),
        [
            "run",
            "outbox",
            db,
            id,
            after,
            limit,
            mode @ ("all" | "pending"),
        ] => report(open_with(db, artifacts)?.outbox(
            id,
            number(after)?,
            number(limit)?,
            *mode == "pending",
        )?),
        ["run", "acknowledge", db, file] => {
            let receipt = read(file)?;
            report(open_with(db, artifacts)?.acknowledge(&receipt)?)
        }
        ["run", "verify", db, id] => report(open_with(db, artifacts)?.verify(id)?),
        _ => Err(Error::new(ErrorCode::InvalidRequest, "usage")),
    }
}
pub(crate) fn drive(
    db: &str,
    id: &str,
    owner: &str,
    budget: &str,
    artifacts: Option<ArtifactLocation<'_>>,
    models: Option<&str>,
    effects: Option<&str>,
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
    let mut store = open_with(db, artifacts)?;
    let bundle = store.bundle(id)?;
    let bindings = models
        .map(crate::models::bindings)
        .transpose()?
        .unwrap_or_default();
    // Validate bindings before acquiring a lease or launching a process.
    crate::models::bound_worker(&bundle, &bindings, false)?;
    let worker = crate::activity::Config {
        bundle: Some(bundle),
        models: bindings,
        allow_unused: false,
        remote: None,
        workspace: None,
        journal: Some(crate::activity::JournalBinding::Local {
            journal: crate::activity::LocalJournal::new(db, artifacts),
        }),
    };
    let result = if let Some(file) = effects {
        let adapters = workflow_effect_http::HttpEffects::new(read(file)?)?;
        workflow_runtime::drive_with_effects(
            &mut store,
            &worker,
            &adapters,
            id,
            &options,
            &workflow_worker::SystemClock,
        )?
    } else {
        workflow_runtime::drive(
            &mut store,
            &worker,
            id,
            &options,
            &workflow_worker::SystemClock,
        )?
    };
    Ok(report(result)?)
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let mut normalized = vec![];
    let (args, artifacts) = if let [
        "run",
        mode @ ("--revalidated-artifacts" | "--revalidated-object-artifacts"),
        root,
        plan,
        tail @ ..,
    ] = args
    {
        normalized.push("run");
        normalized.extend_from_slice(tail);
        (
            normalized.as_slice(),
            Some(if *mode == "--revalidated-artifacts" {
                ArtifactLocation::InvalidatedLocal(root, plan)
            } else {
                ArtifactLocation::InvalidatedS3(root, plan)
            }),
        )
    } else if let [
        "run",
        mode @ ("--artifacts" | "--object-artifacts"),
        root,
        tail @ ..,
    ] = args
    {
        normalized.push("run");
        normalized.extend_from_slice(tail);
        (
            normalized.as_slice(),
            Some(if *mode == "--artifacts" {
                ArtifactLocation::Local(root)
            } else {
                ArtifactLocation::S3(root)
            }),
        )
    } else {
        (args, None)
    };
    let outcome = if let ["run", "drive-workspaces", db, id, owner, budget, binding] = args {
        drive_workspaces(db, id, owner, budget, artifacts, binding)
            .map_err(|e| (e.message.clone(), json!(e)))
    } else if let ["run", "drive", db, id, owner, budget] = args {
        drive(db, id, owner, budget, artifacts, None, None)
            .map_err(|e| (e.message.clone(), json!(e)))
    } else if let ["run", "drive-models", db, id, owner, budget, file] = args {
        drive(db, id, owner, budget, artifacts, Some(file), None)
            .map_err(|e| (e.message.clone(), json!(e)))
    } else if let [
        "run",
        "drive-effects",
        db,
        id,
        owner,
        budget,
        effects,
        models,
    ] = args
    {
        drive(
            db,
            id,
            owner,
            budget,
            artifacts,
            Some(models),
            Some(effects),
        )
        .map_err(|e| (e.message.clone(), json!(e)))
    } else if let ["run", "drive-effects", db, id, owner, budget, file] = args {
        drive(db, id, owner, budget, artifacts, None, Some(file))
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

fn drive_workspaces(
    db: &str,
    id: &str,
    owner: &str,
    budget: &str,
    location: Option<ArtifactLocation<'_>>,
    binding: &str,
) -> std::result::Result<Value, workflow_runtime::Error> {
    let executor = crate::activity::Config::workspace(read(binding)?, location)?;
    let mut store = open_with(db, location)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "clock unavailable"))?
        .as_nanos();
    let options = workflow_runtime::DriveOptions {
        owner: owner.into(),
        acquisition_id: format!("workspace-cli-{}-{nonce}", std::process::id()),
        lease_ms: 120_000,
        max_commands: number(budget)?,
    };
    Ok(report(workflow_runtime::drive(
        &mut store,
        &executor,
        id,
        &options,
        &workflow_worker::SystemClock,
    )?)?)
}
#[cfg(test)]
mod tests;
