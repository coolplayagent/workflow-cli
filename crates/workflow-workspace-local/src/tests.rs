use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use workflow_artifacts::{ArtifactStore, Producer, SourceRevision};
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[test]
fn sealed_parallel_repairs_require_explicit_conflict_resolution_and_create_a_new_verified_revision()
{
    let mut f = Fixture::new();
    f.spec.outputs = vec![
        OutputFile {
            path: "input.txt".into(),
            artifact_type: file_type(),
        },
        OutputFile {
            path: "new.txt".into(),
            artifact_type: file_type(),
        },
    ];
    let a = f.checkout();
    let first = f.store().path(&a.link()).unwrap();
    let mut second_spec = f.spec.clone();
    second_spec.producer.attempt_id = "attempt-2".into();
    let b = f.store().checkout(&second_spec, &f.source()).unwrap();
    let second = f.store().path(&b.link()).unwrap();
    fs::write(first.join("input.txt"), b"repair A\n").unwrap();
    fs::write(first.join("new.txt"), b"shared output\n").unwrap();
    fs::write(second.join("input.txt"), b"repair B\n").unwrap();
    let mut artifacts =
        workflow_artifact_local::LocalArtifactStore::create(f.dir.join("artifacts")).unwrap();
    let left = f
        .store()
        .seal_proposal(
            &a.link(),
            "Fix A with its independent file evidence",
            &mut artifacts,
        )
        .unwrap();
    let right = f
        .store()
        .seal_proposal(&b.link(), "Fix B with a conflicting edit", &mut artifacts)
        .unwrap();
    let mut proposals = vec![left.link(), right.link()];
    proposals.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
    let source = f.source().read(&f.spec.source_revision).unwrap();
    let unresolved = plan_merge(&source, &proposals, &Default::default(), &artifacts).unwrap();
    assert_eq!(unresolved.conflicts.len(), 1);
    assert_eq!(unresolved.conflicts[0].path, "input.txt");
    assert!(merged_files(&source, &unresolved, &artifacts).is_err());
    let resolutions =
        std::collections::BTreeMap::from([("input.txt".into(), right.artifact_id.clone())]);
    let plan = plan_merge(&source, &proposals, &resolutions, &artifacts).unwrap();
    assert!(plan.conflicts.is_empty());
    assert!(plan.requires_revalidation);
    // Work continuing in an old directory cannot change the retained proposal.
    fs::write(second.join("input.txt"), b"later unreviewed edit\n").unwrap();
    let files = merged_files(&source, &plan, &artifacts).unwrap();
    assert_eq!(
        files.iter().find(|f| f.path == "input.txt").unwrap().bytes,
        b"repair B\n"
    );
    assert_eq!(
        files.iter().find(|f| f.path == "new.txt").unwrap().bytes,
        b"shared output\n"
    );
    let destination = f.dir.join("merged.git");
    let revision = GitSource::write_merge(&destination, &plan, &files).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_ne!(revision.revision, f.revision);
    assert_eq!(revision.revision.len(), 64);
    let actual = GitSource::open(&revision.repository, &destination)
        .unwrap()
        .read(&revision)
        .unwrap();
    assert_eq!(
        actual
            .files
            .iter()
            .map(|f| (&f.path, &f.bytes, f.executable))
            .collect::<Vec<_>>(),
        files
            .iter()
            .map(|f| (&f.path, &f.bytes, f.executable))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        GitSource::write_merge(f.dir.join("retry.git"), &plan, &files).unwrap(),
        revision
    );
    assert!(GitSource::write_merge(&destination, &plan, &files).is_err());
    assert_eq!(
        fs::read(f.dir.join("repo/input.txt")).unwrap(),
        b"original\n"
    );
    assert_eq!(artifacts.lineage(&left.link()).unwrap().last(), Some(&left));
    let mut tampered = plan.clone();
    tampered.files[0].digest = content_digest(b"changed");
    assert!(merged_files(&source, &tampered, &artifacts).is_err());
    let mut invalid = std::collections::BTreeMap::new();
    invalid.insert("input.txt".into(), "not-a-proposal".into());
    assert!(plan_merge(&source, &proposals, &invalid, &artifacts).is_err());
}
#[test]
fn sealing_rejects_adapter_substitution_of_file_or_proposal_identity() {
    struct Substitute {
        inner: workflow_artifact_local::LocalArtifactStore,
        proposal: bool,
    }
    impl workflow_artifacts::ArtifactReader for Substitute {
        fn verify(
            &self,
            link: &workflow_artifacts::ArtifactLink,
        ) -> workflow_artifacts::Result<workflow_artifacts::ArtifactRef> {
            self.inner.verify(link)
        }
    }
    impl ArtifactStore for Substitute {
        fn publish(
            &mut self,
            spec: &workflow_artifacts::PublishSpec,
            bytes: &mut dyn std::io::Read,
        ) -> workflow_artifacts::Result<workflow_artifacts::ArtifactRef> {
            let mut result = self.inner.publish(spec, bytes)?;
            if (spec.artifact_type == proposal_type()) == self.proposal {
                result.digest = content_digest(b"different artifact");
            }
            Ok(result)
        }
        fn read(
            &self,
            link: &workflow_artifacts::ArtifactLink,
        ) -> workflow_artifacts::Result<Vec<u8>> {
            self.inner.read(link)
        }
    }
    for proposal in [false, true] {
        let mut f = Fixture::new();
        f.spec.outputs = vec![OutputFile {
            path: "input.txt".into(),
            artifact_type: file_type(),
        }];
        let reference = f.checkout();
        fs::write(
            f.store().path(&reference.link()).unwrap().join("input.txt"),
            b"changed\n",
        )
        .unwrap();
        let mut artifacts = Substitute {
            inner: workflow_artifact_local::LocalArtifactStore::create(f.dir.join("artifacts"))
                .unwrap(),
            proposal,
        };
        assert_eq!(
            f.store()
                .seal_proposal(&reference.link(), "reviewed change", &mut artifacts)
                .unwrap_err()
                .code,
            ErrorCode::CorruptStorage
        );
    }
}

