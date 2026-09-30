use super::*;
use std::{net::TcpListener, path::PathBuf};
struct Time(u64);
impl Clock for Time {
    fn now_unix_ms(&self) -> Result<u64> {
        Ok(self.0)
    }
}
fn fixture() -> EffectAttempt {
    let root = if let Ok(p) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    let spec: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("examples/runs/effect-release.json")).unwrap(),
    )
    .unwrap();
    let run_digest = digest(&"http-test").unwrap();
    let inputs = serde_json::from_value(spec["inputs"].clone()).unwrap();
    let intent = EffectIntent {
        dependencies: vec![],
        compensates: None,
        schema_version: 1,
        operation_key: operation_key(&run_digest, 1).unwrap(),
        run_id: "http-test".into(),
        run_digest,
        instance_id: 1,
        workflow: serde_json::from_value(spec["bundle"]["root"].clone()).unwrap(),
        node_id: "publish".into(),
        command_id: digest(&"command-id").unwrap(),
        command_digest: digest(&"command-body").unwrap(),
        capability: serde_json::from_value(spec["bundle"]["capabilities"][0].clone()).unwrap(),
        input_digest: digest(&inputs).unwrap(),
        inputs,
        policy: serde_json::from_value(spec["bundle"]["effect_bindings"][0]["policy"].clone())
            .unwrap(),
        created_at_unix_ms: 1000,
    };
    EffectAttempt {
        intent,
        attempt_id: "request-one".into(),
        epoch: 1,
        number: 1,
        kind: CallKind::Write,
        prepared_revision: 1,
        issued_at_unix_ms: 1000,
        deadline_unix_ms: 5000,
    }
}
fn binding(p: &EffectAttempt, endpoint: String) -> HttpEffectBinding {
    HttpEffectBinding {
        schema_version: 1,
        target: p.intent.policy.target.clone(),
        call_identity: p.intent.policy.call_identity.clone(),
        capability: p.intent.capability.clone(),
        endpoint,
        credential: None,
        api_key_env: "SANDBOX_TEST_TOKEN".into(),
        allow_loopback_http: true,
    }
}
fn server(
    status: &'static str,
    body: impl FnOnce(&EffectAttempt) -> String + Send + 'static,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = vec![];
        let mut b = [0; 4096];
        let (end, len) = loop {
            let n = socket.read(&mut b).unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&b[..n]);
            if let Some(p) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let h = std::str::from_utf8(&bytes[..p]).unwrap();
                let len = h
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                break (p + 4, len);
            }
        };
        while bytes.len() < end + len {
            let n = socket.read(&mut b).unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&b[..n]);
        }
        let p: EffectAttempt = workflow_worker::parse_message(&bytes[end..end + len]).unwrap();
        let body = body(&p);
        write!(socket,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nLocation: /must-not-follow\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        drop(socket);
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    });
    (endpoint, thread)
}
#[test]
fn non_success_and_malformed_responses_are_unknown_without_retry_or_secret_echo() {
    let p = fixture();
    for status in ["302 Found", "500 Internal Server Error", "200 OK"] {
        let (url, server) = server(status, |_| "sandbox-secret-value".into());
        let adapter = HttpEffect::new(binding(&p, url)).unwrap();
        let result = adapter
            .execute_with_secret(&p, &Time(1000), "sandbox-secret-value")
            .unwrap();
        assert!(matches!(result, Observation::Unknown { .. }));
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("sandbox-secret-value")
        );
        server.join().unwrap();
    }
}
#[test]
fn replies_require_exact_attempt_binding_and_typed_provider_receipts() {
    for valid in [false, true] {
        let p = fixture();
        let (url, server) = server("200 OK", move |p| {
            serde_json::to_string(&EffectReply {
                request_digest: if valid {
                    digest(p).unwrap()
                } else {
                    digest(&"another-request").unwrap()
                },
                observation: Observation::Applied {
                    receipt: EffectReceipt {
                        operation_key: p.intent.operation_key.clone(),
                        intent_digest: digest(&p.intent).unwrap(),
                        target: p.intent.policy.target.clone(),
                        resource_id: "release-1".into(),
                        provider_receipt: "record-1".into(),
                        outputs: [("release_id".into(), "release-1".into())].into(),
                    },
                },
            })
            .unwrap()
        });
        let value = HttpEffect::new(binding(&p, url))
            .unwrap()
            .execute_with_secret(&p, &Time(1000), "sandbox-secret-value")
            .unwrap();
        assert_eq!(matches!(value, Observation::Applied { .. }), valid);
        server.join().unwrap();
    }
}
#[test]
fn successful_secret_reflections_leave_effect_unknown_for_reconciliation() {
    for field in ["provider_receipt", "resource_id", "output", "reason"] {
        let p = fixture();
        let key = "sandbox-private-provider-key";
        let (url, server) = server("200 OK", move |p| {
            let receipt = EffectReceipt {
                operation_key: p.intent.operation_key.clone(),
                intent_digest: digest(&p.intent).unwrap(),
                target: p.intent.policy.target.clone(),
                resource_id: if field == "resource_id" {
                    key
                } else {
                    "release-1"
                }
                .into(),
                provider_receipt: if field == "provider_receipt" {
                    key
                } else {
                    "record-1"
                }
                .into(),
                outputs: [(
                    "release_id".into(),
                    if field == "output" { key } else { "release-1" }.into(),
                )]
                .into(),
            };
            let observation = if field == "reason" {
                Observation::Unknown { reason: key.into() }
            } else {
                Observation::Applied { receipt }
            };
            serde_json::to_string(&EffectReply {
                request_digest: digest(p).unwrap(),
                observation,
            })
            .unwrap()
            .replace("sandbox", r"\u0073andbox")
        });
        let result = HttpEffect::new(binding(&p, url))
            .unwrap()
            .execute_with_secret(&p, &Time(1000), key)
            .unwrap();
        assert!(matches!(result, Observation::Unknown { .. }));
        assert!(!serde_json::to_string(&result).unwrap().contains(key));
        server.join().unwrap();
    }
}
#[test]
fn unsafe_endpoint_or_changed_target_or_expired_call_is_refused_before_io() {
    let p = fixture();
    assert!(
        binding(&p, "https://example.com".into())
            .shared_principal()
            .is_err()
    );
    assert!(
        binding(&p, "http://127.0.0.1:1234".into())
            .shared_principal()
            .unwrap()
            .is_none()
    );
    for url in [
        "http://example.com",
        "http://localhost:1",
        "https://user:password@example.com",
        "https://example.com?token=secret",
        "https://example.com#fragment",
    ] {
        assert!(HttpEffect::new(binding(&p, url.into())).is_err());
    }
    let mut b = binding(&p, "https://example.com".into());
    b.target.id = "different".into();
    assert!(
        HttpEffect::new(b)
            .unwrap()
            .execute_with_secret(&p, &Time(1000), "sandbox-secret-value")
            .is_err()
    );
    let a = HttpEffect::new(binding(&p, "https://example.com".into())).unwrap();
    assert!(
        a.execute_with_secret(&p, &Time(5000), "sandbox-secret-value")
            .is_err()
    );
    let mut changed = p;
    changed.number = 0;
    assert!(
        a.execute_with_secret(&changed, &Time(1000), "sandbox-secret-value")
            .is_err()
    );
}
