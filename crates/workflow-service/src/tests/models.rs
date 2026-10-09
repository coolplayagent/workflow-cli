use super::*;
use std::{
    io::{Read, Write},
    sync::{Arc, atomic::AtomicBool},
};
use workflow_model_http::{HttpBinding, HttpModel, Provider};
use workflow_models::{
    Action, ModelAdapter, ModelCall, ModelCapability, ModelEvent, ModelIdentity, ModelReply,
    Policy, Proposal, Reply as ModelReplyEnvelope, ToolReply, Usage,
};
use workflow_worker::{AdapterOutcome, Capability, Clock, SystemClock};

enum TestTransport {
    Local(Box<InProcessTransport>),
    Remote(RemoteClient),
}
impl TestTransport {
    fn new(binding: ClientBinding, local: bool) -> Self {
        if local {
            Self::Local(Box::new(InProcessTransport::new(
                AuthenticatedService::open(db()).unwrap(),
                binding.credential,
            )))
        } else {
            Self::Remote(RemoteClient::new(binding).unwrap())
        }
    }
}
impl TaskTransport for TestTransport {
    fn call(&self, request: &Request) -> Result<Response> {
        match self {
            Self::Local(c) => c.call(request),
            Self::Remote(c) => c.call(request),
        }
    }
}
fn client(
    h: &Harness,
    credential: &IssuedCredential,
    local: bool,
) -> (ClientBinding, TestTransport) {
    let (binding, _) = h.client(credential);
    let transport = TestTransport::new(binding.clone(), local);
    (binding, transport)
}

fn model_start() -> StartRun {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    serde_json::from_slice(&std::fs::read(root.join("examples/models/start.json")).unwrap())
        .unwrap()
}
struct FixtureModel;
impl ModelAdapter for FixtureModel {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            adapter: workflow_ir::VersionRef {
                id: "fixture.model".into(),
                version: "1.0.0".into(),
            },
            model: "deterministic".into(),
            binding_digest: workflow_worker::digest(&"deterministic").unwrap(),
        }
    }
    fn complete(&self, call: &ModelCall) -> ModelReplyEnvelope {
        let action = match call.events.last() {
            Some(ModelEvent::ToolFinished {
                response: ToolReply::Received { result },
                ..
            }) => {
                let AdapterOutcome::Succeeded { outputs, .. } = &result.outcome else {
                    panic!("fixture validation failed")
                };
                Action::Complete {
                    outputs: outputs.clone(),
                    summary: "Return the observed compiler result".into(),
                }
            }
            _ => Action::Call {
                capability: call.policy.tools[0].capability.clone(),
                inputs: call.inputs.clone(),
                summary: "Inspect the current definition".into(),
            },
        };
        ModelReplyEnvelope::Received {
            reply: ModelReply {
                proposal: Proposal {
                    protocol_version: 1,
                    action,
                },
                resolved_model: "deterministic-v1".into(),
                response_id: "fixture-response".into(),
                usage: Usage {
                    input_tokens: None,
                    output_tokens: None,
                },
            },
        }
    }
}
struct WireJournal {
    binding: ClientBinding,
    assignment_id: String,
}
impl workflow_models::SessionJournal for WireJournal {
    fn load(
        &self,
        _: &workflow_worker::WorkRequest,
    ) -> workflow_worker::Result<Option<workflow_models::ModelCheckpoint>> {
        let response = call(
            &RemoteClient::new(self.binding.clone()).unwrap(),
            Operation::LoadModelCheckpoint {
                assignment_id: self.assignment_id.clone(),
            },
        )
        .map_err(|e| {
            workflow_worker::Error::new(workflow_worker::ErrorCode::InvalidResult, e.message)
        })?;
        let Response::ModelCheckpoint(checkpoint) = response else {
            panic!("checkpoint response")
        };
        Ok(checkpoint.map(|c| *c))
    }
    fn save(
        &self,
        _: &workflow_worker::WorkRequest,
        previous: Option<&workflow_models::ModelCheckpoint>,
        next: &workflow_models::ModelCheckpoint,
    ) -> workflow_worker::Result<()> {
        let previous_digest = previous.map(workflow_worker::digest).transpose()?;
        let response = call(
            &RemoteClient::new(self.binding.clone()).unwrap(),
            Operation::SaveModelCheckpoint {
                assignment_id: self.assignment_id.clone(),
                previous_digest,
                checkpoint: Box::new(next.clone()),
            },
        )
        .map_err(|e| {
            workflow_worker::Error::new(workflow_worker::ErrorCode::InvalidResult, e.message)
        })?;
        assert!(matches!(response, Response::Unit));
        Ok(())
    }
}
pub(super) fn model_worker(spec: &Value) {
    let client = TestTransport::new(
        serde_json::from_value(spec["binding"].clone()).unwrap(),
        spec["local"].as_bool().unwrap(),
    );
    let policy = Policy::new(serde_json::from_value(spec["policy"].clone()).unwrap()).unwrap();
    let adapter: Arc<dyn ModelAdapter> = if spec["http"].is_null() {
        Arc::new(FixtureModel)
    } else {
        Arc::new(HttpModel::new(serde_json::from_value(spec["http"].clone()).unwrap()).unwrap())
    };
    let mut worker = workflow_builtin_capabilities::worker().unwrap();
    worker
        .register(
            ModelCapability::new(
                policy,
                adapter,
                workflow_builtin_capabilities::worker().unwrap(),
            )
            .unwrap()
            .with_journal(Arc::new(WireJournal {
                binding: serde_json::from_value(spec["binding"].clone()).unwrap(),
                assignment_id: spec["assignment"].as_str().unwrap().into(),
            })),
        )
        .unwrap();
    let report = work_once(&client, &worker, 100).unwrap();
    assert_eq!(report.completed, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.fenced, 0);
}

