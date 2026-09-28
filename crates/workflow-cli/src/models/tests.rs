use super::*;
use serde_json::{Value, json};
use std::{net::TcpListener, path::PathBuf, process::Command, thread, time::Duration};

fn cli(args: &[&str]) -> Value {
    let (mut out, mut err) = (vec![], vec![]);
    assert_eq!(
        crate::run(args.iter().map(|s| s.to_string()), &mut out, &mut err),
        0,
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    serde_json::from_slice(&out).unwrap()
}
#[test]
fn child_drive() {
    let Ok(dir) = std::env::var("WORKFLOW_MODEL_TEST_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let result = cli(&[
        "run",
        "drive-models",
        dir.join("runs.db").to_str().unwrap(),
        "model-inspect",
        "fixture-owner",
        "10",
        dir.join("bindings.json").to_str().unwrap(),
    ]);
    std::fs::write(dir.join("drive.json"), serde_json::to_vec(&result).unwrap()).unwrap();
}
fn serve(listener: TcpListener, provider: &'static str) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = vec![];
            let (end, length) = loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8(bytes[..end].to_vec())
                        .unwrap()
                        .to_lowercase();
                    assert!(headers.contains("fixture-only-key"));
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while bytes.len() < end + length {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
            }
            let body: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
            let context = if provider == "openai_responses" {
                &body["input"]
            } else {
                &body["messages"][0]["content"]
            };
            let context: Value = serde_json::from_str(context.as_str().unwrap()).unwrap();
            let action = if index == 0 {
                assert_eq!(context["events"], json!([]));
                json!({"type":"call","capability":context["policy"]["tools"][0]["capability"],
                    "inputs":context["inputs"],"summary":"Run the declared validator"})
            } else {
                let events = context["events"].as_array().unwrap();
                assert_eq!(events.len(), 2);
                assert_eq!(events[1]["type"], "tool_finished");
                let outputs = &events[1]["response"]["result"]["outcome"]["outputs"];
                assert_eq!(outputs["valid"], true);
                assert!(outputs["digest"].as_str().unwrap().starts_with("sha256:"));
                json!({"type":"complete","outputs":outputs,"summary":"Return observed validation"})
            };
            let text = json!({"protocol_version":1,"action":action}).to_string();
            let reply = if provider == "openai_responses" {
                json!({"id":format!("resp-{index}"),"model":"openai-fixture-resolved","status":"completed",
                    "output":[{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}]})
            } else {
                json!({"id":format!("msg-{index}"),"model":"anthropic-fixture-resolved","type":"message","role":"assistant","stop_reason":"end_turn",
                    "content":[{"type":"text","text":text}]})
            }.to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",reply.len(),reply).unwrap();
        }
    })
}
#[test]
fn unchanged_business_bundle_runs_through_both_provider_wires_and_reopens_offline() {
    let base = if let Ok(root) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(root).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    let start = base.join("examples/models/start.json");
    let policy: PolicySpec =
        serde_json::from_slice(&std::fs::read(base.join("examples/models/policy.json")).unwrap())
            .unwrap();
    let policy = Policy::new(policy).unwrap();
    let mut snapshots = vec![];
    for provider in ["openai_responses", "anthropic_messages"] {
        let dir = std::env::temp_dir().join(format!(
            "workflow-model-cli-{}-{provider}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("runs.db");
        let db = db.to_str().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model", listener.local_addr().unwrap());
        let binding = json!([{"policy":policy.binding(),"http":{"schema_version":1,"provider":provider,
            "model":"fixture-requested","endpoint":url,"api_key_env":"WORKFLOW_MODEL_FIXTURE_KEY","allow_loopback_http":true}}]);
        std::fs::write(
            dir.join("bindings.json"),
            serde_json::to_vec(&binding).unwrap(),
        )
        .unwrap();
        cli(&["run", "init", db]);
        let initial = cli(&["run", "start", db, start.to_str().unwrap()]);
        let server = serve(listener, provider);
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "models::tests::child_drive", "--nocapture"])
            .env("WORKFLOW_MODEL_TEST_DIR", &dir)
            .env("WORKFLOW_MODEL_FIXTURE_KEY", "fixture-only-key")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        server.join().unwrap();
        let report: Value =
            serde_json::from_slice(&std::fs::read(dir.join("drive.json")).unwrap()).unwrap();
        assert_eq!(report["result"]["snapshot"]["status"], "succeeded");
        assert_eq!(report["result"]["executed_tasks"], 1);
        let history = cli(&["run", "execution-history", db, "model-inspect", "0", "100"]);
        let history_text = history.to_string();
        assert!(!history_text.contains("fixture-only-key"));
        assert!(history_text.contains(if provider == "openai_responses" {
            "openai.responses"
        } else {
            "anthropic.messages"
        }));
        cli(&["run", "verify", db, "model-inspect"]);
        let again = cli(&[
            "run",
            "drive-models",
            db,
            "model-inspect",
            "offline-owner",
            "10",
            dir.join("bindings.json").to_str().unwrap(),
        ]);
        assert_eq!(again["result"]["executed_tasks"], 0);
        assert_eq!(again["result"]["snapshot"], report["result"]["snapshot"]);
        snapshots.push((
            initial["result"]["snapshot"]["bundle_digest"].clone(),
            report["result"]["snapshot"]["frames"]["1"]["nodes"]["inspect"]["outputs"].clone(),
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
    assert!(!snapshots[0].0.is_null());
    assert_eq!(snapshots[0], snapshots[1]);
}
