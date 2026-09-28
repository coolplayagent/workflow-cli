use super::*;
#[test]
fn pause_resume_cli_is_persistent_audited_and_compare_and_swap_protected() {
    let dir = std::env::temp_dir().join(format!("workflow-run-pause-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("runs.db");
    let db = db.to_str().unwrap();
    let start = base().join("examples/runs/review-start.json");
    assert_eq!(invoke(&["run", "init", db]).0, 0);
    assert_eq!(invoke(&["run", "start", db, start.to_str().unwrap()]).0, 0);
    let args = [
        "run",
        "pause",
        db,
        "demo-review-approved",
        "pause-1",
        "1",
        "1001",
        "maintenance",
    ];
    let (code, paused) = invoke(&args);
    assert_eq!(code, 0);
    assert_eq!(
        paused["result"]["snapshot"]["pause"]["reason"],
        "maintenance"
    );
    assert_eq!(invoke(&args).1["result"]["transition"]["duplicate"], true);
    let resumed = invoke(&[
        "run",
        "resume",
        db,
        "demo-review-approved",
        "resume-1",
        "2",
        "1002",
        "ready",
    ]);
    assert_eq!(resumed.0, 0);
    assert!(resumed.1["result"]["snapshot"].get("pause").is_none());
    assert_eq!(
        invoke(&[
            "run",
            "resume",
            db,
            "demo-review-approved",
            "resume-stale",
            "2",
            "1002",
            "ready"
        ])
        .0,
        1
    );
    let history = invoke(&["run", "history", db, "demo-review-approved", "0", "100"]).1;
    assert_eq!(
        history["result"]["items"][0]["event"]["kind"]["type"],
        "pause"
    );
    assert_eq!(
        history["result"]["items"][1]["event"]["kind"]["type"],
        "resume"
    );
    assert_eq!(invoke(&["run", "verify", db, "demo-review-approved"]).0, 0);
    std::fs::remove_dir_all(dir).unwrap();
}
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
    for kind in ["start", "receipt", "lease", "execution-record", "signal"] {
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

#[test]
fn inbox_cli_uses_live_wait_binding_and_acknowledges_exact_redelivery_without_resampling_time() {
    use workflow_worker::Clock;
    let dir = std::env::temp_dir().join(format!("workflow-inbox-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("runs.db");
    let db = db.to_str().unwrap();
    let mut start: StartRun = read(
        base()
            .join("examples/runs/review-start.json")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    start.started_at_unix_ms = workflow_worker::SystemClock.now_unix_ms().unwrap();
    let start_file = dir.join("start.json");
    std::fs::write(&start_file, serde_json::to_vec(&start).unwrap()).unwrap();
    assert_eq!(invoke(&["run", "init", db]).0, 0);
    let started = invoke(&["run", "start", db, start_file.to_str().unwrap()]);
    assert_eq!(started.0, 0);
    let (code, waits) = invoke(&["run", "waits", db, &start.run_id, "0", "100"]);
    assert_eq!(code, 0);
    let wait = &waits["result"]["items"][0];
    let request = json!({
        "schema_version":1,"run_id":start.run_id,"run_digest":started.1["result"]["snapshot"]["run_digest"],
        "message":{"schema_version":1,"message_id":"callback-1","target":wait["target"],"correlation_id":wait["correlation_id"],"source":"simulation","decision":"request_changes","reason":"fixture only","outputs":{},"expires_at_unix_ms":wait["deadline_unix_ms"]}
    });
    let file = dir.join("signal.json");
    std::fs::write(&file, serde_json::to_vec(&request).unwrap()).unwrap();
    let first = invoke(&["run", "receive", db, file.to_str().unwrap()]);
    assert_eq!(first.0, 0);
    assert_eq!(first.1["result"]["entry"]["status"]["status"], "applied");
    let duplicate = invoke(&["run", "receive", db, file.to_str().unwrap()]);
    assert_eq!(duplicate.0, 0);
    assert_eq!(duplicate.1["result"]["duplicate"], true);
    assert_eq!(duplicate.1["result"]["run_revision"], 2);
    let inbox = invoke(&["run", "inbox", db, &start.run_id, "0", "100"]).1;
    assert_eq!(inbox["result"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        invoke(&["run", "status", db, &start.run_id]).1["result"]["status"],
        "cancelled"
    );
    assert_eq!(invoke(&["run", "verify", db, &start.run_id]).0, 0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn drive_executes_real_builtins_and_reports_business_failure_and_storage_errors() {
    let dir = std::env::temp_dir().join(format!("workflow-drive-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("runs.db");
    let db = db.to_str().unwrap();
    assert_eq!(invoke(&["run", "init", db]).0, 0);
    assert_eq!(invoke(&["run", "migrate", db]).0, 0);
    for (name, status) in [("valid", "succeeded"), ("invalid", "failed")] {
        let id = format!("inspect-{name}");
        let path = base().join(format!("examples/execution/{name}-start.json"));
        assert_eq!(invoke(&["run", "start", db, path.to_str().unwrap()]).0, 0);
        let (code, report) = invoke(&["run", "drive", db, &id, "cli-test", "10"]);
        assert_eq!(code, 0, "{report}");
        assert_eq!(report["result"]["executed_tasks"], 1);
        assert_eq!(report["result"]["snapshot"]["status"], status);
        assert_eq!(
            invoke(&["run", "drive", db, &id, "cli-test", "10"]).1["result"]["executed_tasks"],
            0
        );
        let records = invoke(&["run", "execution-history", db, &id, "0", "100"]).1;
        assert!(
            records["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["action"]["type"] == "finished")
        );
        assert_eq!(invoke(&["run", "verify", db, &id]).0, 0);
    }
    let (code, error) = invoke(&["run", "drive", db, "missing", "cli-test", "10"]);
    assert_eq!(code, 1);
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["storage"]["code"], "not_found");
    assert_eq!(
        invoke(&["run", "drive", db, "inspect-valid", "cli-test", "0"]).0,
        1
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn gated_drive_stops_on_unknown_and_retry_intent_is_cas_bound_and_idempotent() {
    let dir = std::env::temp_dir().join(format!("workflow-gated-drive-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("runs.db");
    let db = db.to_str().unwrap();
    let path = base().join("examples/gates/guarded-start.json");
    assert_eq!(invoke(&["run", "init", db]).0, 0);
    assert_eq!(invoke(&["run", "start", db, path.to_str().unwrap()]).0, 0);
    let drive = || invoke(&["run", "drive", db, "guarded-validation", "cli-test", "10"]);
    let (code, first) = drive();
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["result"]["executed_tasks"], 1);
    assert_eq!(first["result"]["snapshot"]["status"], "running");
    let snapshot = &first["result"]["snapshot"];
    let node = &snapshot["frames"]["1"]["nodes"]["inspect"];
    assert_eq!(node["gate_decision"]["verdict"], "UNKNOWN");
    assert_eq!(node["state"]["awaiting"], false);
    let instance = node["instance_id"].to_string();
    let revision = snapshot["revision"].to_string();
    let retry = [
        "run",
        "retry-gate",
        db,
        "guarded-validation",
        &instance,
        "retry-1",
        &revision,
    ];
    assert_eq!(
        invoke(&[
            "run",
            "retry-gate",
            db,
            "guarded-validation",
            &instance,
            "stale",
            "1"
        ])
        .0,
        1
    );
    let idle = drive();
    assert_eq!(idle.1["result"]["processed_commands"], 0);
    assert_eq!(
        idle.1["result"]["snapshot"]["revision"],
        snapshot["revision"]
    );
    assert_eq!(invoke(&retry).0, 0);
    assert_eq!(invoke(&retry).1["result"]["transition"]["duplicate"], true);
    let (code, second) = drive();
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["result"]["executed_tasks"], 0);
    assert_eq!(second["result"]["processed_commands"], 1);
    assert_eq!(second["result"]["snapshot"]["status"], "running");
    assert_eq!(invoke(&retry).1["result"]["transition"]["duplicate"], true);
    assert_eq!(drive().1["result"]["processed_commands"], 0);
    assert_eq!(
        invoke(&[
            "run",
            "retry-gate",
            db,
            "guarded-validation",
            "999",
            "retry-1",
            &revision
        ])
        .0,
        1
    );
    assert_eq!(invoke(&["run", "verify", db, "guarded-validation"]).0, 0);
    std::fs::remove_dir_all(dir).unwrap();
}