#[test]
fn merge_proposals_record_deletions_and_reject_undeclared_changes() {
    let mut f = Fixture::new();
    f.spec.outputs = vec![OutputFile {
        path: "input.txt".into(),
        artifact_type: file_type(),
    }];
    let reference = f.checkout();
    let path = f.store().path(&reference.link()).unwrap();
    let mut artifacts =
        workflow_artifact_local::LocalArtifactStore::create(f.dir.join("artifacts")).unwrap();
    fs::write(path.join("undeclared.txt"), b"outside contract").unwrap();
    assert!(
        f.store()
            .seal_proposal(&reference.link(), "unscoped change", &mut artifacts)
            .is_err()
    );
    fs::remove_file(path.join("undeclared.txt")).unwrap();
    fs::remove_file(path.join("input.txt")).unwrap();
    let proposal = f
        .store()
        .seal_proposal(&reference.link(), "Remove obsolete input", &mut artifacts)
        .unwrap();
    let source = f.source().read(&f.spec.source_revision).unwrap();
    let plan = plan_merge(&source, &[proposal.link()], &Default::default(), &artifacts).unwrap();
    let files = merged_files(&source, &plan, &artifacts).unwrap();
    assert!(!files.iter().any(|f| f.path == "input.txt"));
    let revision = GitSource::write_merge(f.dir.join("merged.git"), &plan, &files).unwrap();
    assert!(
        !GitSource::open(&revision.repository, f.dir.join("merged.git"))
            .unwrap()
            .read(&revision)
            .unwrap()
            .files
            .iter()
            .any(|f| f.path == "input.txt")
    );
}
struct Fixture {
    dir: PathBuf,
    spec: CheckoutSpec,
    revision: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn git(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write;
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Workspace Test",
            "-c",
            "user.email=workspace@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if input.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn().unwrap();
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "workflow-workspace-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&dir).unwrap();
        let repo = dir.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"], None);
        fs::write(repo.join("input.txt"), b"original\n").unwrap();
        fs::write(repo.join(".gitignore"), b"ignored*\n").unwrap();
        fs::write(repo.join(".gitattributes"), b"input.txt filter=trap\n").unwrap();
        git(&repo, &["add", "."], None);
        git(&repo, &["commit", "-m", "source"], None);
        let revision = git(&repo, &["rev-parse", "HEAD"], None);
        let spec = CheckoutSpec {
            schema_version: 1,
            producer: Producer {
                run_id: "run".into(),
                node_instance_id: "instance-1".into(),
                attempt_id: "attempt-1".into(),
                request_digest: digest(&"request").unwrap(),
                input_digest: digest(&"input").unwrap(),
            },
            source_revision: SourceRevision {
                repository: "example/repo".into(),
                revision: revision.clone(),
            },
            merge_policy: MergePolicy::Explicit,
            inputs: vec![],
            outputs: vec![OutputFile {
                path: "report.txt".into(),
                artifact_type: file_type(),
            }],
        };
        LocalWorkspaceStore::create(dir.join("store")).unwrap();
        Self {
            dir,
            spec,
            revision,
        }
    }
    fn store(&self) -> LocalWorkspaceStore {
        LocalWorkspaceStore::open(self.dir.join("store")).unwrap()
    }
    fn source(&self) -> GitSource {
        GitSource::open("example/repo", self.dir.join("repo")).unwrap()
    }
    fn checkout(&self) -> WorkspaceRef {
        self.store().checkout(&self.spec, &self.source()).unwrap()
    }
}
#[test]
fn physical_attempt_isolation_and_exact_duplicate_preserve_edits_and_never_change_source() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    fs::write(f.dir.join("repo/input.txt"), b"source dirty\n").unwrap();
    let a = f.checkout();
    let first = f.store().path(&a.link()).unwrap();
    assert_eq!(fs::read(first.join("input.txt")).unwrap(), b"original\n");
    let mut spec = f.spec.clone();
    spec.producer.attempt_id = "attempt-2".into();
    let b = f.store().checkout(&spec, &f.source()).unwrap();
    let second = f.store().path(&b.link()).unwrap();
    assert_ne!(first, second);
    assert_ne!(
        fs::metadata(first.join("input.txt")).unwrap().ino(),
        fs::metadata(second.join("input.txt")).unwrap().ino()
    );
    fs::write(first.join("input.txt"), b"edited").unwrap();
    assert_eq!(f.checkout(), a);
    assert_eq!(fs::read(first.join("input.txt")).unwrap(), b"edited");
    assert_eq!(fs::read(second.join("input.txt")).unwrap(), b"original\n");
    assert_eq!(
        fs::read(f.dir.join("repo/input.txt")).unwrap(),
        b"source dirty\n"
    );
    assert!(!f.store().observe(&a.link()).unwrap().clean);
    assert!(f.store().observe(&b.link()).unwrap().clean);
    let mut changed = f.spec.clone();
    changed.outputs.clear();
    assert_eq!(
        f.store().checkout(&changed, &f.source()).unwrap_err().code,
        ErrorCode::Conflict
    );
    assert_eq!(f.store().cleanup_orphans().unwrap().removed_uncommitted, 0);
    assert!(first.exists());
}
#[test]
fn every_regular_file_and_executable_mode_is_observed_without_git_ignore_or_index_shortcuts() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let r = f.checkout();
    let path = f.store().path(&r.link()).unwrap();
    fs::write(path.join("ignored-output"), b"not ignored").unwrap();
    assert!(!f.store().observe(&r.link()).unwrap().clean);
    fs::remove_file(path.join("ignored-output")).unwrap();
    fs::set_permissions(path.join("input.txt"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!f.store().observe(&r.link()).unwrap().clean);
    fs::remove_file(path.join("input.txt")).unwrap();
    let o = f.store().observe(&r.link()).unwrap();
    assert_eq!(o.changes[0].kind, ChangeKind::Deleted);
}
#[test]
fn symlink_directory_file_hardlink_and_special_file_cannot_escape_observation() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let r = f.checkout();
    let path = f.store().path(&r.link()).unwrap();
    let outside = f.dir.join("outside");
    fs::write(&outside, b"outside").unwrap();
    symlink(&outside, path.join("link")).unwrap();
    assert!(f.store().observe(&r.link()).is_err());
    fs::remove_file(path.join("link")).unwrap();
    symlink(&f.dir, path.join("directory")).unwrap();
    assert!(f.store().observe(&r.link()).is_err());
    fs::remove_file(path.join("directory")).unwrap();
    fs::hard_link(&outside, path.join("hard")).unwrap();
    assert!(f.store().observe(&r.link()).is_err());
    fs::remove_file(path.join("hard")).unwrap();
    let fifo = std::ffi::CString::new(path.join("fifo").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(f.store().observe(&r.link()).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"outside");
}
#[test]
fn source_uses_verified_commit_objects_without_replacements_filters_or_current_worktree() {
    let f = Fixture::new();
    let repo = f.dir.join("repo");
    fs::write(repo.join("input.txt"), b"later\n").unwrap();
    git(&repo, &["add", "."], None);
    git(&repo, &["commit", "-m", "later"], None);
    let later = git(&repo, &["rev-parse", "HEAD"], None);
    git(&repo, &["replace", &f.revision, &later], None);
    let marker = f.dir.join("executed-hook");
    let hook = format!("touch {}", marker.display());
    git(&repo, &["config", "core.fsmonitor", &hook], None);
    git(&repo, &["config", "filter.trap.smudge", &hook], None);
    let r = f.checkout();
    let path = f.store().path(&r.link()).unwrap();
    assert_eq!(fs::read(path.join("input.txt")).unwrap(), b"original\n");
    assert!(!marker.exists());
    let mut tag = f.spec.clone();
    tag.source_revision.revision = git(
        &repo,
        &["rev-parse", &format!("{}^{{tree}}", f.revision)],
        None,
    );
    assert!(f.source().read(&tag.source_revision).is_err());
}
#[test]
fn corrupted_object_payload_is_not_accepted_under_an_original_commit() {
    let f = Fixture::new();
    let repo = f.dir.join("repo");
    let original = git(&repo, &["rev-parse", "HEAD:input.txt"], None);
    let other = git(
        &repo,
        &["hash-object", "-w", "--stdin"],
        Some(b"corrupted\n"),
    );
    let object = |id: &str| repo.join(".git/objects").join(&id[..2]).join(&id[2..]);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(object(&original), fs::Permissions::from_mode(0o600)).unwrap();
    fs::copy(object(&other), object(&original)).unwrap();
    assert!(f.store().checkout(&f.spec, &f.source()).is_err());
    assert!(f.store().find(&workspace_id(&f.spec).unwrap()).is_err());
}
#[test]
fn captures_exact_typed_outputs_and_dag_without_claiming_a_new_git_revision() {
    let f = Fixture::new();
    let r = f.checkout();
    let path = f.store().path(&r.link()).unwrap();
    fs::write(path.join("report.txt"), b"real output").unwrap();
    let mut artifacts =
        workflow_artifact_local::LocalArtifactStore::create(f.dir.join("artifacts")).unwrap();
    let captured = f.store().capture(&r.link(), &mut artifacts).unwrap();
    assert!(!captured.observation.clean);
    assert_eq!(captured.files.len(), 1);
    assert_eq!(
        artifacts.read(&captured.files[0].link()).unwrap(),
        b"real output"
    );
    assert_eq!(
        captured.manifest.manifest.spec.inputs,
        vec![captured.files[0].link()]
    );
    assert_eq!(
        captured.manifest.manifest.spec.source_revision,
        f.spec.source_revision
    );
    assert_eq!(captured.manifest.manifest.spec.producer, f.spec.producer);
    let payload: OutputManifest =
        parse_message(&artifacts.read(&captured.manifest.link()).unwrap()).unwrap();
    assert_eq!(
        payload.observed_tree_digest,
        captured.observation.tree_digest
    );
    assert_eq!(payload.baseline_tree_digest, r.manifest.tree_digest);
    assert_eq!(payload.files[0].path, "report.txt");
    let again = f.store().capture(&r.link(), &mut artifacts).unwrap();
    assert_eq!(again, captured);
    fs::write(path.join("report.txt"), b"later output").unwrap();
    assert_eq!(
        artifacts.read(&captured.files[0].link()).unwrap(),
        b"real output"
    );
    assert_ne!(
        f.store().observe(&r.link()).unwrap().tree_digest,
        payload.observed_tree_digest
    );
}
#[test]
fn missing_output_wrong_type_and_mutation_before_manifest_never_confirm_capture() {
    let f = Fixture::new();
    let r = f.checkout();
    let path = f.store().path(&r.link()).unwrap();
    let mut artifacts =
        workflow_artifact_local::LocalArtifactStore::create(f.dir.join("artifacts")).unwrap();
    assert!(f.store().capture(&r.link(), &mut artifacts).is_err());
    fs::write(path.join("report.txt"), b"output").unwrap();
    let error = f
        .store()
        .capture_internal(&r.link(), &mut artifacts, |phase| {
            if phase == "outputs_published" {
                fs::write(path.join("input.txt"), b"changed during capture").unwrap()
            }
        })
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Changed);
    let mut spec = f.spec.clone();
    spec.producer.attempt_id = "typed".into();
    spec.outputs[0].artifact_type.content = workflow_artifacts::ContentSchema::Utf8;
    let typed = f.store().checkout(&spec, &f.source()).unwrap();
    let typed_path = f.store().path(&typed.link()).unwrap();
    fs::write(typed_path.join("report.txt"), [255, 255]).unwrap();
    assert!(f.store().capture(&typed.link(), &mut artifacts).is_err());
}

