use super::*;
use std::path::PathBuf;
fn fixture(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("TEST_SRCDIR") {
        return PathBuf::from(dir)
            .join(std::env::var("TEST_WORKSPACE").unwrap())
            .join(name);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(name)
}
struct FixedClock;
impl Clock for FixedClock {
    fn now_unix_ms(&self) -> Result<u64> {
        Ok(100)
    }
}
fn invoke(id: &str, document: &str, format: &str) -> AdapterOutcome {
    let worker = worker().unwrap();
    let capability = worker.capability(id, "1.0.0").unwrap();
    let request = WorkRequest::standalone(
        capability,
        Values::from([
            ("document".into(), json!(document)),
            ("format".into(), json!(format)),
        ]),
        RequestContext {
            request_id: "request".into(),
            trace_id: "trace".into(),
            issued_at_unix_ms: 90,
            deadline_unix_ms: 1000,
        },
    )
    .unwrap();
    worker
        .execute_with_clock(
            &request,
            &ExecutionGrant::bind(&request).unwrap(),
            &FixedClock,
        )
        .unwrap()
        .into_result()
        .outcome
}
#[test]
fn validation_capability_matches_the_compiler_for_every_shipped_workflow() {
    for (file, format) in [
        ("review.json", "json"),
        ("review.yaml", "yaml"),
        ("parallel-tests.json", "json"),
        ("bounded-repair.json", "json"),
        ("repair-round.json", "json"),
    ] {
        let source = std::fs::read_to_string(fixture(&format!("examples/{file}"))).unwrap();
        let w = workflow_ir::parse(
            &source,
            if format == "json" {
                Format::Json
            } else {
                Format::Yaml
            },
            file,
        )
        .unwrap();
        let AdapterOutcome::Succeeded { outputs, .. } =
            invoke("workflow.validate", &source, format)
        else {
            panic!("validation failed")
        };
        assert_eq!(outputs["valid"], true);
        assert_eq!(outputs["digest"], w.digest().unwrap());
        assert_eq!(outputs["diagnostics"], json!([]));
    }
}
#[test]
fn invalid_definition_is_an_inspection_result_but_cannot_be_canonicalized() {
    let AdapterOutcome::Succeeded { outputs, .. } = invoke("workflow.validate", "{}", "json")
    else {
        panic!("inspection must run")
    };
    assert_eq!(outputs["valid"], false);
    assert!(!outputs.contains_key("digest"));
    assert!(!outputs["diagnostics"].as_array().unwrap().is_empty());
    assert!(matches!(
        invoke("workflow.canonicalize", "{}", "json"),
        AdapterOutcome::Failed {
            class: FailureClass::InvalidInput,
            ..
        }
    ));
    assert!(
        matches!(invoke("workflow.validate","{}","toml"),AdapterOutcome::Failed{code,..} if code=="invalid_format")
    );
}
#[test]
fn canonicalization_preserves_the_published_compiler_digest() {
    let source = std::fs::read_to_string(fixture("examples/review.yaml")).unwrap();
    let AdapterOutcome::Succeeded { outputs, .. } =
        invoke("workflow.canonicalize", &source, "yaml")
    else {
        panic!("canonicalization failed")
    };
    let w = workflow_ir::parse(
        outputs["document"].as_str().unwrap(),
        Format::Json,
        "result",
    )
    .unwrap();
    assert_eq!(outputs["digest"], w.digest().unwrap());
    assert_eq!(outputs["document"], w.canonical_json().unwrap());
}
