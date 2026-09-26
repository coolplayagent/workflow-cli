use super::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "workflow-cli-registry-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture(path: &str) -> PathBuf {
    if let Ok(root) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(root)
            .join(std::env::var("TEST_WORKSPACE").unwrap())
            .join(path)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}
fn call(args: &[&str], expected: i32) -> Value {
    let mut out = vec![];
    let mut err = vec![];
    let code = crate::run(args.iter().map(|s| s.to_string()), &mut out, &mut err);
    assert_eq!(
        code,
        expected,
        "{} {}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    assert!(err.is_empty());
    serde_json::from_slice(&out).unwrap()
}
#[test]
fn cli_authoring_conflict_publication_and_query_cycle() {
    let sandbox = Sandbox::new();
    let db = sandbox.path("definitions.sqlite");
    let file = fixture("examples/review.yaml")
        .to_string_lossy()
        .into_owned();
    let original = call(&["draft", "create", &db, "review-draft", &file], 0);
    assert_eq!(original["draft"]["revision"], 1);
    call(&["draft", "publish", &db, "review-draft", "1"], 0);
    let patch = sandbox.path("edit.json");
    std::fs::write(
        &patch,
        r#"{"expected_revision":1,"operations":[{"op":"set_version","version":"2.0.0"}]}"#,
    )
    .unwrap();
    let edited = call(&["draft", "edit", &db, "review-draft", &patch], 0);
    assert_eq!(edited["draft"]["revision"], 2);
    let conflict = call(&["draft", "edit", &db, "review-draft", &patch], 1);
    assert_eq!(conflict["error"]["code"], "revision_conflict");
    assert_eq!(conflict["error"]["actual_revision"], 2);
    let history = call(&["draft", "revision", &db, "review-draft", "1"], 0);
    assert_eq!(history["draft"], original["draft"]);
    let diff = call(&["draft", "diff", &db, "review-draft", "1", "2"], 0);
    assert_eq!(diff["diff"]["changes"][0]["path"], "/version");
    assert_eq!(diff["diff"]["changes"].as_array().unwrap().len(), 1);
    call(&["draft", "publish", &db, "review-draft", "2"], 0);
    let published = call(&["release", "get", &db, "requirements-review", "1.0.0"], 0);
    assert_eq!(
        published["publication"]["workflow"],
        original["draft"]["workflow"]
    );
    let digest = published["publication"]["digest"].as_str().unwrap();
    assert_eq!(call(&["release", "digest", &db, digest], 0), published);
    assert_eq!(
        call(
            &["release", "list", &db, "requirements-review", "-", "1"],
            0
        )["page"]["next_cursor"],
        "1.0.0"
    );
    call(&["draft", "delete", &db, "review-draft", "2"], 0);
    assert_eq!(
        call(&["draft", "list", &db, "-", "10"], 0)["page"]["items"],
        json!([])
    );
    assert_eq!(
        call(&["draft", "create", &db, "review-draft", &file], 1)["error"]["code"],
        "already_exists"
    );
    assert_eq!(
        call(&["release", "get", &db, "requirements-review", "1.0.0"], 0),
        published
    );
}
#[test]
fn export_round_trip_and_semantic_diff_remain_machine_readable() {
    let sandbox = Sandbox::new();
    let db = sandbox.path("registry.sqlite");
    let file = fixture("examples/review.json")
        .to_string_lossy()
        .into_owned();
    call(&["draft", "create", &db, "draft", &file], 0);
    let mut out = vec![];
    let mut err = vec![];
    assert_eq!(
        crate::run(
            ["draft", "export", &db, "draft", "yaml"].map(str::to_owned),
            &mut out,
            &mut err
        ),
        0
    );
    let exported = sandbox.path("exported.yaml");
    std::fs::write(&exported, out).unwrap();
    assert_eq!(
        call(&["diff", &file, &exported], 0)["diff"]["changes"],
        json!([])
    );
    assert!(err.is_empty());
}
#[test]
fn query_and_bad_command_do_not_create_a_database() {
    let sandbox = Sandbox::new();
    let db = sandbox.path("absent.sqlite");
    assert_eq!(
        call(&["draft", "get", &db, "missing"], 2)["error"]["code"],
        "storage"
    );
    assert!(!PathBuf::from(&db).exists());
    let mut out = vec![];
    let mut err = vec![];
    assert_eq!(
        crate::run(
            ["draft", "nonsense", &db].map(str::to_owned),
            &mut out,
            &mut err
        ),
        2
    );
    assert!(!PathBuf::from(&db).exists());
    assert!(out.is_empty());
}
#[test]
fn patch_schema_and_example_match_the_published_shape() {
    let mut out = vec![];
    assert_eq!(
        crate::run(
            ["schema", "patch"].map(str::to_owned),
            &mut out,
            &mut vec![]
        ),
        0
    );
    let generated: Value = serde_json::from_slice(&out).unwrap();
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture("schemas/workflow-patch-v1.schema.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(generated, committed);
    let patch = read_patch(
        fixture("examples/registry/review.patch.json")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let w = load(fixture("examples/review.json").to_str().unwrap()).unwrap();
    workflow_definitions::check_publish(&workflow_definitions::apply_patch(&w, &patch).unwrap())
        .unwrap();
}
