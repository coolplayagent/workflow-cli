use super::*;
#[test]
fn missing_database_queries_do_not_create_storage() {
    let path = std::env::temp_dir().join(format!("workflow-missing-run-db-{}", std::process::id()));
    let mut out = vec![];
    assert_eq!(
        run(
            &["run", "status", path.to_str().unwrap(), "run"],
            &mut out,
            &mut vec![]
        ),
        1
    );
    assert!(!path.exists());
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap()["ok"], false);
}
fn base() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        std::path::PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn invoke(args: &[&str]) -> (i32, Value) {
    let mut out = vec![];
    let code = crate::run(args.iter().map(|a| a.to_string()), &mut out, &mut vec![]);
    (code, serde_json::from_slice(&out).unwrap())
}
#[test]
fn durable_cli_commits_recovers_and_reports_business_status_separately() {
    let dir = std::env::temp_dir().join(format!("workflow-run-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("runs.db");
    let db = db.to_str().unwrap();
    let start = base().join("examples/runs/review-start.json");
    assert_eq!(invoke(&["run", "start", db, start.to_str().unwrap()]).0, 1);
    assert!(!std::path::Path::new(db).exists());
    assert_eq!(invoke(&["run", "init", db]).0, 0);
    let (code, started) = invoke(&["run", "start", db, start.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(started["result"]["snapshot"]["status"], "running");
    let duplicate = invoke(&["run", "start", db, start.to_str().unwrap()]).1;
    assert_eq!(duplicate["result"]["transition"]["duplicate"], true);
    let receipt = base().join("examples/runs/timer-delivered.json");
    assert_eq!(
        invoke(&["run", "acknowledge", db, receipt.to_str().unwrap()]).0,
        0
    );
    for file in ["review-approved", "implementation-completed"] {
        let path = base().join(format!("examples/runs/{file}.json"));
        assert_eq!(invoke(&["run", "event", db, path.to_str().unwrap()]).0, 0);
    }
    let (code, status) = invoke(&["run", "status", db, "demo-review-approved"]);
    assert_eq!(code, 0);
    assert_eq!(status["result"]["status"], "succeeded");
    assert_eq!(status["result"]["revision"], 3);
    let history = invoke(&["run", "history", db, "demo-review-approved", "0", "1"]).1;
    assert_eq!(history["result"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(history["result"]["next_cursor"], 2);
    let commands = invoke(&[
        "run",
        "outbox",
        db,
        "demo-review-approved",
        "0",
        "100",
        "pending",
    ])
    .1;
    assert_eq!(commands["result"]["items"].as_array().unwrap().len(), 2);
    let verify = invoke(&["run", "verify", db, "demo-review-approved"]).1;
    assert_eq!(verify["result"]["events_checked"], 2);
    assert_eq!(verify["result"]["commands_checked"], 3);
    assert_eq!(
        invoke(&[
            "run",
            "cancel",
            db,
            "demo-review-approved",
            "late",
            "3",
            "1004"
        ])
        .0,
        1
    );
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn cancellation_is_durable_idempotent_and_does_not_claim_task_completion() {
    let dir = std::env::temp_dir().join(format!("workflow-run-cancel-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("start.json");
    let db = dir.join("runs.db");
    let db = db.to_str().unwrap();
    let mut req: StartRun = read(
        base()
            .join("examples/runs/review-start.json")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    req.run_id = "cancel-demo".into();
    std::fs::write(&path, serde_json::to_vec(&req).unwrap()).unwrap();
    assert_eq!(invoke(&["run", "init", db]).0, 0);
    assert_eq!(invoke(&["run", "start", db, path.to_str().unwrap()]).0, 0);
    let args = ["run", "cancel", db, "cancel-demo", "cancel-1", "1", "1001"];
    let (code, cancelled) = invoke(&args);
    assert_eq!(code, 0);
    assert_eq!(cancelled["result"]["snapshot"]["status"], "cancelled");
    assert_eq!(invoke(&args).1["result"]["transition"]["duplicate"], true);
    let pending = invoke(&["run", "outbox", db, "cancel-demo", "0", "100", "pending"]).1;
    assert_eq!(pending["result"]["items"].as_array().unwrap().len(), 2);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn generated_run_schemas_and_shipped_requests_match_the_contract() {
    for kind in ["start", "receipt"] {
        let (code, actual) = invoke(&["schema", &format!("run-{kind}")]);
        assert_eq!(code, 0);
        let expected: Value = serde_json::from_slice(
            &std::fs::read(base().join(format!("schemas/run-{kind}-v1.schema.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }
    let _: StartRun = read(
        base()
            .join("examples/runs/review-start.json")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let _: DeliveryReceipt = read(
        base()
            .join("examples/runs/timer-delivered.json")
            .to_str()
            .unwrap(),
    )
    .unwrap();
}
