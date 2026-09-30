use super::*;
#[test]
fn missing_workspace_queries_never_create_storage() {
    let dir =
        std::env::temp_dir().join(format!("workflow-missing-workspace-{}", std::process::id()));
    let mut out = vec![];
    assert_eq!(
        run(
            &[
                "workspace",
                "observe",
                dir.to_str().unwrap(),
                "workspace-missing"
            ],
            &mut out,
            &mut vec![]
        ),
        1
    );
    assert!(!dir.exists());
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap()["ok"], false);
}
#[test]
fn committed_workspace_schemas_match() {
    let base = if let Ok(p) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    for kind in [
        "checkout",
        "ref",
        "observation",
        "output",
        "merge-plan",
        "merge-proposal",
    ] {
        let mut out = vec![];
        assert_eq!(
            run(
                &["schema", &format!("workspace-{kind}")],
                &mut out,
                &mut vec![]
            ),
            0
        );
        let actual: Value = serde_json::from_slice(&out).unwrap();
        let expected: Value = serde_json::from_slice(
            &std::fs::read(base.join(format!("schemas/workspace-{kind}-v1.schema.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }
}
