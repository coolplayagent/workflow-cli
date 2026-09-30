use super::*;
fn root() -> std::path::PathBuf {
    std::env::var("TEST_SRCDIR")
        .map(|p| std::path::PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap()))
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}
fn invoke(args: &[&str]) -> (i32, Value) {
    let (mut out, mut err) = (vec![], vec![]);
    let code = crate::run(args.iter().map(|s| s.to_string()), &mut out, &mut err);
    (
        code,
        serde_json::from_slice(if out.is_empty() { &err } else { &out }).unwrap(),
    )
}
#[test]
fn template_schema_exports_and_pure_cli_plans_match_shipped_contracts() {
    for name in [
        "template",
        "template-instance",
        "template-candidate",
        "template-owners",
    ] {
        let (code, actual) = invoke(&["schema", name]);
        assert_eq!(code, 0);
        let expected: Value = serde_json::from_slice(
            &std::fs::read(root().join(format!("schemas/{name}-v1.schema.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }
    for kind in ["defect", "feature", "release"] {
        let template = root().join(format!("examples/templates/{kind}.json"));
        let instance = root().join(format!("examples/templates/{kind}-local.json"));
        let (code, result) = invoke(&[
            "template",
            "plan",
            template.to_str().unwrap(),
            instance.to_str().unwrap(),
        ]);
        assert_eq!(code, 0);
        assert_eq!(result["request"]["run_id"], format!("example-{kind}"));
        assert_eq!(result["waits"].as_array().unwrap().len(), 2);
        let (code, diff) = invoke(&[
            "template",
            "diff",
            template.to_str().unwrap(),
            template.to_str().unwrap(),
        ]);
        assert_eq!(code, 0);
        assert_eq!(diff["new_review_required"], false);
        assert!(diff["changes"].as_array().unwrap().is_empty());
    }
}
