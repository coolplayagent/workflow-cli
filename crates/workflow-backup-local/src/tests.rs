use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use workflow_artifacts::{AccessScope, PublishSpec, Retention, SourceRevision};
use workflow_definitions::DefinitionRegistry;
use workflow_kernel::{EventKind, SignalDecision};
use workflow_runstore::*;
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "workflow-backup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Time(u64);
impl Clock for Time {
    fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
        Ok(self.0)
    }
}
fn base() -> PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn start(path: &str) -> StartRun {
    workflow_worker::parse_message(&std::fs::read(base().join(path)).unwrap()).unwrap()
}
fn lease(s: &mut SqliteRunStore, id: &str, owner: &str, c: &dyn Clock) -> Lease {
    s.acquire(
        &LeaseRequest {
            run_id: id.into(),
            owner: owner.into(),
            acquisition_id: format!("claim-{owner}"),
            ttl_ms: 10000,
        },
        c,
    )
    .unwrap()
}
fn event(s: &Snapshot, id: &str, kind: EventKind, at: u64) -> Event {
    Event {
        event_id: id.into(),
        run_id: s.run_id.clone(),
        run_digest: s.run_digest.clone(),
        expected_revision: s.revision,
        at_unix_ms: at,
        kind,
    }
}
struct Fixture {
    dir: Dir,
    sources: BackupSources,
    guarded: StartRun,
    review: StartRun,
    old_lease: Lease,
}
impl Fixture {
    fn new() -> Self {
        let dir = Dir::new();
        let sources = BackupSources {
            runs: dir.0.join("source.db"),
            artifacts: Some(dir.0.join("source-artifacts")),
            registry: Some(dir.0.join("definitions.db")),
        };
        let mut artifacts =
            LocalArtifactStore::create(sources.artifacts.as_ref().unwrap()).unwrap();
        let mut s = SqliteRunStore::create(&sources.runs)
            .unwrap()
            .with_artifacts(Box::new(
                LocalArtifactStore::open(sources.artifacts.as_ref().unwrap()).unwrap(),
            ));
        let guarded = start("examples/gates/guarded-start.json");
        s.start(&guarded).unwrap();
        let c = Time(1000);
        let old_lease = lease(&mut s, &guarded.run_id, "before-backup", &c);
        let Claimed::Task { attempt } = s.claim_next(&old_lease, &c).unwrap() else {
            panic!()
        };
        let mut result = workflow_builtin_capabilities::worker()
            .unwrap()
            .execute_with_clock(&attempt.request, &attempt.grant, &c)
            .unwrap()
            .into_result();
        let spec = PublishSpec {
            schema_version: 1,
            artifact_type: guarded.bundle.postconditions[0].policy.requirements[0]
                .report_type
                .clone(),
            producer: artifact_producer(&attempt.request).unwrap(),
            source_revision: SourceRevision {
                repository: "fixture-repository".into(),
                revision: "a".repeat(40),
            },
            inputs: vec![],
            access: AccessScope::Run {
                run_id: guarded.run_id.clone(),
            },
            retention: Retention::RunDependency,
        };
        let report = artifacts
            .publish(&spec, &mut br#"{"valid":true}"#.as_slice())
            .unwrap();
        let workflow_worker::AdapterOutcome::Succeeded { evidence, .. } = &mut result.outcome
        else {
            panic!()
        };
        evidence.push(workflow_worker::EvidenceRef {
            artifact_id: report.artifact_id,
            digest: report.digest,
        });
        s.finish_task(&old_lease, &attempt.attempt_id, &result, &c)
            .unwrap();
        let review = start("examples/runs/review-start.json");
        let snapshot = s.start(&review).unwrap().snapshot;
        s.apply(&event(
            &snapshot,
            "paused-for-backup",
            EventKind::Pause {
                reason: "fixture pause".into(),
            },
            1000,
        ))
        .unwrap();
        let wait = s.waits(&review.run_id, 0, 100).unwrap().items.remove(0);
        s.receive_signal(
            &SignalSubmission {
                schema_version: 1,
                run_id: review.run_id.clone(),
                run_digest: snapshot.run_digest,
                message: workflow_kernel::SignalMessage {
                    schema_version: 1,
                    message_id: "fixture-callback".into(),
                    correlation_id: wait.correlation_id,
                    target: wait.target,
                    source: "simulated-test-approver".into(),
                    decision: SignalDecision::Approve,
                    reason: "test-only callback".into(),
                    outputs: Values::new(),
                    expires_at_unix_ms: 60000,
                },
            },
            &c,
        )
        .unwrap();
        let mut registry =
            workflow_registry_sqlite::SqliteRegistry::create(sources.registry.as_ref().unwrap())
                .unwrap();
        registry
            .create_draft("retained-definition", &review.bundle.workflows[0])
            .unwrap();
        registry.publish("retained-definition", 1).unwrap();
        registry.delete_draft("retained-definition", 1).unwrap();
        Self {
            dir,
            sources,
            guarded,
            review,
            old_lease,
        }
    }
    fn store(&self) -> SqliteRunStore {
        SqliteRunStore::open(&self.sources.runs)
            .unwrap()
            .with_artifacts(Box::new(
                LocalArtifactStore::open(self.sources.artifacts.as_ref().unwrap()).unwrap(),
            ))
    }
}
#[test]
fn relocation_retains_gated_artifacts_deleted_definition_history_and_pending_inbox_but_fences_old_lease()
 {
    let f = Fixture::new();
    let destination = f.dir.0.join("backup");
    let before = f.store().get(&f.review.run_id).unwrap();
    let guarded_before = f.store().get(&f.guarded.run_id).unwrap();
    let index = create(&f.sources, &destination, &Time(1050)).unwrap();
    assert_eq!(index.manifest.artifact_manifests, 1);
    assert_eq!(index.manifest.runs.len(), 2);
    let archive = LocalBackup::open(&destination).unwrap();
    let moved = Dir::new();
    let restored = moved.0.join("restored");
    let report = restore(
        &archive,
        &restored,
        &RestoreRequest {
            actor: "test-operator".into(),
            reason: "source stopped for relocation fixture".into(),
        },
        &Time(1100),
    )
    .unwrap();
    let mut s = open_runs(&restored, true).unwrap();
    assert_eq!(s.get(&f.review.run_id).unwrap(), before);
    assert_eq!(s.inbox(&f.review.run_id, 0, 100).unwrap().items.len(), 1);
    assert_eq!(
        s.get(&f.guarded.run_id).unwrap().frames,
        guarded_before.frames
    );
    assert!(s.get(&f.guarded.run_id).unwrap().pause.is_some());
    assert_eq!(
        s.renew(&f.old_lease, 20000, &Time(1100)).unwrap_err().code,
        workflow_runstore::ErrorCode::LeaseConflict
    );
    let fresh = lease(&mut s, &f.guarded.run_id, "restored-owner", &Time(1100));
    assert_eq!(fresh.epoch, f.old_lease.epoch + 1);
    assert_eq!(fresh.generation, Some(report.generation.clone()));
    assert_eq!(
        s.recovery_barrier(&f.guarded.run_id)
            .unwrap()
            .unwrap()
            .backup_digest,
        index.digest
    );
    let r =
        workflow_registry_sqlite::SqliteRegistry::open(restored.join("registry.sqlite")).unwrap();
    r.verify_all().unwrap();
    assert!(r.get_draft("retained-definition").is_err());
    assert_eq!(
        r.get_published(
            &f.review.bundle.workflows[0].id,
            &f.review.bundle.workflows[0].version
        )
        .unwrap()
        .workflow,
        workflow_registry_sqlite::SqliteRegistry::open(f.sources.registry.as_ref().unwrap())
            .unwrap()
            .get_published(
                &f.review.bundle.workflows[0].id,
                &f.review.bundle.workflows[0].version
            )
            .unwrap()
            .workflow
    );
    // Recovered artifacts can execute both gates after an explicit resume; the
    // original checked task remains settled and is not invoked again.
    let snapshot = s.get(&f.guarded.run_id).unwrap();
    s.apply(&event(
        &snapshot,
        "resume-restored",
        EventKind::Resume {
            reason: "inspect pending evidence gates".into(),
        },
        1100,
    ))
    .unwrap();
    for _ in 0..10 {
        if s.get(&f.guarded.run_id).unwrap().status != RunStatus::Running {
            break;
        }
        assert!(matches!(
            s.claim_next(&fresh, &Time(1100)).unwrap(),
            Claimed::Handled { .. }
        ));
    }
    assert_eq!(
        s.get(&f.guarded.run_id).unwrap().status,
        RunStatus::Succeeded
    );
    s.verify(&f.guarded.run_id).unwrap();
    s.verify(&f.review.run_id).unwrap();
}

fn provider_apply(
    path: &Path,
    p: &workflow_effects::EffectAttempt,
) -> workflow_effects::Observation {
    use rusqlite::OptionalExtension;
    let mut c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch("PRAGMA synchronous=FULL;CREATE TABLE IF NOT EXISTS releases(key TEXT PRIMARY KEY,intent TEXT NOT NULL,receipt TEXT NOT NULL);CREATE TABLE IF NOT EXISTS calls(kind TEXT);").unwrap();
    let tx = c
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "INSERT INTO calls VALUES(?1)",
        [if p.kind == workflow_effects::CallKind::Write {
            "write"
        } else {
            "query"
        }],
    )
    .unwrap();
    let found: Option<String> = tx
        .query_row(
            "SELECT receipt FROM releases WHERE key=?1",
            [&p.intent.operation_key],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    let result = if let Some(s) = found {
        serde_json::from_str(&s).unwrap()
    } else if p.kind == workflow_effects::CallKind::Query {
        workflow_effects::Observation::Absent
    } else {
        let result = workflow_effects::Observation::Applied {
            receipt: workflow_effects::EffectReceipt {
                operation_key: p.intent.operation_key.clone(),
                intent_digest: workflow_effects::digest(&p.intent).unwrap(),
                target: p.intent.policy.target.clone(),
                resource_id: "backup-release".into(),
                provider_receipt: "provider-record-1".into(),
                outputs: [("release_id".into(), serde_json::json!("backup-release"))].into(),
            },
        };
        tx.execute(
            "INSERT INTO releases VALUES(?1,?2,?3)",
            rusqlite::params![
                p.intent.operation_key,
                serde_json::to_string(&p.intent).unwrap(),
                serde_json::to_string(&result).unwrap()
            ],
        )
        .unwrap();
        result
    };
    tx.commit().unwrap();
    result
}
#[test]
fn post_backup_provider_effect_is_queried_or_imported_without_a_second_write() {
    for intent_in_backup in [true, false] {
        let dir = Dir::new();
        let source = dir.0.join("runs.db");
        let mut s = SqliteRunStore::create(&source).unwrap();
        let request = start("examples/runs/effect-release.json");
        s.start(&request).unwrap();
        let old = lease(&mut s, &request.run_id, "old", &Time(1000));
        let mut original = None;
        if intent_in_backup {
            let EffectClaim::Call { attempt } = s.claim_effect(&old, &Time(1000)).unwrap() else {
                panic!()
            };
            original = Some(*attempt);
        }
        let backup_dir = dir.0.join("backup");
        let index = create(
            &BackupSources {
                runs: source,
                artifacts: None,
                registry: None,
            },
            &backup_dir,
            &Time(1000),
        )
        .unwrap();
        if original.is_none() {
            let EffectClaim::Call { attempt } = s.claim_effect(&old, &Time(1000)).unwrap() else {
                panic!()
            };
            original = Some(*attempt);
        }
        let p = original.unwrap();
        // Source issued the write after the snapshot. Provider commits, while
        // no receipt is ever settled in the source run DB before disaster.
        let provider = dir.0.join("provider.db");
        provider_apply(&provider, &p);
        drop(s);
        let restored_dir = dir.0.join("restored");
        let restored = restore(
            &LocalBackup::open(backup_dir).unwrap(),
            &restored_dir,
            &RestoreRequest {
                actor: "test-operator".into(),
                reason: "simulated source loss; provider retained".into(),
            },
            &Time(1100),
        )
        .unwrap();
        let mut s = open_runs(&restored_dir, false).unwrap();
        let fresh = lease(&mut s, &request.run_id, "new", &Time(1100));
        let snapshot = s.get(&request.run_id).unwrap();
        s.apply(&event(
            &snapshot,
            "resume-for-reconciliation",
            EventKind::Resume {
                reason: "query/import under recovery barrier".into(),
            },
            1100,
        ))
        .unwrap();
        let acknowledgement = RecoveryAcknowledgement {
            no_missing_effect_intents: true,
            resolution_id: "recovery-reviewed".into(),
            generation: restored.generation,
            backup_digest: index.digest,
            actor: "test-operator".into(),
            evidence: "provider releases inventory inspected; source process stopped".into(),
            reason: "retain one actual release".into(),
        };
        if intent_in_backup {
            assert_eq!(
                s.acknowledge_recovery(&request.run_id, &acknowledgement, &Time(1100))
                    .unwrap_err()
                    .code,
                workflow_runstore::ErrorCode::RecoveryRequired
            );
            let EffectClaim::Call { attempt } = s.claim_effect(&fresh, &Time(1100)).unwrap() else {
                panic!()
            };
            assert_eq!(attempt.kind, workflow_effects::CallKind::Query);
            assert_eq!(attempt.intent, p.intent);
            s.observe_effect(
                &fresh,
                &attempt.attempt_id,
                &provider_apply(&provider, &attempt),
                &Time(1100),
            )
            .unwrap();
        } else {
            assert_eq!(
                s.claim_effect(&fresh, &Time(1100)).unwrap_err().code,
                workflow_runstore::ErrorCode::RecoveryRequired
            );
            assert!(s.effects(&request.run_id, 0, 100).unwrap().items.is_empty());
            let audit = rusqlite::Connection::open(&provider).unwrap();
            let (original_intent, actual_receipt): (String, String) = audit
                .query_row(
                    "SELECT intent,receipt FROM releases WHERE key=?1",
                    [&p.intent.operation_key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let workflow_effects::Observation::Applied { receipt } =
                serde_json::from_str(&actual_receipt).unwrap()
            else {
                panic!()
            };
            let import = RestoredEffect {
                intent: serde_json::from_str(&original_intent).unwrap(),
                resolution: workflow_effects::ManualResolution {
                    resolution_id: "import-provider-audit".into(),
                    actor: "test-operator".into(),
                    reason: "post-backup intent and receipt obtained from provider audit".into(),
                    evidence: receipt.provider_receipt.clone(),
                    outcome: workflow_effects::ManualOutcome::Applied { receipt },
                },
            };
            let mut wrong = import.clone();
            wrong
                .intent
                .inputs
                .insert("release_name".into(), serde_json::json!("different"));
            assert!(
                s.import_restored_effect(&fresh, &wrong, &Time(1100))
                    .is_err()
            );
            s.import_restored_effect(&fresh, &import, &Time(1100))
                .unwrap();
            assert!(
                s.import_restored_effect(&fresh, &import, &Time(1100))
                    .unwrap()
                    .transition
                    .duplicate
            );
            let effect = s.effects(&request.run_id, 0, 100).unwrap().items.remove(0);
            assert_eq!(effect.intent, p.intent);
            assert!(effect.calls.is_empty()); // No invented missing attempt history.
        }
        assert_eq!(s.get(&request.run_id).unwrap().status, RunStatus::Succeeded);
        assert!(
            !s.acknowledge_recovery(&request.run_id, &acknowledgement, &Time(1100))
                .unwrap()
        );
        assert!(
            s.acknowledge_recovery(&request.run_id, &acknowledgement, &Time(1100))
                .unwrap()
        );
        assert!(s.recovery_barrier(&request.run_id).unwrap().is_none());
        s.verify(&request.run_id).unwrap();
        let provider = rusqlite::Connection::open(provider).unwrap();
        assert_eq!(
            provider
                .query_row("SELECT count(*) FROM releases", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            provider
                .query_row("SELECT count(*) FROM calls WHERE kind='write'", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            1
        );
        assert_eq!(
            provider
                .query_row("SELECT count(*) FROM calls WHERE kind='query'", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            i64::from(intent_in_backup)
        );
    }
}

#[test]
fn tampering_missing_files_symlinks_and_unlisted_bytes_are_rejected_before_restore() {
    for damage in [
        "missing-object",
        "changed-object",
        "extra-file",
        "symlink",
        "manifest",
        "missing-db",
    ] {
        let f = Fixture::new();
        let destination = f.dir.0.join("backup");
        create(&f.sources, &destination, &Time(1000)).unwrap();
        let object = std::fs::read_dir(destination.join("artifacts/objects"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        match damage {
            "missing-object" => std::fs::remove_file(object).unwrap(),
            "changed-object" => std::fs::write(object, b"false").unwrap(),
            "extra-file" => std::fs::write(destination.join("extra.txt"), b"unlisted").unwrap(),
            "symlink" => {
                std::fs::remove_file(&object).unwrap();
                std::os::unix::fs::symlink(f.sources.runs.clone(), object).unwrap();
            }
            "manifest" => {
                let p = destination.join("backup.json");
                let mut x: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
                x["manifest"]["created_at_unix_ms"] = serde_json::json!(999);
                std::fs::write(p, serde_json::to_vec(&x).unwrap()).unwrap();
            }
            _ => std::fs::remove_file(destination.join("runs.sqlite")).unwrap(),
        }
        assert!(
            LocalBackup::open(&destination).is_err(),
            "accepted {damage}"
        );
        assert!(!f.dir.0.join("restored").exists());
        f.store().verify(&f.guarded.run_id).unwrap();
    }
}
#[test]
fn snapshot_is_consistent_when_source_accepts_later_commits_and_publication_never_overwrites() {
    let f = Fixture::new();
    let destination = f.dir.0.join("backup");
    let mut later = f.review.clone();
    later.run_id = "after-backup".into();
    let index = create_internal(&f.sources, &destination, &Time(1000), |phase| {
        if phase == "run_snapshot" {
            f.store().start(&later).unwrap();
        }
    })
    .unwrap();
    assert_eq!(index.manifest.runs.len(), 2);
    assert_eq!(f.store().list(None, 100).unwrap().items.len(), 3);
    LocalBackup::open(&destination).unwrap();
    assert!(create(&f.sources, &destination, &Time(1000)).is_err());
    let contested = f.dir.0.join("contested");
    assert!(
        create_internal(&f.sources, &contested, &Time(1000), |phase| {
            if phase == "before_publish" {
                std::fs::create_dir(&contested).unwrap();
                std::fs::write(contested.join("keep"), b"existing owner").unwrap();
            }
        })
        .is_err()
    );
    assert_eq!(
        std::fs::read(contested.join("keep")).unwrap(),
        b"existing owner"
    );
    assert!(!contested.join("runs.sqlite").exists());
    assert!(
        restore(
            &LocalBackup::open(destination).unwrap(),
            &contested,
            &RestoreRequest {
                actor: "operator".into(),
                reason: "must not overwrite".into()
            },
            &Time(1100)
        )
        .is_err()
    );
}
fn child(f: &Fixture, mode: &str, phase: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::process_worker", "--nocapture"])
        .env("WORKFLOW_BACKUP_TEST_DIR", &f.dir.0)
        .env("WORKFLOW_BACKUP_TEST_MODE", mode)
        .env("WORKFLOW_BACKUP_TEST_PHASE", phase)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}
#[test]
fn process_worker() {
    let Ok(dir) = std::env::var("WORKFLOW_BACKUP_TEST_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mode = std::env::var("WORKFLOW_BACKUP_TEST_MODE").unwrap();
    let phase = std::env::var("WORKFLOW_BACKUP_TEST_PHASE").unwrap();
    let hook = |at: &str| {
        if phase == at {
            std::fs::write(dir.join("ready"), b"ready").unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    let sources: BackupSources =
        serde_json::from_slice(&std::fs::read(dir.join("sources.json")).unwrap()).unwrap();
    if mode == "create" {
        create_internal(&sources, &dir.join("backup"), &Time(1000), hook).unwrap();
    } else if mode == "restore" {
        restore_internal(
            &LocalBackup::open(dir.join("backup")).unwrap(),
            &dir.join("restored"),
            &RestoreRequest {
                actor: "test-operator".into(),
                reason: "kill during restore fixture".into(),
            },
            &Time(1100),
            hook,
        )
        .unwrap();
    } else {
        let limit = libc::rlimit {
            rlim_cur: 4096,
            rlim_max: 4096,
        };
        // Limits apply only inside this dedicated child. Ignore SIGXFSZ so the
        // actual failing file write can be reported through the normal error path.
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
        }
        let error = create(&sources, dir.join("backup"), &Time(1000)).unwrap_err();
        std::fs::write(
            dir.join("write-error.json"),
            serde_json::to_vec(&error).unwrap(),
        )
        .unwrap();
    }
}
#[test]
fn killed_backup_and_restore_publish_nothing_or_a_complete_fenced_directory() {
    for (mode, phases) in [
        (
            "create",
            vec!["run_snapshot", "before_publish", "after_publish"],
        ),
        (
            "restore",
            vec![
                "before_fence",
                "after_fence",
                "before_publish",
                "after_publish",
            ],
        ),
    ] {
        for phase in phases {
            let f = Fixture::new();
            std::fs::write(
                f.dir.0.join("sources.json"),
                serde_json::to_vec(&f.sources).unwrap(),
            )
            .unwrap();
            if mode == "restore" {
                create(&f.sources, f.dir.0.join("backup"), &Time(1000)).unwrap();
            }
            let mut worker = child(&f, mode, phase);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !f.dir.0.join("ready").exists() {
                assert!(
                    worker.try_wait().unwrap().is_none(),
                    "child exited before {mode}/{phase}"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "child timeout {mode}/{phase}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            worker.kill().unwrap();
            worker.wait().unwrap();
            let destination = f.dir.0.join(if mode == "create" {
                "backup"
            } else {
                "restored"
            });
            if phase == "after_publish" {
                if mode == "create" {
                    LocalBackup::open(&destination).unwrap();
                } else {
                    let mut s = open_runs(&destination, true).unwrap();
                    s.verify(&f.guarded.run_id).unwrap();
                    assert!(s.recovery_barrier(&f.guarded.run_id).unwrap().is_some());
                    assert_eq!(
                        s.renew(&f.old_lease, 20000, &Time(1100)).unwrap_err().code,
                        workflow_runstore::ErrorCode::LeaseConflict
                    );
                    assert!(destination.join("restore.json").exists());
                }
            } else {
                assert!(!destination.exists(), "partial {mode} exposed at {phase}");
            }
            f.store().verify(&f.guarded.run_id).unwrap();
        }
    }
}
#[test]
fn actual_child_file_write_limit_failure_does_not_publish_or_modify_source() {
    let f = Fixture::new();
    std::fs::write(
        f.dir.0.join("sources.json"),
        serde_json::to_vec(&f.sources).unwrap(),
    )
    .unwrap();
    let before = f.store().get(&f.guarded.run_id).unwrap();
    let status = child(&f, "write-limit", "").wait().unwrap();
    assert!(status.success());
    let error: workflow_backups::Error =
        serde_json::from_slice(&std::fs::read(f.dir.0.join("write-error.json")).unwrap()).unwrap();
    assert_eq!(error.code, workflow_backups::ErrorCode::Storage);
    assert!(!f.dir.0.join("backup").exists());
    assert_eq!(f.store().get(&f.guarded.run_id).unwrap(), before);
}

#[test]
fn recomputed_file_hashes_cannot_hide_inconsistent_definition_history() {
    let f = Fixture::new();
    let destination = f.dir.0.join("backup");
    let mut index = create(&f.sources, &destination, &Time(1000)).unwrap();
    let c = rusqlite::Connection::open(destination.join("registry.sqlite")).unwrap();
    c.execute("UPDATE drafts SET deleted=0", []).unwrap();
    drop(c);
    index.manifest.files = files::inventory(&destination).unwrap();
    let changed = BackupIndex::seal(index.manifest).unwrap();
    std::fs::write(
        destination.join("backup.json"),
        serde_json::to_vec(&changed).unwrap(),
    )
    .unwrap();
    let error = match LocalBackup::open(destination) {
        Ok(_) => panic!("accepted inconsistent head"),
        Err(e) => e,
    };
    assert!(error.message.contains("deletion status"), "{error}");
}
#[test]
fn post_backup_source_lease_with_the_same_epoch_is_fenced_and_old_ack_cannot_clear_a_later_restore()
{
    let dir = Dir::new();
    let source = dir.0.join("source.db");
    let mut s = SqliteRunStore::create(&source).unwrap();
    let start = start("examples/runs/effect-release.json");
    s.start(&start).unwrap();
    let first = lease(&mut s, &start.run_id, "first", &Time(1000));
    s.release(&first, &Time(1001)).unwrap();
    let backup = dir.0.join("backup");
    let index = create(
        &BackupSources {
            runs: source,
            artifacts: None,
            registry: None,
        },
        &backup,
        &Time(1001),
    )
    .unwrap();
    let post_backup_owner = lease(&mut s, &start.run_id, "same-owner", &Time(1100));
    drop(s); // Fixture source is actually quiescent; no provider was ever invoked.
    let target = dir.0.join("restored");
    let request = RestoreRequest {
        actor: "test-operator".into(),
        reason: "source stopped with no admitted writes".into(),
    };
    let report = restore(
        &LocalBackup::open(backup).unwrap(),
        &target,
        &request,
        &Time(1100),
    )
    .unwrap();
    let mut s = open_runs(&target, false).unwrap();
    let current = lease(&mut s, &start.run_id, "same-owner", &Time(1100));
    let mut old_shape = current.clone();
    old_shape.generation = None;
    assert_eq!(old_shape, post_backup_owner);
    assert_eq!(
        s.renew(&post_backup_owner, 20000, &Time(1100))
            .unwrap_err()
            .code,
        workflow_runstore::ErrorCode::LeaseConflict
    );
    let ack = RecoveryAcknowledgement {
        resolution_id: "review-first-restore".into(),
        no_missing_effect_intents: true,
        generation: report.generation,
        backup_digest: index.digest,
        actor: "test-operator".into(),
        reason: "all source calls accounted for".into(),
        evidence: "controlled fixture source execution journal has only lease actions".into(),
    };
    s.acknowledge_recovery(&start.run_id, &ack, &Time(1100))
        .unwrap();
    drop(s);
    let second_backup = dir.0.join("second-backup");
    create(
        &BackupSources {
            runs: target.join("runs.sqlite"),
            artifacts: None,
            registry: None,
        },
        &second_backup,
        &Time(1200),
    )
    .unwrap();
    let second_target = dir.0.join("second-restored");
    restore(
        &LocalBackup::open(second_backup).unwrap(),
        &second_target,
        &request,
        &Time(1300),
    )
    .unwrap();
    let mut second = open_runs(&second_target, false).unwrap();
    assert!(
        second
            .acknowledge_recovery(&start.run_id, &ack, &Time(1300))
            .unwrap()
    );
    assert!(second.recovery_barrier(&start.run_id).unwrap().is_some());
    assert_eq!(
        second.renew(&current, 20000, &Time(1300)).unwrap_err().code,
        workflow_runstore::ErrorCode::LeaseConflict
    );
    second.verify(&start.run_id).unwrap();
}
