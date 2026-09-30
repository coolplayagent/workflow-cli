use super::*;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};
use workflow_ir::*;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(crate) struct Database(pub(crate) PathBuf);
impl Database {
    pub(crate) fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "workflow-registry-{}-{}.sqlite",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn registry(&self) -> SqliteRegistry {
        SqliteRegistry::create(&self.0).unwrap()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        for suffix in ["-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}
fn sample() -> Workflow {
    WorkflowBuilder::new("review", "1", "done")
        .node(Node {
            id: "done".into(),
            kind: NodeKind::Terminal {
                outcome: TerminalOutcome::Succeeded,
            },
            inputs: Contract::new(),
            outputs: Contract::new(),
            bindings: BTreeMap::new(),
            preconditions: vec![],
        })
        .build()
}
fn edit(revision: u64, version: &str) -> Patch {
    Patch {
        expected_revision: revision,
        operations: vec![Edit::SetVersion {
            version: version.into(),
        }],
    }
}

#[test]
fn persists_history_and_publications_across_reopen_and_deletion() {
    let db = Database::new();
    let mut registry = db.registry();
    let first = registry.create_draft("review", &sample()).unwrap();
    let published = registry.publish("review", 1).unwrap();
    assert_eq!(registry.publish("review", 1).unwrap(), published);
    let next = registry.edit_draft("review", &edit(1, "2")).unwrap();
    assert_eq!(next.revision, 2);
    registry.publish("review", 2).unwrap();
    assert_eq!(registry.get_published("review", "1").unwrap(), published);
    assert_eq!(registry.get_revision("review", 1).unwrap(), first);
    drop(registry);
    let mut registry = SqliteRegistry::open(&db.0).unwrap();
    assert_eq!(registry.get_draft("review").unwrap(), next);
    assert_eq!(
        registry.get_by_digest(&published.digest).unwrap(),
        published
    );
    let deleted = registry.delete_draft("review", 2).unwrap();
    assert_eq!(deleted.revision, 3);
    assert!(deleted.deleted);
    assert_eq!(
        registry.get_draft("review").unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(registry.get_revision("review", 3).unwrap(), deleted);
    assert_eq!(
        registry.create_draft("review", &sample()).unwrap_err().code,
        ErrorCode::AlreadyExists
    );
    assert_eq!(registry.get_published("review", "1").unwrap(), published);
}
#[test]
fn conflict_and_invalid_batches_leave_content_and_history_unchanged() {
    let db = Database::new();
    let mut registry = db.registry();
    registry.create_draft("d", &sample()).unwrap();
    let current = registry.edit_draft("d", &edit(1, "2")).unwrap();
    for error in [
        registry.edit_draft("d", &edit(1, "3")).unwrap_err(),
        registry.delete_draft("d", 1).unwrap_err(),
        registry.publish("d", 1).unwrap_err(),
        registry.replace_draft("d", 1, &sample()).unwrap_err(),
    ] {
        assert_eq!(error.code, ErrorCode::RevisionConflict);
        assert_eq!(error.expected_revision, Some(1));
        assert_eq!(error.actual_revision, Some(2));
    }
    let bad = Patch {
        expected_revision: 2,
        operations: vec![
            Edit::SetVersion {
                version: "3".into(),
            },
            Edit::RemoveEdge {
                id: "missing".into(),
            },
        ],
    };
    assert_eq!(
        registry.edit_draft("d", &bad).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(registry.get_draft("d").unwrap(), current);
    assert_eq!(
        registry.get_revision("d", 3).unwrap_err().code,
        ErrorCode::NotFound
    );
    // A no-op preserves the revision but still performs the version check.
    assert_eq!(registry.edit_draft("d", &edit(2, "2")).unwrap(), current);
}
#[test]
fn incomplete_drafts_are_editable_and_failed_publication_has_no_side_effect() {
    let db = Database::new();
    let mut registry = db.registry();
    let mut w = sample();
    w.nodes.clear();
    let draft = registry.create_draft("d", &w).unwrap();
    assert!(!draft.diagnostics.is_empty());
    assert_eq!(
        registry.publish("d", 1).unwrap_err().code,
        ErrorCode::InvalidDefinition
    );
    assert_eq!(
        registry.get_published("review", "1").unwrap_err().code,
        ErrorCode::NotFound
    );
    registry.replace_draft("d", 1, &sample()).unwrap();
    registry.publish("d", 2).unwrap();
    let mut w = sample();
    w.nodes[0].kind = NodeKind::Terminal {
        outcome: TerminalOutcome::Failed,
    };
    registry.replace_draft("d", 2, &w).unwrap();
    assert_eq!(
        registry.publish("d", 3).unwrap_err().code,
        ErrorCode::PublicationConflict
    );
    assert_eq!(
        registry.get_published("review", "1").unwrap().workflow,
        sample()
    );
}
#[test]
fn pagination_is_bounded_and_tombstones_are_not_returned() {
    let db = Database::new();
    let mut registry = db.registry();
    for id in ["c", "a", "b"] {
        registry.create_draft(id, &sample()).unwrap();
    }
    let page = registry
        .list_drafts(&PageRequest {
            after: None,
            limit: 2,
        })
        .unwrap();
    assert_eq!(page.next_cursor.as_deref(), Some("b"));
    assert_eq!(page.items[0].draft_id.as_deref(), Some("a"));
    let page = registry
        .list_drafts(&PageRequest {
            after: page.next_cursor,
            limit: 2,
        })
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].draft_id.as_deref(), Some("c"));
    assert!(page.next_cursor.is_none());
    registry.delete_draft("a", 1).unwrap();
    assert_eq!(
        registry
            .list_drafts(&PageRequest {
                after: None,
                limit: 100
            })
            .unwrap()
            .items
            .len(),
        2
    );
    assert_eq!(
        registry
            .list_drafts(&PageRequest {
                after: None,
                limit: 0
            })
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    registry.publish("b", 1).unwrap();
    registry.edit_draft("b", &edit(1, "2")).unwrap();
    registry.publish("b", 2).unwrap();
    let page = registry
        .list_published(
            "review",
            &PageRequest {
                after: None,
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(page.items[0].version, "1");
    assert_eq!(page.next_cursor.as_deref(), Some("1"));
    assert_eq!(
        registry
            .list_published(
                "review",
                &PageRequest {
                    after: page.next_cursor,
                    limit: 1
                }
            )
            .unwrap()
            .items[0]
            .version,
        "2"
    );
}
#[test]
fn refuses_foreign_and_future_databases_without_schema_rewrite() {
    let foreign = Database::new();
    let connection = Connection::open(&foreign.0).unwrap();
    connection
        .execute_batch("CREATE TABLE sqliteXforeign (value TEXT);")
        .unwrap();
    assert!(matches!(
        SqliteRegistry::create(&foreign.0),
        Err(Error {
            code: ErrorCode::UnsupportedStorage,
            ..
        })
    ));
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let db = Database::new();
    let registry = db.registry();
    registry
        .connection
        .pragma_update(None, "user_version", 2)
        .unwrap();
    drop(registry);
    assert!(matches!(
        SqliteRegistry::create(&db.0),
        Err(Error {
            code: ErrorCode::UnsupportedStorage,
            ..
        })
    ));
    let connection = Connection::open(&db.0).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let absent = Database::new();
    assert!(SqliteRegistry::open_readonly(&absent.0).is_err());
    assert!(!absent.0.exists());
}
#[test]
fn sql_guards_and_digest_checks_protect_immutable_history_and_provenance() {
    let db = Database::new();
    let mut registry = db.registry();
    registry.create_draft("d", &sample()).unwrap();
    registry.publish("d", 1).unwrap();
    assert!(
        registry
            .connection
            .execute("UPDATE publications SET digest='bad'", [])
            .is_err()
    );
    assert!(
        registry
            .connection
            .execute("DELETE FROM draft_revisions", [])
            .is_err()
    );
    registry
        .connection
        .execute_batch(
            "DROP TRIGGER immutable_publications_update; UPDATE publications SET digest='bad';",
        )
        .unwrap();
    assert_eq!(
        registry.get_published("review", "1").unwrap_err().code,
        ErrorCode::CorruptStorage
    );
}
#[test]
fn committed_changes_survive_process_exit_and_uncommitted_changes_recover() {
    let db = Database::new();
    db.registry().create_draft("d", &sample()).unwrap();
    let before_crash = std::fs::read(&db.0).unwrap();
    let status = worker(&db, "crash").status().unwrap();
    assert_eq!(status.code(), Some(77));
    // Dirty pages reached the database; recovery must use the retained rollback journal.
    assert_ne!(std::fs::read(&db.0).unwrap(), before_crash);
    assert!(
        std::fs::metadata(format!("{}-journal", db.0.display()))
            .unwrap()
            .len()
            > 0
    );
    let registry = SqliteRegistry::open(&db.0).unwrap();
    assert_eq!(registry.get_draft("d").unwrap().revision, 1);
    assert_eq!(
        registry.get_revision("d", 2).unwrap_err().code,
        ErrorCode::NotFound
    );
    drop(registry);
    assert!(worker(&db, "2").status().unwrap().success());
    assert_eq!(
        SqliteRegistry::open_readonly(&db.0)
            .unwrap()
            .get_draft("d")
            .unwrap()
            .revision,
        2
    );
}
#[test]
fn two_processes_with_one_revision_produce_exactly_one_winner() {
    let db = Database::new();
    db.registry().create_draft("d", &sample()).unwrap();
    let mut a = worker(&db, "2").spawn().unwrap();
    let mut b = worker(&db, "3").spawn().unwrap();
    let mut statuses = vec![a.wait().unwrap().code(), b.wait().unwrap().code()];
    statuses.sort();
    assert_eq!(statuses, vec![Some(0), Some(3)]);
    let registry = SqliteRegistry::open_readonly(&db.0).unwrap();
    assert_eq!(registry.get_draft("d").unwrap().revision, 2);
    assert_eq!(registry.get_revision("d", 1).unwrap().workflow.version, "1");
}
fn worker(db: &Database, mode: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "tests::process_worker", "--nocapture"])
        .env("WORKFLOW_TEST_DB", &db.0)
        .env("WORKFLOW_TEST_MODE", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    cmd
}
#[test]
fn process_worker() {
    let Ok(path) = std::env::var("WORKFLOW_TEST_DB") else {
        return;
    };
    let mode = std::env::var("WORKFLOW_TEST_MODE").unwrap();
    let mut registry = SqliteRegistry::open(path).unwrap();
    if mode == "crash" {
        let tx = registry
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut draft = read_draft(&tx, "d", None).unwrap();
        draft.revision = 2;
        draft.workflow.version = "uncommitted".into();
        draft.digest = definition_digest(&draft.workflow).unwrap();
        write_revision(&tx, &draft).unwrap();
        tx.execute("UPDATE drafts SET revision=2 WHERE draft_id='d'", [])
            .unwrap();
        tx.cache_flush().unwrap();
        std::process::exit(77);
    }
    match registry.edit_draft("d", &edit(1, &mode)) {
        Ok(_) => {}
        Err(e) if e.code == ErrorCode::RevisionConflict => std::process::exit(3),
        Err(e) => panic!("{e:?}"),
    }
}
