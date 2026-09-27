use super::*;
use workflow_artifacts::{Producer, SourceRevision};
fn spec() -> CheckoutSpec {
    CheckoutSpec {
        schema_version: 1,
        producer: Producer {
            run_id: "run".into(),
            node_instance_id: "instance-1".into(),
            attempt_id: "attempt-1".into(),
            request_digest: digest(&"request").unwrap(),
            input_digest: digest(&"input").unwrap(),
        },
        source_revision: SourceRevision {
            repository: "example".into(),
            revision: "a".repeat(40),
        },
        merge_policy: MergePolicy::Explicit,
        inputs: vec![],
        outputs: vec![OutputFile {
            path: "out/report.json".into(),
            artifact_type: file_type(),
        }],
    }
}
fn file(path: &str, bytes: &[u8]) -> FileEntry {
    FileEntry {
        path: path.into(),
        digest: content_digest(bytes),
        bytes: bytes.len() as u64,
        executable: false,
    }
}
fn reference_for(s: CheckoutSpec) -> WorkspaceRef {
    let baseline = vec![file("in.txt", b"source")];
    reference(WorkspaceManifest {
        spec: s,
        git_tree: "b".repeat(40),
        tree_digest: digest(&baseline).unwrap(),
        baseline,
        environment: Environment {
            os: "linux".into(),
            architecture: "x86_64".into(),
            git_version: "git version fixture".into(),
        },
    })
    .unwrap()
}
#[test]
fn attempt_identity_is_stable_but_changed_source_inputs_and_outputs_change_the_binding() {
    let s = spec();
    let original = reference_for(s.clone());
    for n in 0..4 {
        let mut changed = s.clone();
        match n {
            0 => changed.producer.input_digest = digest(&"other").unwrap(),
            1 => changed.source_revision.revision = "c".repeat(40),
            2 => changed.outputs.clear(),
            _ => changed.producer.request_digest = digest(&"replacement").unwrap(),
        }
        let r = reference_for(changed);
        assert_eq!(r.workspace_id, original.workspace_id);
        assert_ne!(r.digest, original.digest);
    }
    let mut next = s;
    next.producer.attempt_id = "attempt-2".into();
    assert_ne!(workspace_id(&next).unwrap(), original.workspace_id);
    let bytes = to_message(&original).unwrap();
    let restored: WorkspaceRef = parse_message(&bytes).unwrap();
    assert_eq!(restored, original);
    validate_ref(&restored).unwrap();
    let mut forged = original;
    forged.manifest.baseline[0].digest = content_digest(b"forged");
    assert!(validate_ref(&forged).is_err());
}
#[test]
fn observation_reports_added_deleted_modified_and_mode_changes_including_undeclared_paths() {
    let r = reference_for(spec());
    let clean = observation(&r, r.manifest.baseline.clone()).unwrap();
    assert!(clean.clean);
    let current = vec![
        file("in.txt", b"changed"),
        file("out/report.json", b"report"),
        file("unexpected", b"untracked"),
    ];
    let o = observation(&r, current).unwrap();
    assert!(!o.clean);
    assert_eq!(o.changes.len(), 3);
    assert_eq!(o.changes[0].kind, ChangeKind::Modified);
    assert!(!o.changes[0].declared_output);
    assert!(o.changes[1].declared_output);
    assert!(!o.changes[2].declared_output);
    let mut mode = r.manifest.baseline.clone();
    mode[0].executable = true;
    assert!(!observation(&r, mode).unwrap().clean);
    assert_eq!(
        observation(&r, vec![]).unwrap().changes[0].kind,
        ChangeKind::Deleted
    );
}
#[test]
fn paths_collisions_types_and_budgets_fail_before_materialization() {
    for p in [
        "",
        "/etc/passwd",
        "../escape",
        "a/../x",
        "a\\b",
        "a/.git/config",
        "A/.GIT/config",
        "a//b",
        "a\nname",
        "a:NUL",
        "NUL.txt",
        "COM1",
        "ending.",
    ] {
        assert!(validate_path(p).is_err(), "{p}");
    }
    validate_path("目录/source file.rs").unwrap();
    for paths in [
        vec!["a", "a/b"],
        vec!["A/b", "a"],
        vec!["A", "a"],
        vec!["z", "a"],
    ] {
        assert!(
            validate_files(&paths.into_iter().map(|p| file(p, b"x")).collect::<Vec<_>>()).is_err()
        );
    }
    let mut s = spec();
    s.outputs.push(s.outputs[0].clone());
    assert!(validate_spec(&s).is_err());
    let mut large = file("large", b"");
    large.bytes = MAX_FILE_BYTES + 1;
    assert!(validate_files(&[large]).is_err());
    let files = (0..5)
        .map(|i| FileEntry {
            path: format!("{i}"),
            bytes: MAX_FILE_BYTES,
            ..file("", b"")
        })
        .collect::<Vec<_>>();
    assert_eq!(validate_files(&files).unwrap_err().code, ErrorCode::Budget);
    assert!(parse_message::<CheckoutSpec>(br#"{"schema_version":1,"schema_version":1}"#).is_err());
}
