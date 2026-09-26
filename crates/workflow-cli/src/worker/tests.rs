use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};
struct Files(PathBuf);
impl Files {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "workflow-worker-cli-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn put(&self, name: &str, value: &Value) -> String {
        let p = self.0.join(name);
        fs::write(&p, serde_json::to_vec(value).unwrap()).unwrap();
        p.to_str().unwrap().into()
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn call(args: &[&str], expected: i32) -> Value {
    let mut out = vec![];
    let mut err = vec![];
    assert_eq!(
        crate::run(args.iter().map(|s| s.to_string()), &mut out, &mut err),
        expected,
        "{} {}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    serde_json::from_slice(&out).unwrap()
}
fn fixture(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("TEST_SRCDIR") {
        return PathBuf::from(dir)
            .join(std::env::var("TEST_WORKSPACE").unwrap())
            .join(name);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(name)
}
#[test]
fn prepare_grant_dispatch_and_host_result_validation_work_across_files() {
    let files = Files::new();
    let inputs = json!({"document":fs::read_to_string(fixture("examples/review.json")).unwrap(),"format":"json"});
    let job = files.put(
        "job.json",
        &json!({"request_id":"r1","trace_id":"t1","timeout_ms":60000,"inputs":inputs}),
    );
    let request = call(
        &["worker", "prepare", "workflow.validate", "1.0.0", &job],
        0,
    );
    let request_file = files.put("request.json", &request);
    let grant = call(&["worker", "grant", &request_file], 0);
    let grant_file = files.put("grant.json", &grant);
    let result = call(&["worker", "dispatch", &request_file, &grant_file], 0);
    assert_eq!(result["outcome"]["outputs"]["valid"], true);
    let result_file = files.put("result.json", &result);
    assert_eq!(
        call(
            &[
                "worker",
                "check-result",
                &request_file,
                &grant_file,
                &result_file
            ],
            0
        ),
        result
    );
    let standalone = files.put("inputs.json", &inputs);
    assert_eq!(
        call(
            &[
                "capability",
                "invoke",
                "workflow.validate",
                "1.0.0",
                &standalone
            ],
            0
        )["outcome"],
        result["outcome"]
    );
    let mut tampered = request;
    tampered["trace_id"] = json!("changed");
    let tampered = files.put("tampered.json", &tampered);
    assert_eq!(
        call(&["worker", "dispatch", &tampered, &grant_file], 1)["error"]["code"],
        "unauthorized"
    );
}
#[test]
fn metadata_and_failures_are_machine_readable_and_dont_create_files() {
    let list = call(&["capability", "list"], 0);
    assert_eq!(list["protocol_versions"], json!([1]));
    assert_eq!(list["capabilities"].as_array().unwrap().len(), 2);
    assert_eq!(
        call(
            &["capability", "describe", "workflow.validate", "latest"],
            1
        )["error"]["code"],
        "missing_capability"
    );
    let files = Files::new();
    let path = files.0.join("absent.json");
    call(&["worker", "grant", path.to_str().unwrap()], 2);
    assert!(!path.exists());
    let inputs = files.put("bad-format.json", &json!({"document":"{}","format":"toml"}));
    assert_eq!(
        call(
            &[
                "capability",
                "invoke",
                "workflow.validate",
                "1.0.0",
                &inputs
            ],
            1
        )["outcome"]["code"],
        "invalid_format"
    );
}

#[test]
fn shipped_node_example_invokes_the_same_compiler_capability() {
    let files = Files::new();
    let definition = fixture("examples/worker/inspect-definition.json");
    let job = fixture("examples/worker/node-job.json");
    let request = call(
        &[
            "worker",
            "prepare-node",
            definition.to_str().unwrap(),
            "inspect",
            job.to_str().unwrap(),
        ],
        0,
    );
    assert_eq!(request["scope"]["type"], "workflow");
    assert_eq!(request["scope"]["lease_epoch"], 1);
    let path = files.put("request.json", &request);
    let grant = files.put("grant.json", &call(&["worker", "grant", &path], 0));
    let result = call(&["worker", "dispatch", &path, &grant], 0);
    let inputs = fixture("examples/worker/inputs.json");
    let independent = call(
        &[
            "capability",
            "invoke",
            "workflow.validate",
            "1.0.0",
            inputs.to_str().unwrap(),
        ],
        0,
    );
    assert_eq!(result["outcome"], independent["outcome"]);
    let mut malicious = result;
    malicious["outcome"]["outputs"]["valid"] = json!("yes");
    let output = files.put("malicious.json", &malicious);
    assert_eq!(
        call(&["worker", "check-result", &path, &grant, &output], 1)["error"]["code"],
        "invalid_output"
    );
}

#[test]
fn committed_worker_schemas_match_the_runtime() {
    for kind in ["capability", "request", "grant", "result"] {
        let committed: Value = serde_json::from_slice(
            &fs::read(fixture(&format!("schemas/worker-{kind}-v1.schema.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(call(&["schema", kind], 0), committed);
    }
}
