use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use workflow_worker::Clock;
static NEXT: AtomicU64 = AtomicU64::new(0);
fn base() -> PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn call(args: &[&str], expected: i32) -> Value {
    let mut out = vec![];
    let mut errors = vec![];
    let code = crate::run(args.iter().map(|s| s.to_string()), &mut out, &mut errors);
    assert_eq!(
        code,
        expected,
        "{} {}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&errors)
    );
    serde_json::from_slice(&out).unwrap()
}
#[test]
fn exported_backup_and_recovery_schemas_match_the_cli() {
    for kind in [
        "backup-index",
        "backup-sources",
        "backup-restore-request",
        "run-recovery-acknowledgement",
        "run-restored-effect",
    ] {
        let actual = call(&["schema", kind], 0);
        let expected: Value = serde_json::from_slice(
            &std::fs::read(base().join(format!("schemas/{kind}-v1.schema.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected, "{kind}");
    }
}
#[test]
fn cli_restore_keeps_new_writes_blocked_until_explicit_source_audit() {
    let d = Dir(std::env::temp_dir().join(format!(
        "workflow-backup-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir(&d.0).unwrap();
    let path = |name: &str| d.0.join(name).to_str().unwrap().to_string();
    let save = |name: &str, value: &Value| {
        let p = path(name);
        std::fs::write(&p, serde_json::to_vec(value).unwrap()).unwrap();
        p
    };
    let now = || workflow_worker::SystemClock.now_unix_ms().unwrap();
    let db = path("runs.db");
    call(&["run", "init", &db], 0);
    let mut request: Value = serde_json::from_slice(
        &std::fs::read(base().join("examples/runs/effect-release.json")).unwrap(),
    )
    .unwrap();
    request["started_at_unix_ms"] = json!(now());
    call(&["run", "start", &db, &save("start.json", &request)], 0);
    let id = request["run_id"].as_str().unwrap();
    let backup = path("backup");
    let sources = save(
        "sources.json",
        &json!({"runs": db, "artifacts": null, "registry": null}),
    );
    call(&["backup", "create", &sources, &backup], 0);
    assert_eq!(call(&["backup", "verify", &backup], 0)["result"]["runs"], 1);
    let restored = path("restored");
    let restore_request = save(
        "restore.json",
        &json!({"actor": "test-operator", "reason": "test source has never dispatched a provider write"}),
    );
    call(
        &["backup", "restore", &backup, &restored, &restore_request],
        0,
    );
    let target = format!("{restored}/runs.sqlite");
    let barrier = call(&["run", "recovery", &target, id], 0)["result"].clone();
    let snapshot = call(&["run", "status", &target, id], 0)["result"].clone();
    call(
        &[
            "run",
            "resume",
            &target,
            id,
            "resume-held",
            &snapshot["revision"].to_string(),
            &now().to_string(),
            "inspect recovery hold",
        ],
        0,
    );
    let lease_request = save(
        "lease-request.json",
        &json!({"run_id": id, "owner": "test-new-owner", "acquisition_id": "restored-claim", "ttl_ms": 10000}),
    );
    let lease = call(&["run", "acquire", &target, &lease_request], 0)["result"].clone();
    let lease_file = save("lease.json", &lease);
    assert_eq!(
        call(&["run", "effect-claim", &target, &lease_file], 1)["error"]["code"],
        "recovery_required"
    );
    let mut audit = json!({"resolution_id": "audit-unissued", "no_missing_effect_intents": false, "generation": barrier["generation"], "backup_digest": barrier["backup_digest"], "actor": "test-operator", "reason": "source process stopped without dispatch", "evidence": "test source execution history has no prepared effect; fixture controls all calls"});
    call(
        &[
            "run",
            "recovery-acknowledge",
            &target,
            id,
            &save("audit.json", &audit),
        ],
        1,
    );
    audit["no_missing_effect_intents"] = json!(true);
    let result = call(
        &[
            "run",
            "recovery-acknowledge",
            &target,
            id,
            &save("audit.json", &audit),
        ],
        0,
    );
    assert!(result["result"]["pending_recovery"].is_null());
    let claimed = call(&["run", "effect-claim", &target, &lease_file], 0);
    assert_eq!(claimed["result"]["attempt"]["kind"], "write");
    // This test intentionally stops at durable intent. No provider is invoked.
    call(&["run", "verify", &target, id], 0);
}
