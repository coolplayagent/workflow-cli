use super::*;
use std::{io::Write, net::TcpListener, thread};

fn binding(provider: Provider, endpoint: String) -> HttpBinding {
    HttpBinding {
        schema_version: 1,
        provider,
        model: "fixture-model".into(),
        endpoint,
        credential: None,
        api_key_env: "WORKFLOW_HTTP_TEST_KEY".into(),
        allow_loopback_http: true,
    }
}
fn call() -> ModelCall {
    ModelCall {
        protocol_version: 1, request_digest: digest(&"request").unwrap(),
        policy: serde_json::from_value(json!({"schema_version":1,"policy":{"id":"test.policy","version":"1.0.0"},
            "goal":"Inspect", "tools":[], "budget":{"model_calls":2,"tool_calls":0,"context_bytes":8192,"response_bytes":4096,"output_tokens_per_call":512},
            "task":{"schema_version":1,"capability":{"id":"test.task","version":"1.0.0"},"inputs":{},"outputs":{},"timeout_ms":10000,
                "error_codes":{},"effects":{"type":"read_only"},"usage":"Inspect","skill":null}})).unwrap(),
        inputs: Default::default(), events: vec![], remaining_model_calls: 2, remaining_tool_calls: 0,
        deadline_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 + 10000,
    }
}
fn proposal() -> String {
    json!({"protocol_version":1,"action":{"type":"complete","outputs":{},"summary":"Inspection complete"}}).to_string()
}
fn response(provider: &Provider) -> Value {
    match provider {
        Provider::OpenaiResponses => {
            json!({"id":"resp_fixture","model":"resolved-model","status":"completed",
            "output":[{"type":"reasoning","summary":[]},{"type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":proposal()}]}],"usage":{"input_tokens":20,"output_tokens":30}})
        }
        Provider::AnthropicMessages => {
            json!({"id":"msg_fixture","model":"resolved-model","type":"message","role":"assistant","stop_reason":"end_turn",
            "content":[{"type":"thinking","thinking":"not retained"},{"type":"text","text":proposal()}],"usage":{"input_tokens":20,"output_tokens":30}})
        }
    }
}
fn server(
    status: &str,
    extra: &str,
    body: String,
) -> (String, thread::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/model", listener.local_addr().unwrap());
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    );
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let (header, end, length) = loop {
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buf[..n]);
            if let Some(i) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                let header = String::from_utf8(bytes[..i].to_vec())
                    .unwrap()
                    .to_lowercase();
                let length = header
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                break (header, i + 4, length);
            }
        };
        while bytes.len() < end + length {
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buf[..n]);
        }
        let body = serde_json::from_slice(&bytes[end..end + length]).unwrap();
        stream.write_all(reply.as_bytes()).unwrap();
        (header, body)
    });
    (url, handle)
}
#[test]
fn both_provider_wires_send_bounded_context_and_preserve_visible_identity() {
    for provider in [Provider::OpenaiResponses, Provider::AnthropicMessages] {
        let (url, handle) = server("200 OK", "", response(&provider).to_string());
        let model = HttpModel::new(binding(provider.clone(), url)).unwrap();
        let reply = model.request_with_key(&call(), "fixture-key").unwrap();
        assert_eq!(reply.resolved_model, "resolved-model");
        assert_eq!(reply.usage.output_tokens, Some(30));
        assert!(
            !serde_json::to_string(&reply)
                .unwrap()
                .contains("not retained")
        );
        let (headers, body) = handle.join().unwrap();
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["stream"], false);
        let context = match provider {
            Provider::OpenaiResponses => {
                assert!(headers.contains("authorization: bearer fixture-key"));
                assert_eq!(body["store"], false);
                assert_eq!(body["tools"], json!([]));
                assert_eq!(body["max_output_tokens"], 512);
                &body["input"]
            }
            Provider::AnthropicMessages => {
                assert!(headers.contains("x-api-key: fixture-key"));
                assert!(headers.contains("anthropic-version: 2023-06-01"));
                assert_eq!(body["max_tokens"], 512);
                &body["messages"][0]["content"]
            }
        };
        let context: Value = serde_json::from_str(context.as_str().unwrap()).unwrap();
        assert_eq!(context["remaining_model_calls"], 2);
        assert_eq!(context["policy"]["goal"], "Inspect");
        assert!(!body.to_string().contains("fixture-key"));
    }
}
#[test]
fn malformed_truncated_refused_and_oversized_responses_are_failures() {
    for provider in [Provider::OpenaiResponses, Provider::AnthropicMessages] {
        let good = response(&provider);
        assert!(decode(&provider, br#"{"id":"one","id":"two"}"#, 4096).is_err());
        assert_eq!(
            decode(&provider, &serde_json::to_vec(&good).unwrap(), 2),
            Err(ModelFailure::Budget)
        );
        for mode in 0..4 {
            let mut v = good.clone();
            match mode {
                0 => {
                    v["status"] = json!("incomplete");
                    v["stop_reason"] = json!("max_tokens");
                }
                1 => v["usage"]["output_tokens"] = json!(-1),
                2 => v["model"] = json!(null),
                _ => match provider {
                    Provider::OpenaiResponses => {
                        v["output"][1]["content"][0]["text"] =
                            json!("{\"protocol_version\":1,\"action\":{\"type\":\"cancel\"}}")
                    }
                    Provider::AnthropicMessages => v["content"][1]["text"] = json!("not JSON"),
                },
            }
            assert_eq!(
                decode(&provider, &serde_json::to_vec(&v).unwrap(), 4096),
                Err(ModelFailure::InvalidResponse)
            );
        }
    }
    let mut v = response(&Provider::OpenaiResponses);
    v["output"][1]["content"][0] = json!({"type":"refusal","refusal":"No"});
    assert_eq!(
        decode(
            &Provider::OpenaiResponses,
            &serde_json::to_vec(&v).unwrap(),
            4096
        ),
        Err(ModelFailure::Refused)
    );
}
#[test]
fn redirects_do_not_forward_credentials_and_error_bodies_are_not_recorded() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let location = format!(
        "Location: http://{}/secret\r\n",
        target.local_addr().unwrap()
    );
    for (status, extra) in [("302 Found", location.as_str()), ("503 Unavailable", "")] {
        let (url, handle) = server(status, extra, "provider echoed fixture-key".into());
        let model = HttpModel::new(binding(Provider::OpenaiResponses, url)).unwrap();
        assert_eq!(
            model.request_with_key(&call(), "fixture-key"),
            Err(ModelFailure::Unavailable)
        );
        handle.join().unwrap();
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
#[test]
fn successful_provider_responses_cannot_persist_credentials_in_proposals_or_metadata() {
    for provider in [Provider::OpenaiResponses, Provider::AnthropicMessages] {
        for field in ["summary", "output_value", "output_key", "id", "model"] {
            let key = "fixture-private-provider-key";
            let mut v = response(&provider);
            if matches!(field, "id" | "model") {
                v[field] = json!(key);
            } else {
                let mut p: Value = serde_json::from_str(&proposal()).unwrap();
                match field {
                    "summary" => p["action"]["summary"] = json!(format!("echo {key}")),
                    "output_value" => p["action"]["outputs"] = json!({"result":key}),
                    _ => p["action"]["outputs"] = json!({key:"leak"}),
                }
                // Escaping on both wire levels must not bypass the decoded check.
                let encoded = p.to_string().replace("fixture", r"\u0066ixture");
                match provider {
                    Provider::OpenaiResponses => {
                        v["output"][1]["content"][0]["text"] = json!(encoded)
                    }
                    Provider::AnthropicMessages => v["content"][1]["text"] = json!(encoded),
                }
            }
            let (url, handle) = server("200 OK", "", v.to_string());
            let reply = HttpModel::new(binding(provider.clone(), url))
                .unwrap()
                .request_with_key(&call(), key);
            assert_eq!(reply, Err(ModelFailure::InvalidResponse));
            assert!(!format!("{reply:?}").contains(key));
            handle.join().unwrap();
        }
    }
}
#[test]
fn plaintext_nonloopback_and_credential_urls_are_rejected() {
    assert!(
        binding(Provider::OpenaiResponses, "https://example.com/v1".into())
            .shared_principal()
            .is_err()
    );
    assert!(
        binding(Provider::OpenaiResponses, "http://127.0.0.1:1234/v1".into())
            .shared_principal()
            .unwrap()
            .is_none()
    );
    for url in [
        "http://example.com/v1",
        "http://localhost/v1",
        "https://key@example.com/v1",
        "https://example.com/v1?key=x",
        "https://example.com/v1#x",
    ] {
        assert!(HttpModel::new(binding(Provider::OpenaiResponses, url.into())).is_err());
    }
    let model = HttpModel::new(binding(
        Provider::OpenaiResponses,
        "https://example.com/v1".into(),
    ))
    .unwrap();
    let mut expired = call();
    expired.deadline_unix_ms = 1;
    assert_eq!(
        model.request_with_key(&expired, "fixture-key"),
        Err(ModelFailure::Deadline)
    );
    assert_eq!(
        model.request_with_key(&call(), "bad\nkey"),
        Err(ModelFailure::Unavailable)
    );
}