struct WireProvider {
    binding: HttpBinding,
    calls: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl WireProvider {
    fn new(provider: Provider, mode: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let binding = HttpBinding {
            schema_version: 1,
            provider: provider.clone(),
            model: "sandbox-model".into(),
            endpoint: format!("http://{}/model", listener.local_addr().unwrap()),
            credential: None,
            api_key_env: "WORKFLOW_MODEL_FIXTURE_KEY".into(),
            allow_loopback_http: true,
        };
        let calls = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let count = calls.clone();
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut bytes = vec![];
                let mut buffer = [0; 4096];
                let (offset, len) = loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(p) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..p])
                            .unwrap()
                            .to_ascii_lowercase();
                        assert!(headers.contains(match provider {
                            Provider::OpenaiResponses => "authorization: bearer sandbox-model-key",
                            Provider::AnthropicMessages => "x-api-key: sandbox-model-key",
                        }));
                        let len: usize = headers
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .map(|n| n.trim().parse().unwrap())
                            })
                            .unwrap();
                        assert!(len <= 262144);
                        break (p + 4, len);
                    }
                };
                while bytes.len() < offset + len {
                    let n = socket.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                count.fetch_add(1, Ordering::SeqCst);
                let body: Value = serde_json::from_slice(&bytes[offset..offset + len]).unwrap();
                assert!(!body.to_string().contains("sandbox-model-key"));
                let context: Value = serde_json::from_str(match provider {
                    Provider::OpenaiResponses => body["input"].as_str().unwrap(),
                    Provider::AnthropicMessages => body["messages"][0]["content"].as_str().unwrap(),
                })
                .unwrap();
                let events = context["events"].as_array().unwrap();
                let action = if mode == "invalid" {
                    json!({"type":"cancel","run_id":"another-run"})
                } else if let Some(last) = events.last() {
                    assert_eq!(last["type"], "tool_finished");
                    json!({"type":"complete","outputs":last["response"]["result"]["outcome"]["outputs"],"summary":"Return compiler result"})
                } else {
                    json!({"type":"call","capability":context["policy"]["tools"][0]["capability"],"inputs":context["inputs"],"summary":"Inspect definition"})
                };
                let proposal = json!({"protocol_version":1,"action":action}).to_string();
                let reply = match provider {
                    Provider::OpenaiResponses => {
                        json!({"id":"resp_sandbox","model":"sandbox-v1","status":"completed","output":[{"type":"reasoning","summary":["hidden-reasoning-fixture"]},{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":proposal}]}],"usage":{"input_tokens":31,"output_tokens":23}})
                    }
                    Provider::AnthropicMessages => {
                        json!({"id":"msg_sandbox","model":"sandbox-v1","type":"message","role":"assistant","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"hidden-reasoning-fixture"},{"type":"text","text":proposal}],"usage":{"input_tokens":31,"output_tokens":23}})
                    }
                };
                let (status, reply) = if mode == "unavailable" {
                    ("503 Unavailable", "sandbox-model-key".into())
                } else {
                    ("200 OK", reply.to_string())
                };
                write!(socket,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).unwrap();
            }
        });
        Self {
            binding,
            calls,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for WireProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take()
            && !std::thread::panicking()
        {
            t.join().unwrap();
        }
    }
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn https_model_bindings_records_failures_and_policy_authority_contract() {
    for local in [true, false] {
        model_transport_contract(local);
    }
}
fn model_transport_contract(in_process: bool) {
    let mut h = Harness::new(true);
    let template = model_start();
    let policy = Policy::new(template.bundle.model_policies[0].clone()).unwrap();
    let cap = Capability::new(policy.spec().task.clone()).unwrap();
    let tenant = format!(
        "model-tls-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let author = credential(&mut service, &admin, "author", Role::DefinitionMaintainer);
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let scheduler = credential(&mut service, &admin, "scheduler", Role::Scheduler);
    let rule = CapabilityRule {
        id: cap.descriptor().capability.id.clone(),
        version: cap.descriptor().capability.version.clone(),
        contract_digest: cap.digest().into(),
        artifacts: None,
        effect: None,
        model_policy: Some(policy.binding().clone()),
    };
    let worker = service
        .issue(
            admin.expose_secret(),
            "model-worker",
            Role::Worker,
            std::slice::from_ref(&rule),
            3600000,
        )
        .unwrap();
    let mut missing = rule.clone();
    missing.model_policy = None;
    let legacy = service
        .issue(
            admin.expose_secret(),
            "legacy",
            Role::Worker,
            &[missing],
            3600000,
        )
        .unwrap();
    let mut different = rule.clone();
    different.model_policy.as_mut().unwrap().digest =
        workflow_worker::digest(&"another policy").unwrap();
    let wrong = service
        .issue(
            admin.expose_secret(),
            "wrong-policy",
            Role::Worker,
            &[different],
            3600000,
        )
        .unwrap();
    let (_, author_client) = client(&h, &author, in_process);
    let (_, runner_client) = client(&h, &runner, in_process);
    let (_, scheduler_client) = client(&h, &scheduler, in_process);
    let (worker_binding, worker_client) = client(&h, &worker, in_process);
    let (_, legacy_client) = client(&h, &legacy, in_process);
    assert_eq!(
        worker_client
            .call(&Request {
                protocol_version: 999,
                request_id: "unsupported".into(),
                operation: Operation::Pending {
                    after: String::new(),
                    limit: 10
                },
            })
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest,
    );
    call(
        &author_client,
        Operation::Publish {
            bundle: Box::new(template.bundle.clone()),
        },
    )
    .unwrap();
    let cases = [
        (None, "success"),
        (Some(Provider::OpenaiResponses), "success"),
        (Some(Provider::AnthropicMessages), "success"),
        (Some(Provider::OpenaiResponses), "unavailable"),
        (Some(Provider::AnthropicMessages), "invalid"),
    ];
    let mut expected_outputs = None;
    let mut committed: Vec<(String, workflow_kernel::Snapshot)> = vec![];
    for (i, (provider, mode)) in cases.into_iter().enumerate() {
        let gateway = provider.map(|p| WireProvider::new(p, mode));
        let mut start = template.clone();
        start.run_id = format!("model-{i}");
        call(
            &runner_client,
            Operation::Start {
                request: Box::new(start.clone()),
            },
        )
        .unwrap();
        let Response::Lease(lease) = call(
            &scheduler_client,
            Operation::Acquire {
                run_id: start.run_id.clone(),
                acquisition_id: format!("claim-{i}"),
                ttl_ms: 120000,
            },
        )
        .unwrap() else {
            panic!("lease")
        };
        for denied in [&legacy, &wrong] {
            let before: String = db().query_one("SELECT image_digest FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2", &[&tenant,&start.run_id]).unwrap().get(0);
            assert_eq!(
                call(
                    &scheduler_client,
                    Operation::Dispatch {
                        lease: lease.clone(),
                        worker_id: denied.id.clone()
                    }
                )
                .unwrap_err()
                .code,
                ErrorCode::Unauthorized
            );
            let after: String = db().query_one("SELECT image_digest FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2", &[&tenant,&start.run_id]).unwrap().get(0);
            assert_eq!(before, after);
        }
        let Response::Dispatch(workflow_runstore_postgres::access::Dispatch::Task {
            assignment_id,
        }) = call(
            &scheduler_client,
            Operation::Dispatch {
                lease: lease.clone(),
                worker_id: worker.id.clone(),
            },
        )
        .unwrap()
        else {
            panic!("assignment")
        };
        assert_eq!(
            call(
                &legacy_client,
                Operation::Assignment {
                    assignment_id: assignment_id.clone()
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::Unauthorized
        );
        assert_eq!(
            call(
                &legacy_client,
                Operation::LoadModelCheckpoint {
                    assignment_id: assignment_id.clone()
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::Unauthorized
        );
        let Response::Assignment(task) = call(
            &worker_client,
            Operation::Assignment {
                assignment_id: assignment_id.clone(),
            },
        )
        .unwrap() else {
            panic!("task")
        };
        assert_eq!(task.request.model_policy.as_ref(), Some(policy.binding()));
        let mut local = workflow_builtin_capabilities::worker().unwrap();
        local
            .register(
                ModelCapability::new(
                    policy.clone(),
                    Arc::new(FixtureModel),
                    workflow_builtin_capabilities::worker().unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let local_result = local
            .execute(&task.request, &task.grant)
            .unwrap()
            .into_result();
        workflow_models::verify_result(&policy, &task.request, &local_result).unwrap();
        let forged = workflow_worker::WorkResult {
            protocol_version: 2,
            request_digest: workflow_worker::digest(&task.request).unwrap(),
            completed_at_unix_ms: SystemClock.now_unix_ms().unwrap(),
            outcome: AdapterOutcome::Succeeded {
                outputs: [
                    ("valid".into(), json!(true)),
                    ("diagnostics".into(), json!([])),
                ]
                .into(),
                evidence: vec![],
            },
            model_record: None,
        };
        assert!(
            call(
                &worker_client,
                Operation::Finish {
                    assignment_id: assignment_id.clone(),
                    result: Box::new(forged)
                }
            )
            .is_err()
        );
        let child = h.spawn(json!({"kind":"model_worker","assignment":assignment_id,"local":in_process,"binding":worker_binding,"policy":policy.spec(),"http":gateway.as_ref().map(|g| &g.binding)}));
        h.wait_child(child);
        let snapshot = service.get(runner.expose_secret(), &start.run_id).unwrap();
        assert_eq!(
            snapshot.status,
            if mode == "success" {
                RunStatus::Succeeded
            } else {
                RunStatus::Failed
            }
        );
        if mode == "success" {
            let outputs = snapshot.frames[&1].nodes["inspect"].outputs.clone();
            let AdapterOutcome::Succeeded {
                outputs: local_outputs,
                ..
            } = local_result.outcome
            else {
                panic!("local contract")
            };
            assert_eq!(outputs, local_outputs);
            if let Some(expected) = &expected_outputs {
                assert_eq!(&outputs, expected);
            } else {
                expected_outputs = Some(outputs);
            }
        }
        let count = gateway.as_ref().map(|g| g.calls.load(Ordering::SeqCst));
        assert_eq!(
            count,
            gateway
                .as_ref()
                .map(|_| if mode == "success" { 2 } else { 1 })
        );
        drop(service);
        service = AuthenticatedService::open(db()).unwrap();
        assert_eq!(
            service.get(runner.expose_secret(), &start.run_id).unwrap(),
            snapshot
        );
        for (run_id, prior) in &committed {
            assert_eq!(service.get(runner.expose_secret(), run_id).unwrap(), *prior);
        }
        committed.push((start.run_id.clone(), snapshot));
        if let Some(gateway) = &gateway {
            assert_eq!(gateway.calls.load(Ordering::SeqCst), count.unwrap());
        }
        let image: Vec<u8> = db().query_one("SELECT image FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2", &[&tenant,&start.run_id]).unwrap().get(0);
        let text = String::from_utf8_lossy(&image);
        for secret in [
            "sandbox-model-key",
            "hidden-reasoning-fixture",
            worker.expose_secret(),
        ] {
            assert!(!text.contains(secret));
        }
    }
    // Model provider failure cannot disable standalone deterministic capability
    // invocation; it uses the exact descriptor used in the model's tool loop.
    let builtin = workflow_builtin_capabilities::worker().unwrap();
    let tool = Capability::new(policy.spec().tools[0].clone()).unwrap();
    let now = SystemClock.now_unix_ms().unwrap();
    let request = workflow_worker::WorkRequest::standalone(
        &tool,
        template.inputs.clone(),
        workflow_worker::RequestContext {
            request_id: "standalone-after-provider-failure".into(),
            trace_id: "independent-capability".into(),
            issued_at_unix_ms: now,
            deadline_unix_ms: now + 60000,
        },
    )
    .unwrap();
    let result = builtin
        .execute(
            &request,
            &workflow_worker::ExecutionGrant::bind(&request).unwrap(),
        )
        .unwrap()
        .into_result();
    let AdapterOutcome::Succeeded { outputs, .. } = result.outcome else {
        panic!("provider failure escaped model boundary")
    };
    assert_eq!(Some(outputs), expected_outputs);
    service.revoke(admin.expose_secret(), &worker.id).unwrap();
    assert_eq!(
        call(
            &worker_client,
            Operation::Pending {
                after: String::new(),
                limit: 10
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Unauthorized
    );
}
