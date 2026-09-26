use super::*;
#[test]
fn missing_kernel_input_is_an_io_error() {
    let mut output = vec![];
    assert_eq!(
        crate::run(
            [
                "kernel".into(),
                "check".into(),
                "nonexistent-kernel-bundle.json".into()
            ],
            &mut output,
            &mut vec![]
        ),
        2
    );
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["ok"], false);
}

fn base() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(dir).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn invoke(args: &[&str]) -> (i32, Value) {
    let mut output = vec![];
    let code = crate::run(args.iter().map(|s| s.to_string()), &mut output, &mut vec![]);
    (code, serde_json::from_slice(&output).unwrap())
}
#[test]
fn committed_scenarios_replay_to_expected_business_outcomes() {
    for (name, status) in [
        ("review-approved", "succeeded"),
        ("review-rejected", "cancelled"),
        ("parallel-all", "succeeded"),
        ("parallel-any-cancel", "succeeded"),
        ("repair-third-round", "succeeded"),
    ] {
        let p = base().join(format!("examples/kernel/{name}.json"));
        let (code, report) = invoke(&["kernel", "replay", p.to_str().unwrap()]);
        assert_eq!(code, 0, "{report}");
        assert_eq!(report["snapshot"]["status"], status);
        if name == "repair-third-round" {
            assert_eq!(report["snapshot"]["frames"].as_object().unwrap().len(), 4);
            assert_eq!(report["snapshot"]["frames"]["2"]["status"], "failed");
            assert_eq!(report["snapshot"]["frames"]["3"]["status"], "failed");
        }
    }
}
#[test]
fn generated_kernel_schemas_match_the_committed_contracts() {
    for kind in ["bundle", "event", "scenario", "checkpoint"] {
        let (code, actual) = invoke(&["schema", &format!("kernel-{kind}")]);
        assert_eq!(code, 0);
        let expected: Value = serde_json::from_str(
            &std::fs::read_to_string(base().join(format!("schemas/kernel-{kind}-v1.schema.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected, "{kind}");
    }
}
#[test]
fn cli_restore_apply_and_retry_preserve_input_checkpoint_and_deduplicate() {
    let dir = std::env::temp_dir().join(format!("workflow-kernel-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut scenario: Value = serde_json::from_str(
        &std::fs::read_to_string(base().join("examples/kernel/parallel-all.json")).unwrap(),
    )
    .unwrap();
    let first = scenario["events"][0].clone();
    scenario["events"] = json!([]);
    let scenario_path = dir.join("scenario.json");
    let bundle_path = dir.join("bundle.json");
    let cp_path = dir.join("checkpoint.json");
    let event_path = dir.join("event.json");
    std::fs::write(&scenario_path, scenario.to_string()).unwrap();
    std::fs::write(&bundle_path, scenario["bundle"].to_string()).unwrap();
    let (code, initial) = invoke(&["kernel", "replay", scenario_path.to_str().unwrap()]);
    assert_eq!(code, 0);
    let original = initial["checkpoint"].to_string();
    std::fs::write(&cp_path, &original).unwrap();
    let (code, restored) = invoke(&[
        "kernel",
        "restore",
        bundle_path.to_str().unwrap(),
        cp_path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    assert_eq!(restored["snapshot"], initial["snapshot"]);
    assert_eq!(restored["transitions"], json!([]));
    std::fs::write(&event_path, first.to_string()).unwrap();
    let args = [
        "kernel",
        "apply",
        bundle_path.to_str().unwrap(),
        cp_path.to_str().unwrap(),
        event_path.to_str().unwrap(),
    ];
    let (code, applied) = invoke(&args);
    assert_eq!(code, 0);
    assert_eq!(applied["snapshot"]["revision"], 2);
    assert_eq!(std::fs::read_to_string(&cp_path).unwrap(), original);
    std::fs::write(&cp_path, applied["checkpoint"].to_string()).unwrap();
    let (code, duplicate) = invoke(&args);
    assert_eq!(code, 0);
    assert_eq!(duplicate["snapshot"], applied["snapshot"]);
    assert_eq!(duplicate["transitions"][0]["duplicate"], true);
    assert_eq!(duplicate["transitions"][0]["commands"], json!([]));
    let mut stale = first;
    stale["event_id"] = json!("different-id");
    std::fs::write(&event_path, stale.to_string()).unwrap();
    let (code, rejected) = invoke(&args);
    assert_eq!(code, 1);
    assert_eq!(rejected["error"]["code"], "revision_conflict");
    assert_eq!(
        std::fs::read_to_string(&cp_path).unwrap(),
        applied["checkpoint"].to_string()
    );
    std::fs::remove_dir_all(dir).unwrap();
}
