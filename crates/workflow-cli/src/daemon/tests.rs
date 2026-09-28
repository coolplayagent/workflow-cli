use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use workflow_runstore::{ExecutionStore, InboxStore};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn base() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        Path::new(&p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
struct Dir(std::path::PathBuf);
impl Dir {
    fn new() -> Self {
        let d = Self(std::env::temp_dir().join(format!(
            "wfc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::create_dir(&d.0).unwrap();
        d
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[track_caller]
fn wait(mut test: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !test() {
        assert!(Instant::now() < until, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn config(d: &Dir) -> Config {
    Config {
        schema_version: 1,
        database: d.0.join("runs.db").to_str().unwrap().into(),
        control_directory: d.0.join("control").to_str().unwrap().into(),
        artifacts: None,
        model_bindings: None,
        effect_bindings: None,
        poll_interval_ms: 20,
        error_backoff_ms: 100,
    }
}
fn start(c: &Config) -> Child {
    let path = Path::new(&c.database).with_extension("config.json");
    std::fs::write(&path, serde_json::to_vec(c).unwrap()).unwrap();
    let child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "daemon::tests::daemon_child", "--nocapture"])
            .env("WORKFLOW_DAEMON_CLI_CONFIG", &path)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait(|| {
        workflow_daemon_local::inspect(&c.control_directory)
            .unwrap()
            .availability
            == Availability::Responsive
    });
    child
}
#[test]
fn daemon_child() {
    let Ok(path) = std::env::var("WORKFLOW_DAEMON_CLI_CONFIG") else {
        return;
    };
    assert_eq!(
        run(
            &["daemon", "serve", &path],
            &mut std::io::sink(),
            &mut std::io::stderr()
        ),
        0
    );
}
fn request(id: &str, timeout: u64) -> workflow_runstore::StartRun {
    let mut value: Value = serde_json::from_slice(
        &std::fs::read(base().join("examples/execution/offline-start.json")).unwrap(),
    )
    .unwrap();
    value["run_id"] = json!(id);
    value["bundle"]["root"]["version"] = json!(format!("wait-{timeout}"));
    value["bundle"]["workflows"][0]["version"] = json!(format!("wait-{timeout}"));
    value["started_at_unix_ms"] = json!(workflow_worker::SystemClock.now_unix_ms().unwrap());
    for node in value["bundle"]["workflows"][0]["nodes"]
        .as_array_mut()
        .unwrap()
    {
        if node["id"] == "review" {
            node["kind"]["timeout_ms"] = json!(timeout);
        }
    }
    serde_json::from_value(value).unwrap()
}
fn finished(store: &mut impl ExecutionStore, id: &str) -> usize {
    store
        .execution_history(id, 0, 100)
        .unwrap()
        .items
        .iter()
        .filter(|r| {
            matches!(
                r.action,
                workflow_runstore::ExecutionAction::Finished { .. }
            )
        })
        .count()
}
#[test]
fn daemon_recovers_parallel_loop_and_branch_work_then_wakes_on_callback_and_timeout() {
    let d = Dir::new();
    let c = config(&d);
    let mut store = workflow_runstore_sqlite::SqliteRunStore::create(&c.database).unwrap();
    for id in ["approved", "rejected"] {
        store.start(&request(id, 30000)).unwrap();
    }
    let mut child = start(&c);
    wait(|| {
        store.waits("approved", 0, 100).unwrap().items.len() == 1
            && store.waits("rejected", 0, 100).unwrap().items.len() == 1
    });
    assert_eq!(finished(&mut store, "approved"), 2);
    // Create the timer only after the CPU-intensive builtin setup has finished.
    // This seed starts directly at a wait, so observing it does not race task work.
    let mut timed: Value = serde_json::from_slice(
        &std::fs::read(base().join("examples/runs/review-start.json")).unwrap(),
    )
    .unwrap();
    timed["run_id"] = json!("timed-out");
    timed["started_at_unix_ms"] = json!(workflow_worker::SystemClock.now_unix_ms().unwrap());
    for workflow in timed["bundle"]["workflows"].as_array_mut().unwrap() {
        for node in workflow["nodes"].as_array_mut().unwrap() {
            if node["kind"]["type"] == "wait" {
                node["kind"]["timeout_ms"] = json!(5000);
            }
        }
    }
    store
        .start(&serde_json::from_value(timed).unwrap())
        .unwrap();
    let deadline = store.waits("timed-out", 0, 100).unwrap().items[0].deadline_unix_ms;
    // Stop the whole process, including its control listener, across an absolute deadline.
    assert!(
        std::process::Command::new("kill")
            .args(["-STOP", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        workflow_daemon_local::inspect(&c.control_directory)
            .unwrap()
            .availability,
        Availability::Unreachable
    );
    match store.get("timed-out") {
        Ok(snapshot) => assert_eq!(snapshot.status, RunStatus::Running),
        // SIGSTOP may freeze the process while SQLite holds its transaction lock.
        Err(e) => assert_eq!(e.code, workflow_runstore::ErrorCode::Busy),
    }
    let now = workflow_worker::SystemClock.now_unix_ms().unwrap();
    std::thread::sleep(Duration::from_millis(deadline.saturating_sub(now) + 50));
    assert!(
        std::process::Command::new("kill")
            .args(["-CONT", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait(|| store.get("timed-out").unwrap().status == RunStatus::Failed);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert_eq!(
        workflow_daemon_local::inspect(&c.control_directory)
            .unwrap()
            .availability,
        Availability::Stopped
    );
    let first = store.get("approved").unwrap();
    store.verify("approved").unwrap();
    let mut child = start(&c);
    for (id, decision) in [("approved", "approve"), ("rejected", "reject")] {
        let target = store.waits(id, 0, 100).unwrap().items.remove(0);
        let s = store.get(id).unwrap();
        let submission=serde_json::from_value(json!({"schema_version":1,"run_id":id,"run_digest":s.run_digest,"message":{"schema_version":1,"message_id":format!("callback-{id}"),"source":"local-test-operator","target":target.target,"correlation_id":target.correlation_id,"decision":decision,"outputs":{},"expires_at_unix_ms":target.deadline_unix_ms,"reason":"actual test operator response"}})).unwrap();
        store
            .receive_signal(&submission, &workflow_worker::SystemClock)
            .unwrap();
    }
    wait(|| store.get("timed-out").unwrap().status == RunStatus::Failed);
    assert_eq!(store.get("approved").unwrap().status, RunStatus::Succeeded);
    assert_eq!(store.get("rejected").unwrap().status, RunStatus::Cancelled);
    assert_eq!(
        finished(&mut store, "approved"),
        2,
        "committed tasks re-executed after restart"
    );
    assert!(store.get("approved").unwrap().revision > first.revision);
    for id in ["approved", "rejected", "timed-out"] {
        store.verify(id).unwrap();
    }
    let instance = workflow_daemon_local::inspect(&c.control_directory)
        .unwrap()
        .status
        .unwrap()
        .instance;
    workflow_daemon_local::request_stop(&c.control_directory, &instance).unwrap();
    wait(|| child.0.try_wait().unwrap().is_some());
    assert_eq!(
        workflow_daemon_local::inspect(&c.control_directory)
            .unwrap()
            .availability,
        Availability::Stopped
    );
}
#[test]
fn competing_cli_child() {
    let Ok(input) = std::env::var("WORKFLOW_DAEMON_RACE_INPUT") else {
        return;
    };
    let data: Value = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    wait(|| Path::new(data["go"].as_str().unwrap()).exists());
    let args: Vec<String> = serde_json::from_value(data["args"].clone()).unwrap();
    let mut out = vec![];
    let code = crate::run(args, &mut out, &mut std::io::stderr());
    let response: Value = serde_json::from_slice(&out).unwrap();
    std::fs::write(
        data["out"].as_str().unwrap(),
        serde_json::to_vec(&json!({"code":code,"response":response})).unwrap(),
    )
    .unwrap();
}
fn race(d: &Dir, label: &str, args: Vec<Vec<String>>) -> Vec<Value> {
    let go = d.0.join(format!("{label}-go"));
    let mut children = vec![];
    let mut outputs = vec![];
    for (i, args) in args.into_iter().enumerate() {
        let input = d.0.join(format!("{label}-{i}.json"));
        let output = d.0.join(format!("{label}-{i}-out.json"));
        std::fs::write(
            &input,
            serde_json::to_vec(&json!({"go":go,"out":output,"args":args})).unwrap(),
        )
        .unwrap();
        outputs.push(output);
        children.push(Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "daemon::tests::competing_cli_child"])
                .env("WORKFLOW_DAEMON_RACE_INPUT", input)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        ));
    }
    std::fs::write(go, b"go").unwrap();
    for child in &mut children {
        wait(|| child.0.try_wait().unwrap().is_some());
    }
    outputs
        .into_iter()
        .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
        .collect()
}
#[test]
fn concurrent_cli_resume_and_acquire_have_only_one_cas_winner_and_owner() {
    let d = Dir::new();
    let c = config(&d);
    let mut store = workflow_runstore_sqlite::SqliteRunStore::create(&c.database).unwrap();
    let s = store
        .start(&request("resume-race", 30000))
        .unwrap()
        .snapshot;
    let now = || workflow_worker::SystemClock.now_unix_ms().unwrap();
    let paused = store
        .apply(&workflow_runstore::Event {
            event_id: "pause-for-race".into(),
            run_id: s.run_id.clone(),
            run_digest: s.run_digest,
            expected_revision: s.revision,
            at_unix_ms: now(),
            kind: workflow_kernel::EventKind::Pause {
                reason: "concurrency check".into(),
            },
        })
        .unwrap()
        .snapshot;
    let args = (0..2)
        .map(|i| {
            vec![
                "run".into(),
                "resume".into(),
                c.database.clone(),
                s.run_id.clone(),
                format!("resume-{i}"),
                paused.revision.to_string(),
                now().to_string(),
                "operator resumed".into(),
            ]
        })
        .collect();
    let results = race(&d, "resume", args);
    assert_eq!(
        results.iter().filter(|r| r["code"] == 0).count(),
        1,
        "{results:?}"
    );
    let args=(0..2).map(|i| {
        let file=d.0.join(format!("lease-{i}.json"));std::fs::write(&file,serde_json::to_vec(&json!({"run_id":s.run_id,"owner":format!("owner-{i}"),"acquisition_id":format!("acquire-{i}"),"ttl_ms":10000})).unwrap()).unwrap();
        vec!["run".into(),"acquire".into(),c.database.clone(),file.to_str().unwrap().into()]
    }).collect();
    let results = race(&d, "acquire", args);
    assert_eq!(
        results.iter().filter(|r| r["code"] == 0).count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        results.iter().find(|r| r["code"] != 0).unwrap()["response"]["error"]["code"],
        "lease_busy"
    );
    store.verify(&s.run_id).unwrap();
}
#[test]
fn idle_waits_do_not_create_lease_history_on_every_scan_and_storage_errors_are_visible() {
    let d = Dir::new();
    let c = config(&d);
    let mut store = workflow_runstore_sqlite::SqliteRunStore::create(&c.database).unwrap();
    store.start(&request("waiting", 30000)).unwrap();
    let server = Server::start(
        &c.control_directory,
        Info {
            database: c.database.clone(),
            configuration_digest: "test".into(),
            poll_interval_ms: 20,
        },
    )
    .unwrap();
    let control = server.control();
    let mut p = Poller {
        cursor: None,
        delayed: BTreeMap::new(),
        models: vec![],
        effects: None,
        sequence: 0,
    };
    for _ in 0..10 {
        p.poll(&c, &control).unwrap();
    }
    assert_eq!(store.waits("waiting", 0, 100).unwrap().items.len(), 1);
    let history = store.execution_history("waiting", 0, 100).unwrap().items;
    for _ in 0..5 {
        p.poll(&c, &control).unwrap();
    }
    assert_eq!(
        store.execution_history("waiting", 0, 100).unwrap().items,
        history
    );
    let mut missing = c.clone();
    missing.database = d.0.join("absent.db").to_str().unwrap().into();
    assert!(p.poll(&missing, &control).is_err());
    assert!(!Path::new(&missing.database).exists());
}
#[test]
fn daemon_usage_does_not_claim_a_missing_service_is_scheduling_timers() {
    let p = std::env::temp_dir().join(format!("missing-wfd-{}", std::process::id()));
    let mut out = vec![];
    let mut err = vec![];
    assert_eq!(
        run(
            &["daemon", "status", p.to_str().unwrap()],
            &mut out,
            &mut err
        ),
        0
    );
    let value: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(value["result"]["availability"], "stopped");
    assert!(value["result"]["status"].is_null());
}
#[test]
fn daemon_configuration_schema_matches_exported_contract() {
    let mut out = vec![];
    assert_eq!(
        run(&["schema", "daemon-config"], &mut out, &mut std::io::sink()),
        0
    );
    let actual: Value = serde_json::from_slice(&out).unwrap();
    let expected: Value = serde_json::from_slice(
        &std::fs::read(base().join("schemas/daemon-config-v1.schema.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(actual, expected);
}