#[test]
fn rejects_symlink_and_submodule_sources_without_creating_a_published_allocation() {
    use std::os::unix::fs::symlink;
    for submodule in [false, true] {
        let f = Fixture::new();
        let repo = f.dir.join("repo");
        if submodule {
            git(
                &repo,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{},child", f.revision),
                ],
                None,
            );
        } else {
            symlink("../../escape", repo.join("link")).unwrap();
            git(&repo, &["add", "link"], None);
        }
        git(&repo, &["commit", "-m", "unsupported source"], None);
        let mut spec = f.spec.clone();
        spec.source_revision.revision = git(&repo, &["rev-parse", "HEAD"], None);
        assert_eq!(
            f.store().checkout(&spec, &f.source()).unwrap_err().code,
            ErrorCode::UnsupportedSource
        );
        assert!(f.store().find(&workspace_id(&spec).unwrap()).is_err());
        assert!(
            fs::read_dir(f.dir.join("store/workspaces"))
                .unwrap()
                .next()
                .is_none()
        );
    }
}
#[test]
fn sha256_git_repository_requires_its_full_revision_and_preserves_executable_files() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let repo = f.dir.join("sha256");
    fs::create_dir(&repo).unwrap();
    git(
        &repo,
        &["init", "--object-format=sha256", "--initial-branch=main"],
        None,
    );
    fs::write(repo.join("script"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(repo.join("script"), fs::Permissions::from_mode(0o700)).unwrap();
    git(&repo, &["add", "."], None);
    git(&repo, &["commit", "-m", "sha256 source"], None);
    let mut spec = f.spec.clone();
    spec.source_revision.revision = git(&repo, &["rev-parse", "HEAD"], None);
    assert_eq!(spec.source_revision.revision.len(), 64);
    let source = GitSource::open("example/repo", &repo).unwrap();
    let r = f.store().checkout(&spec, &source).unwrap();
    assert!(r.manifest.baseline[0].executable);
    assert!(f.store().observe(&r.link()).unwrap().clean);
    spec.source_revision.revision.truncate(40);
    assert!(source.read(&spec.source_revision).is_err());
}
#[test]
fn foreign_symlinked_or_corrupt_stores_are_not_reinitialized_or_reported_healthy() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let foreign = f.dir.join("foreign");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("keep"), b"retained").unwrap();
    assert!(LocalWorkspaceStore::create(&foreign).is_err());
    assert_eq!(fs::read(foreign.join("keep")).unwrap(), b"retained");
    symlink(f.dir.join("store"), f.dir.join("link")).unwrap();
    assert!(LocalWorkspaceStore::open(f.dir.join("link")).is_err());
    let _ = f.checkout();
    let c = rusqlite::Connection::open(f.dir.join("store/catalog.sqlite")).unwrap();
    c.execute_batch("DROP TRIGGER immutable_workspace_delete; DELETE FROM workspaces;")
        .unwrap();
    assert!(LocalWorkspaceStore::open(f.dir.join("store")).is_err());
}
#[test]
fn workspace_process_worker() {
    let Ok(root) = std::env::var("WORKFLOW_WORKSPACE_PROCESS") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut spec: CheckoutSpec = parse_message(&fs::read(root.join("spec.json")).unwrap()).unwrap();
    if let Ok(attempt) = std::env::var("WORKFLOW_WORKSPACE_ATTEMPT") {
        spec.producer.attempt_id = attempt;
    }
    let mut store = LocalWorkspaceStore::open(root.join("store")).unwrap();
    if let Ok(ready) = std::env::var("WORKFLOW_WORKSPACE_READY") {
        fs::write(root.join(ready), b"ready").unwrap();
        let start = std::time::Instant::now();
        while !root.join("go").exists() {
            assert!(start.elapsed() < std::time::Duration::from_secs(20));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let source = GitSource::open(&spec.source_revision.repository, root.join("repo")).unwrap();
    let result = store
        .checkout_internal(&spec, &source, |phase| {
            if std::env::var("WORKFLOW_WORKSPACE_KILL").ok().as_deref() == Some(phase) {
                // SAFETY: the crash fixture terminates only its own test process.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
            }
        })
        .unwrap();
    if let Ok(output) = std::env::var("WORKFLOW_WORKSPACE_RESULT") {
        fs::write(root.join(output), to_message(&result).unwrap()).unwrap();
    }
}
fn child(f: &Fixture) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "tests::workspace_process_worker", "--nocapture"])
        .env("WORKFLOW_WORKSPACE_PROCESS", &f.dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    c
}
#[test]
fn process_termination_never_publishes_a_partial_tree_and_cleanup_retains_committed_edits() {
    for phase in [
        "file_written",
        "tree_synced",
        "tree_published",
        "manifest_written",
        "after_commit",
    ] {
        let f = Fixture::new();
        fs::write(f.dir.join("spec.json"), to_message(&f.spec).unwrap()).unwrap();
        let status = child(&f)
            .env("WORKFLOW_WORKSPACE_KILL", phase)
            .status()
            .unwrap();
        assert!(!status.success(), "{phase}");
        let result = f.store().find(&workspace_id(&f.spec).unwrap());
        assert_eq!(result.is_ok(), phase == "after_commit", "{phase}");
        let mut store = f.store();
        let cleaned = store.cleanup_orphans().unwrap();
        if phase != "after_commit" {
            assert_eq!(
                cleaned.removed_staging + cleaned.removed_uncommitted,
                1,
                "{phase}"
            );
        } else {
            assert_eq!(cleaned.removed_staging + cleaned.removed_uncommitted, 0);
        }
        let r = store.checkout(&f.spec, &f.source()).unwrap();
        assert!(store.observe(&r.link()).unwrap().clean);
        let path = store.path(&r.link()).unwrap();
        fs::write(path.join("input.txt"), b"retained changes").unwrap();
        store.cleanup_orphans().unwrap();
        assert_eq!(
            fs::read(path.join("input.txt")).unwrap(),
            b"retained changes"
        );
    }
}
#[test]
fn independent_processes_serialize_duplicate_allocation_and_isolate_distinct_attempts() {
    for distinct in [false, true] {
        let f = Fixture::new();
        fs::write(f.dir.join("spec.json"), to_message(&f.spec).unwrap()).unwrap();
        let mut a = child(&f)
            .env("WORKFLOW_WORKSPACE_READY", "ready-a")
            .env("WORKFLOW_WORKSPACE_RESULT", "result-a")
            .spawn()
            .unwrap();
        let mut b = child(&f)
            .env("WORKFLOW_WORKSPACE_READY", "ready-b")
            .env("WORKFLOW_WORKSPACE_RESULT", "result-b")
            .env(
                "WORKFLOW_WORKSPACE_ATTEMPT",
                if distinct { "attempt-2" } else { "attempt-1" },
            )
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        while !f.dir.join("ready-a").exists() || !f.dir.join("ready-b").exists() {
            assert!(start.elapsed() < std::time::Duration::from_secs(20));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        fs::write(f.dir.join("go"), b"go").unwrap();
        assert!(a.wait().unwrap().success());
        assert!(b.wait().unwrap().success());
        let left: WorkspaceRef = parse_message(&fs::read(f.dir.join("result-a")).unwrap()).unwrap();
        let right: WorkspaceRef =
            parse_message(&fs::read(f.dir.join("result-b")).unwrap()).unwrap();
        assert_eq!(left == right, !distinct);
        assert!(f.store().observe(&left.link()).unwrap().clean);
        assert!(f.store().observe(&right.link()).unwrap().clean);
        let c = rusqlite::Connection::open(f.dir.join("store/catalog.sqlite")).unwrap();
        let count: i64 = c
            .query_row("SELECT count(*) FROM workspaces", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, if distinct { 2 } else { 1 });
    }
}

#[test]
fn declared_input_artifacts_remain_in_output_lineage_and_corruption_blocks_capture() {
    let f = Fixture::new();
    let mut artifacts =
        workflow_artifact_local::LocalArtifactStore::create(f.dir.join("artifacts")).unwrap();
    let input = artifacts
        .publish(
            &publish_spec(&f.spec, file_type()),
            &mut b"upstream input".as_slice(),
        )
        .unwrap();
    let mut spec = f.spec.clone();
    spec.inputs = vec![input.link()];
    let r = f.store().checkout(&spec, &f.source()).unwrap();
    let path = f.store().path(&r.link()).unwrap();
    fs::write(path.join("report.txt"), b"output").unwrap();
    let captured = f.store().capture(&r.link(), &mut artifacts).unwrap();
    assert_eq!(captured.files[0].manifest.spec.inputs, vec![input.link()]);
    assert!(
        captured
            .manifest
            .manifest
            .spec
            .inputs
            .contains(&input.link())
    );
    assert_eq!(
        artifacts.lineage(&captured.manifest.link()).unwrap().len(),
        3
    );
    fs::write(
        f.dir
            .join("artifacts/objects")
            .join(&input.manifest.content_digest[7..]),
        b"corrupt",
    )
    .unwrap();
    assert!(f.store().capture(&r.link(), &mut artifacts).is_err());
}
#[test]
fn full_catalog_and_missing_revision_never_confirm_an_allocation() {
    let f = Fixture::new();
    let mut store = f.store();
    let pages: i64 = store
        .connection
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    store
        .connection
        .pragma_update(None, "max_page_count", pages)
        .unwrap();
    let mut large = f.spec.clone();
    large.outputs = (0..64)
        .map(|i| OutputFile {
            path: format!(
                "out-{i}/{}/{}/{}/report",
                "x".repeat(200),
                "y".repeat(200),
                "z".repeat(200)
            ),
            artifact_type: file_type(),
        })
        .collect();
    assert_eq!(
        store.checkout(&large, &f.source()).unwrap_err().code,
        ErrorCode::Storage
    );
    drop(store);
    assert!(f.store().find(&workspace_id(&large).unwrap()).is_err());
    assert_eq!(f.store().cleanup_orphans().unwrap().removed_uncommitted, 1);
    let mut missing = f.spec.clone();
    missing.source_revision.revision = "0".repeat(40);
    assert!(f.store().checkout(&missing, &f.source()).is_err());
    assert!(f.store().find(&workspace_id(&missing).unwrap()).is_err());
    assert!(f.store().observe(&f.checkout().link()).unwrap().clean);
}

#[test]
fn catalog_read_keeps_one_snapshot_while_another_connection_commits() {
    let f = Fixture::new();
    let first = f.checkout();
    let reader = f.store();
    // WAL lets the independent writer commit while the reader holds its old snapshot.
    reader
        .connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    let mut writer = f.store();
    let mut next = f.spec.clone();
    next.producer.attempt_id = "concurrent-writer".into();
    let old = crate::store::catalog_internal(&reader.connection, || {
        writer.checkout(&next, &f.source()).unwrap();
    })
    .unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(old[&first.workspace_id], first);
    let later = reader.find(&workspace_id(&next).unwrap()).unwrap();
    assert!(reader.observe(&later.link()).unwrap().clean);
}
