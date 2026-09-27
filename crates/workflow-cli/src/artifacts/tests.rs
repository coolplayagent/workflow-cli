use super::*;
use std::path::PathBuf;
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let d = Self(
            std::env::temp_dir().join(format!("workflow-artifact-cli-{}", std::process::id())),
        );
        std::fs::create_dir(&d.0).unwrap();
        d
    }
    fn file(&self, name: &str, value: &impl Serialize) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, serde_json::to_vec(value).unwrap()).unwrap();
        p.to_str().unwrap().into()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn base() -> PathBuf {
    if let Ok(p) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn invoke(args: &[&str]) -> (i32, Value) {
    let mut out = vec![];
    let mut err = vec![];
    let code = crate::run(args.iter().map(|s| s.to_string()), &mut out, &mut err);
    let value = serde_json::from_slice(&out).unwrap_or_else(|_| {
        panic!(
            "{args:?}: {} {}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        )
    });
    (code, value)
}
fn ok(args: &[&str]) -> Value {
    let (code, value) = invoke(args);
    assert_eq!(code, 0, "{args:?}: {value}");
    value
}
#[test]
fn actual_worker_report_is_required_verified_bound_and_portable_before_completion() {
    let d = Dir::new();
    let db = d.0.join("runs.db");
    let db = db.to_str().unwrap();
    let root = d.0.join("artifacts");
    let root = root.to_str().unwrap();
    ok(&["artifact", "init", root]);
    ok(&["run", "init", db]);
    let start = base().join("examples/execution/valid-start.json");
    ok(&["run", "start", db, start.to_str().unwrap()]);
    let input=d.file("lease-input.json",&json!({"run_id":"inspect-valid","owner":"cli-test","acquisition_id":"first","ttl_ms":300000}));
    let acquired = ok(&["run", "acquire", db, &input]);
    let lease = d.file("lease.json", &acquired["result"]);
    let claimed = ok(&["run", "claim", db, &lease]);
    let attempt = &claimed["result"]["attempt"];
    let id = attempt["attempt_id"].as_str().unwrap();
    let request = d.file("request.json", &attempt["request"]);
    let grant = d.file("grant.json", &attempt["grant"]);
    let mut result = ok(&["worker", "dispatch", &request, &grant]);
    assert_eq!(result["outcome"]["outputs"]["valid"], true);
    let body = json!({"valid":result["outcome"]["outputs"]["valid"],"definition_digest":result["outcome"]["outputs"]["digest"],"diagnostics_count":result["outcome"]["outputs"]["diagnostics"].as_array().unwrap().len(),"request_digest":result["request_digest"]});
    let payload = d.file("report.json", &body);
    let source = d.file(
        "source.json",
        &json!({"repository":"fixture-repository","revision":"a".repeat(40)}),
    );
    let inputs = d.file("inputs.json", &json!([]));
    let ty = base().join("examples/artifacts/validation-report-type.json");
    let ty = ty.to_str().unwrap();
    let prepared = ok(&["artifact", "prepare", &request, ty, &source, &inputs]);
    let spec = d.file("publish.json", &prepared["result"]);
    let artifact = ok(&["artifact", "put", root, &spec, &payload]);
    let artifact = &artifact["result"];
    let artifact_id = artifact["artifact_id"].as_str().unwrap();
    ok(&["artifact", "verify", root, artifact_id, ty]);
    result["outcome"]["evidence"] =
        json!([{"artifact_id":artifact_id,"digest":artifact["digest"]}]);
    let finished = d.file("result.json", &result);
    let rejected = invoke(&["run", "finish", db, &lease, id, &finished]);
    assert_eq!(rejected.0, 1);
    assert_eq!(rejected.1["error"]["code"], "artifact_unavailable");
    assert_eq!(
        ok(&["run", "status", db, "inspect-valid"])["result"]["revision"],
        1
    );
    let mut wrong = prepared["result"].clone();
    wrong["producer"]["input_digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    let wrong = d.file("wrong-publish.json", &wrong);
    let stale = ok(&["artifact", "put", root, &wrong, &payload]);
    let mut bad = result.clone();
    bad["outcome"]["evidence"] =
        json!([{"artifact_id":stale["result"]["artifact_id"],"digest":stale["result"]["digest"]}]);
    let bad = d.file("wrong-result.json", &bad);
    let rejected = invoke(&["run", "--artifacts", root, "finish", db, &lease, id, &bad]);
    assert_eq!(rejected.0, 1);
    assert_eq!(rejected.1["error"]["code"], "artifact_rejected");
    let args = [
        "run",
        "--artifacts",
        root,
        "finish",
        db,
        &lease,
        id,
        &finished,
    ];
    let committed = ok(&args);
    assert_eq!(committed["result"]["snapshot"]["status"], "succeeded");
    assert_eq!(ok(&args)["result"]["transition"]["duplicate"], true);
    ok(&["run", "--artifacts", root, "release", db, &lease]);
    assert_eq!(invoke(&["run", "status", db, "inspect-valid"]).0, 1);
    let export = d.0.join("export.json");
    let export = export.to_str().unwrap();
    let reference = ok(&["artifact", "export", root, artifact_id, export]);
    let reference = d.file("reference.json", &reference["result"]);
    let target = d.0.join("imported");
    let target = target.to_str().unwrap();
    ok(&["artifact", "init", target]);
    let imported = ok(&["artifact", "import", target, &reference, export]);
    assert_eq!(imported["result"], *artifact);
    ok(&["run", "--artifacts", target, "verify", db, "inspect-valid"]);
    assert_eq!(
        invoke(&["artifact", "export", root, artifact_id, export]).0,
        2
    );
    let bad_content = d.file("bad-content.json", &json!({}));
    assert_eq!(
        invoke(&["artifact", "import", target, &reference, &bad_content]).0,
        1
    );
    let object = Path::new(target)
        .join("objects")
        .join(&artifact["manifest"]["content_digest"].as_str().unwrap()[7..]);
    std::fs::write(&object, b"corrupted").unwrap();
    assert_eq!(
        invoke(&["run", "--artifacts", target, "status", db, "inspect-valid"]).0,
        1
    );
    assert_eq!(
        ok(&["run", "--artifacts", root, "status", db, "inspect-valid"])["result"]["status"],
        "succeeded"
    );
}
#[test]
fn committed_artifact_schemas_match_generated_contracts() {
    for kind in ["publish", "ref", "type"] {
        let generated = ok(&["schema", &format!("artifact-{kind}")]);
        let expected: Value = serde_json::from_slice(
            &std::fs::read(base().join(format!("schemas/artifact-{kind}-v1.schema.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(generated, expected);
    }
}
