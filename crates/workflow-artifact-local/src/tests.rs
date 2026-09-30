use super::*;
use serde_json::json;
use std::{
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "workflow-artifacts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn store(&self) -> LocalArtifactStore {
        LocalArtifactStore::create(&self.0).unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn spec() -> PublishSpec {
    parse_message(&serde_json::to_vec(&json!({"schema_version":1,"artifact_type":{"identity":{"id":"test-report","version":"1.0.0"},"content":{"format":"json","value_type":{"type":"boolean"}}},"producer":{"run_id":"run","node_instance_id":"instance-1","attempt_id":"attempt-1","request_digest":content_digest(b"request"),"input_digest":content_digest(b"input")},"source_revision":{"repository":"example","revision":"a".repeat(40)},"inputs":[],"access":{"type":"run","run_id":"run"},"retention":"run_dependency"})).unwrap()).unwrap()
}
fn put(s: &mut LocalArtifactStore, p: &PublishSpec, b: &[u8]) -> ArtifactRef {
    s.publish(p, &mut &*b).unwrap()
}
#[test]
fn content_is_immutable_deduplicated_typed_and_portable_with_lineage() {
    let d = Dir::new();
    let mut store = d.store();
    let p = spec();
    let first = put(&mut store, &p, b"true");
    assert_eq!(put(&mut store, &p, b"true"), first);
    let mut next = p.clone();
    next.producer.attempt_id = "attempt-2".into();
    next.inputs = vec![first.link()];
    let second = put(&mut store, &next, b"true");
    assert_ne!(first.artifact_id, second.artifact_id);
    assert_eq!(std::fs::read_dir(d.0.join("objects")).unwrap().count(), 1);
    assert_eq!(store.impact(&first.link()).unwrap(), vec![second.clone()]);
    assert!(store.impact(&second.link()).unwrap().is_empty());
    assert_eq!(
        store.lineage(&second.link()).unwrap(),
        vec![first.clone(), second.clone()]
    );
    let target = Dir::new();
    let mut imported = target.store();
    for artifact in store.lineage(&second.link()).unwrap() {
        let bytes = store.read(&artifact.link()).unwrap();
        assert_eq!(
            put(&mut imported, &artifact.manifest.spec, &bytes),
            artifact
        );
    }
    assert_eq!(imported.verify(&second.link()).unwrap(), second);
    let mut wrong = p.artifact_type.clone();
    wrong.identity.version = "2.0.0".into();
    assert_eq!(
        verify_expected(&store, &first.link(), &wrong)
            .unwrap_err()
            .code,
        ErrorCode::TypeConflict
    );
    let before = catalog::read(&store.connection).unwrap().len();
    assert!(store.publish(&p, &mut &b"123"[..]).is_err());
    assert_eq!(catalog::read(&store.connection).unwrap().len(), before);
}
#[test]
fn producer_scope_parent_and_type_version_conflicts_preserve_the_catalog() {
    let d = Dir::new();
    let mut store = d.store();
    let p = spec();
    let first = put(&mut store, &p, b"true");
    let mut changed = p.clone();
    changed.artifact_type.content = ContentSchema::Utf8;
    assert_eq!(
        store.publish(&changed, &mut &b"text"[..]).unwrap_err().code,
        ErrorCode::TypeConflict
    );
    let mut child = p.clone();
    child.producer.run_id = "other".into();
    child.access = AccessScope::Run {
        run_id: "other".into(),
    };
    child.inputs = vec![first.link()];
    assert_eq!(
        store.publish(&child, &mut &b"true"[..]).unwrap_err().code,
        ErrorCode::ScopeMismatch
    );
    let mut missing = p;
    missing.inputs = vec![ArtifactLink {
        artifact_id: format!("artifact-{}", "0".repeat(64)),
        digest: format!("sha256:{}", "0".repeat(64)),
    }];
    assert_eq!(
        store.publish(&missing, &mut &b"true"[..]).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(catalog::read(&store.connection).unwrap().len(), 1);
    assert_eq!(store.verify(&first.link()).unwrap(), first);
}
#[test]
fn missing_changed_and_symlink_content_are_never_verified_or_cleaned_as_healthy() {
    for mode in ["missing", "changed", "symlink"] {
        let d = Dir::new();
        let mut store = d.store();
        let r = put(&mut store, &spec(), b"true");
        let path = files::object(&d.0, &r);
        std::fs::remove_file(&path).unwrap();
        if mode == "changed" {
            std::fs::write(&path, b"false").unwrap();
        }
        #[cfg(unix)]
        if mode == "symlink" {
            let target = d.0.join("outside");
            std::fs::write(&target, b"true").unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
        }
        assert!(store.verify(&r.link()).is_err());
        assert!(store.cleanup_orphans().is_err());
    }
}
#[test]
fn corruption_and_foreign_storage_cannot_turn_committed_artifacts_into_orphans() {
    let missing = Dir::new();
    assert!(LocalArtifactStore::open(&missing.0).is_err());
    assert!(!missing.0.exists());
    let foreign = Dir::new();
    std::fs::create_dir(&foreign.0).unwrap();
    std::fs::write(foreign.0.join("unrelated"), b"retain").unwrap();
    assert!(LocalArtifactStore::create(&foreign.0).is_err());
    let d = Dir::new();
    let mut store = d.store();
    let r = put(&mut store, &spec(), b"true");
    assert!(
        store
            .connection
            .execute("DELETE FROM artifacts", [])
            .is_err()
    );
    store
        .connection
        .execute_batch("DROP TRIGGER artifacts_no_delete; DELETE FROM artifacts;")
        .unwrap();
    assert_eq!(
        store.cleanup_orphans().unwrap_err().code,
        ErrorCode::CorruptStorage
    );
    assert!(files::object(&d.0, &r).exists());
    assert!(store.resolve("../../outside").is_err());
}
#[test]
fn full_and_unwritable_catalogs_never_confirm_publication() {
    for full in [false, true] {
        let d = Dir::new();
        let mut store = d.store();
        let mut p = spec();
        let fields: serde_json::Map<_, _> = (0..128)
            .map(|i| {
                (
                    format!("field{i}{}", "x".repeat(80)),
                    json!({"type":"string"}),
                )
            })
            .collect();
        p.artifact_type.content = parse_message(
            &serde_json::to_vec(
                &json!({"format":"json","value_type":{"type":"object","fields":fields}}),
            )
            .unwrap(),
        )
        .unwrap();
        let payload: serde_json::Map<_, _> =
            fields.keys().map(|k| (k.clone(), json!(""))).collect();
        let payload = serde_json::to_vec(&payload).unwrap();
        if full {
            let count: i64 = store
                .connection
                .pragma_query_value(None, "page_count", |r| r.get(0))
                .unwrap();
            store
                .connection
                .pragma_update(None, "max_page_count", count)
                .unwrap();
        } else {
            store
                .connection
                .execute_batch("PRAGMA query_only=ON")
                .unwrap();
        }
        assert!(store.publish(&p, &mut payload.as_slice()).is_err());
        assert!(catalog::read(&store.connection).unwrap().is_empty());
        if full {
            store
                .connection
                .pragma_update(None, "max_page_count", 100000)
                .unwrap();
        } else {
            store
                .connection
                .execute_batch("PRAGMA query_only=OFF")
                .unwrap();
        }
        store.cleanup_orphans().unwrap();
        assert_eq!(std::fs::read_dir(d.0.join("objects")).unwrap().count(), 0);
    }
}
fn child(d: &Dir, slot: &str, phase: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::process_worker", "--nocapture"])
        .env("WORKFLOW_ARTIFACT_PROCESS_ROOT", &d.0)
        .env("WORKFLOW_ARTIFACT_SLOT", slot)
        .env("WORKFLOW_ARTIFACT_PHASE", phase)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
fn wait(d: &Dir, name: &str, c: &mut std::process::Child) {
    let until = Instant::now() + Duration::from_secs(20);
    while !d.0.join(name).exists() {
        assert!(
            c.try_wait().unwrap().is_none(),
            "child exited before {name}"
        );
        assert!(Instant::now() < until, "worker marker timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn process_worker() {
    let Ok(root) = std::env::var("WORKFLOW_ARTIFACT_PROCESS_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let slot = std::env::var("WORKFLOW_ARTIFACT_SLOT").unwrap();
    let phase = std::env::var("WORKFLOW_ARTIFACT_PHASE").unwrap();
    let mut store = LocalArtifactStore::open(&root).unwrap();
    if phase == "race" {
        std::fs::write(root.join(format!("ready-{slot}")), b"ready").unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        while !root.join("go").exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let r = store
        .publish_internal(&spec(), &mut &b"true"[..], |at| {
            if at == phase {
                std::fs::write(root.join(format!("ready-{slot}")), b"ready").unwrap();
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        })
        .unwrap();
    std::fs::write(root.join(format!("result-{slot}")), to_message(&r).unwrap()).unwrap();
}
#[test]
fn concurrent_processes_deduplicate_manifest_and_do_not_overwrite() {
    let d = Dir::new();
    drop(d.store());
    let mut one = child(&d, "one", "race");
    let mut two = child(&d, "two", "race");
    wait(&d, "ready-one", &mut one);
    wait(&d, "ready-two", &mut two);
    std::fs::write(d.0.join("go"), b"go").unwrap();
    assert!(one.wait().unwrap().success());
    assert!(two.wait().unwrap().success());
    assert_eq!(
        std::fs::read(d.0.join("result-one")).unwrap(),
        std::fs::read(d.0.join("result-two")).unwrap()
    );
    let store = LocalArtifactStore::open(&d.0).unwrap();
    assert_eq!(catalog::read(&store.connection).unwrap().len(), 1);
    assert_eq!(
        store
            .read(&reference(&spec(), b"true").unwrap().link())
            .unwrap(),
        b"true"
    );
}
#[test]
fn killed_publishers_leave_an_orphan_or_a_durable_complete_manifest() {
    for phase in [
        "upload_half",
        "upload_synced",
        "object_published",
        "manifest_written",
        "after_commit",
    ] {
        let d = Dir::new();
        drop(d.store());
        let mut worker = child(&d, "one", phase);
        wait(&d, "ready-one", &mut worker);
        let mut store = LocalArtifactStore::open(&d.0).unwrap();
        store
            .connection
            .busy_timeout(Duration::from_millis(20))
            .unwrap();
        if phase != "after_commit" {
            assert_eq!(store.cleanup_orphans().unwrap_err().code, ErrorCode::Busy);
        }
        worker.kill().unwrap();
        assert!(!worker.wait().unwrap().success());
        drop(store);
        let mut store = LocalArtifactStore::open(&d.0).unwrap();
        let r = reference(&spec(), b"true").unwrap();
        if phase == "after_commit" {
            assert_eq!(store.verify(&r.link()).unwrap(), r);
        } else {
            assert_eq!(
                store.verify(&r.link()).unwrap_err().code,
                ErrorCode::NotFound
            );
        }
        store.cleanup_orphans().unwrap();
        assert_eq!(std::fs::read_dir(d.0.join("uploads")).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(d.0.join("objects")).unwrap().count(),
            usize::from(phase == "after_commit")
        );
        assert_eq!(put(&mut store, &spec(), b"true"), r);
    }
}

#[test]
fn replacing_an_input_identifies_transitive_producers_without_rewriting_history() {
    let d = Dir::new();
    let mut store = d.store();
    let p = spec();
    let input = put(&mut store, &p, b"true");
    let mut derived = p.clone();
    derived.producer.node_instance_id = "instance-2".into();
    derived.producer.attempt_id = "attempt-2".into();
    derived.inputs = vec![input.link()];
    let report = put(&mut store, &derived, b"false");
    derived.producer.node_instance_id = "instance-3".into();
    derived.producer.attempt_id = "attempt-3".into();
    derived.inputs = vec![input.link(), report.link()];
    let final_report = put(&mut store, &derived, b"true");
    let mut replacement = p;
    replacement.source_revision.revision = "b".repeat(40);
    replacement.producer.input_digest = content_digest(b"changed input");
    let replacement = put(&mut store, &replacement, b"false");
    assert_ne!(replacement.link(), input.link());
    let impacted: BTreeSet<_> = store
        .impact(&input.link())
        .unwrap()
        .into_iter()
        .map(|r| r.manifest.spec.producer.node_instance_id)
        .collect();
    assert_eq!(
        impacted,
        BTreeSet::from(["instance-2".into(), "instance-3".into()])
    );
    assert!(store.impact(&replacement.link()).unwrap().is_empty());
    assert_eq!(
        store.lineage(&final_report.link()).unwrap(),
        vec![input.clone(), report, final_report]
    );
    assert_eq!(store.verify(&input.link()).unwrap(), input);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&d.0).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            std::fs::metadata(files::object(&d.0, &input))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }
}

#[test]
fn durable_revalidation_view_blocks_old_and_new_stale_descendants_but_preserves_history() {
    let d = Dir::new();
    let mut store = d.store();
    let original = put(&mut store, &spec(), b"true");
    let mut dependent_spec = spec();
    dependent_spec.producer.node_instance_id = "test".into();
    dependent_spec.inputs = vec![original.link()];
    let old_report = put(&mut store, &dependent_spec, b"true");
    let mut new_spec = spec();
    new_spec.source_revision.revision = "b".repeat(40);
    new_spec.producer.attempt_id = "replacement".into();
    let replacement = put(&mut store, &new_spec, b"false");
    let plan = plan_revalidation(
        &store,
        &[Replacement {
            old: original.link(),
            new: replacement.link(),
        }],
        "Requirement changed; regenerate dependent test evidence",
    )
    .unwrap();
    assert_eq!(
        plan.affected
            .iter()
            .map(|a| a.artifact.artifact_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([original.artifact_id.clone(), old_report.artifact_id.clone()])
    );
    let view = InvalidatedReader::new(Box::new(d.store()), &plan).unwrap();
    assert!(view.verify(&old_report.link()).is_err());
    assert_eq!(view.verify(&replacement.link()).unwrap(), replacement);
    // A report published after the projection still cannot launder the stale input.
    dependent_spec.producer.attempt_id = "late-report".into();
    dependent_spec.inputs = vec![old_report.link()];
    let late = put(&mut store, &dependent_spec, b"true");
    assert!(view.verify(&late.link()).is_err());
    dependent_spec.inputs = vec![replacement.link()];
    dependent_spec.producer.attempt_id = "fresh-report".into();
    dependent_spec.source_revision.revision = "b".repeat(40);
    let fresh = put(&mut store, &dependent_spec, b"true");
    assert_eq!(view.verify(&fresh.link()).unwrap(), fresh);
    assert_eq!(store.verify(&old_report.link()).unwrap(), old_report);
    assert!(
        plan_revalidation(
            &store,
            &[Replacement {
                old: original.link(),
                new: late.link()
            }],
            "cannot replace with stale output"
        )
        .is_err()
    );
    assert!(
        plan_revalidation(
            &store,
            &[Replacement {
                old: original.link(),
                new: original.link()
            }],
            "not a change"
        )
        .is_err()
    );
}
