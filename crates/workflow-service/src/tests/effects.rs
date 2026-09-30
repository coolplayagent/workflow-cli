use super::*;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use workflow_effect_http::{HttpEffect, HttpEffectBinding};
use workflow_effects::{
    CallKind, EffectAdapter, EffectAttempt, EffectReceipt, EffectReply, Observation,
};
use workflow_runstore_postgres::access::EffectRule;

fn effect_start() -> StartRun {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    serde_json::from_slice(&std::fs::read(root.join("examples/runs/effect-release.json")).unwrap())
        .unwrap()
}
struct Sandbox {
    endpoint: String,
    methods: Arc<Mutex<Vec<CallKind>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Sandbox {
    fn new() -> Self {
        // Separate provider state commits before returning its receipt. It is
        // deliberately outside the workflow API's transaction and process.
        db().batch_execute("CREATE TABLE IF NOT EXISTS public.workflow_test_effect_receipts (operation_key text PRIMARY KEY, intent_digest text NOT NULL, receipt text NOT NULL)").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let methods = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let calls = methods.clone();
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
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = vec![];
                let mut buffer = [0; 4096];
                let (offset, len) = loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(p) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..p])
                            .unwrap()
                            .to_ascii_lowercase();
                        assert!(headers.contains("authorization: bearer sandbox-effect-test"));
                        let len = headers
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .map(|n| n.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (p + 4, len);
                    }
                };
                while bytes.len() < offset + len {
                    let n = socket.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let attempt: EffectAttempt =
                    serde_json::from_slice(&bytes[offset..offset + len]).unwrap();
                attempt.validate().unwrap();
                let headers = std::str::from_utf8(&bytes[..offset]).unwrap();
                assert!(headers.starts_with(if attempt.kind == CallKind::Write {
                    "POST /write "
                } else {
                    "POST /query "
                }));
                let digest = workflow_effects::digest(&attempt.intent).unwrap();
                let mut provider = db();
                if attempt.kind == CallKind::Write {
                    let receipt = EffectReceipt {
                        operation_key: attempt.intent.operation_key.clone(),
                        intent_digest: digest.clone(),
                        target: attempt.intent.policy.target.clone(),
                        resource_id: "sandbox-release-1".into(),
                        provider_receipt: "sandbox-provider-commit-1".into(),
                        outputs: [("release_id".into(), json!("sandbox-release-1"))].into(),
                    };
                    provider.execute("INSERT INTO public.workflow_test_effect_receipts(operation_key,intent_digest,receipt) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[&attempt.intent.operation_key,&digest,&serde_json::to_string(&receipt).unwrap()]).unwrap();
                }
                let row=provider.query_opt("SELECT intent_digest,receipt FROM public.workflow_test_effect_receipts WHERE operation_key=$1", &[&attempt.intent.operation_key]).unwrap();
                let observation = if let Some(row) = row {
                    assert_eq!(row.get::<_, String>(0), digest);
                    Observation::Applied {
                        receipt: serde_json::from_str(row.get(1)).unwrap(),
                    }
                } else {
                    Observation::Absent
                };
                calls.lock().unwrap().push(attempt.kind);
                let body = serde_json::to_vec(&EffectReply {
                    request_digest: workflow_effects::digest(&attempt).unwrap(),
                    observation,
                })
                .unwrap();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        Self {
            endpoint,
            methods,
            stop,
            thread: Some(thread),
        }
    }
    fn binding(&self, start: &StartRun) -> HttpEffectBinding {
        let policy = &start.bundle.effect_bindings[0].policy;
        HttpEffectBinding {
            schema_version: 1,
            target: policy.target.clone(),
            call_identity: policy.call_identity.clone(),
            capability: start.bundle.capabilities[0].clone(),
            endpoint: self.endpoint.clone(),
            api_key_env: "WORKFLOW_EFFECT_TEST_SECRET".into(),
            allow_loopback_http: true,
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take()
            && !std::thread::panicking()
        {
            t.join().unwrap();
        }
    }
}
struct Adapter {
    http: HttpEffect,
    crash_marker: Option<PathBuf>,
}
impl EffectAdapter for Adapter {
    fn execute(
        &self,
        a: &EffectAttempt,
        c: &dyn workflow_worker::Clock,
    ) -> workflow_worker::Result<Observation> {
        let observation = self.http.execute_with_secret(a, c, "sandbox-effect-test")?;
        if let Some(path) = &self.crash_marker {
            std::fs::write(
                path,
                serde_json::to_vec(&json!({"attempt":a,"observation":observation})).unwrap(),
            )
            .unwrap();
            loop {
                std::thread::park();
            }
        }
        Ok(observation)
    }
}
pub(super) fn crashing_worker(s: &Value) {
    let client = RemoteClient::new(serde_json::from_value(s["binding"].clone()).unwrap()).unwrap();
    let adapter = Adapter {
        http: HttpEffect::new(serde_json::from_value(s["effect"].clone()).unwrap()).unwrap(),
        crash_marker: Some(s["ready"].as_str().unwrap().into()),
    };
    for _ in 0..1000 {
        work_effects_once(&client, &adapter, 100).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("worker never received effect");
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn https_effect_worker_crash_recovers_provider_receipt_without_duplicate_write() {
    let mut h = Harness::new(true);
    let provider = Sandbox::new();
    let mut start = effect_start();
    let tenant = format!(
        "effect-tls-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    );
    start.run_id = tenant.clone();
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let maintainer = credential(
        &mut service,
        &admin,
        "maintainer",
        Role::DefinitionMaintainer,
    );
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let scheduler = credential(&mut service, &admin, "scheduler", Role::Scheduler);
    let successor = credential(&mut service, &admin, "successor", Role::Scheduler);
    let cap = workflow_worker::Capability::new(start.bundle.capabilities[0].clone()).unwrap();
    let rule = CapabilityRule {
        model_policy: None,
        id: cap.descriptor().capability.id.clone(),
        version: cap.descriptor().capability.version.clone(),
        contract_digest: cap.digest().into(),
        artifacts: None,
        effect: Some(EffectRule {
            policy: start.bundle.effect_bindings[0].policy.clone(),
        }),
    };
    let first = service
        .issue(
            admin.expose_secret(),
            "first",
            Role::Worker,
            std::slice::from_ref(&rule),
            3600000,
        )
        .unwrap();
    let second = service
        .issue(
            admin.expose_secret(),
            "second",
            Role::Worker,
            &[rule],
            3600000,
        )
        .unwrap();
    let (_, maintainer_client) = h.client(&maintainer);
    let (_, runner_client) = h.client(&runner);
    let (scheduler_binding, _) = h.client(&scheduler);
    let (_, successor_client) = h.client(&successor);
    let (worker_binding, old_client) = h.client(&first);
    let (_, worker_client) = h.client(&second);
    call(
        &maintainer_client,
        Operation::Publish {
            bundle: Box::new(start.bundle.clone()),
        },
    )
    .unwrap();
    call(
        &runner_client,
        Operation::Start {
            request: Box::new(start.clone()),
        },
    )
    .unwrap();
    let scheduler_child=h.spawn(json!({"kind":"scheduler","effects":true,"binding":scheduler_binding,"workers":[first.id],"id":"effect-owner","ttl":8000,"stop":h.dir.join("stop"),"ready":h.dir.join("scheduled")}));
    h.wait_file("scheduled");
    let Response::Pending(pending) = call(
        &old_client,
        Operation::PendingEffects {
            after: String::new(),
            limit: 100,
        },
    )
    .unwrap() else {
        panic!("pending")
    };
    let assignment = pending.items[0].clone();
    let worker_child=h.spawn(json!({"kind":"effect_worker_crash","binding":worker_binding,"effect":provider.binding(&start),"ready":h.dir.join("provider-committed")}));
    h.wait_file("provider-committed");
    let observed: Value =
        serde_json::from_slice(&std::fs::read(h.dir.join("provider-committed")).unwrap()).unwrap();
    for child in [worker_child, scheduler_child] {
        h.children[child].kill().unwrap();
        assert!(!h.children[child].wait().unwrap().success());
    }
    let states = service
        .effects(runner.expose_secret(), &start.run_id, 0, 100)
        .unwrap();
    assert_eq!(states.items.len(), 1);
    assert!(states.items[0].calls[0].observation.is_none());
    let key = states.items[0].intent.operation_key.clone();
    let began = Instant::now();
    let mut scheduler = Scheduler::new("successor", vec![second.id.clone()])
        .unwrap()
        .with_effects();
    loop {
        let report = scheduler.step(&successor_client, 60000, 100).unwrap();
        if report.dispatched > 0 {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(30));
    }
    let adapter = Adapter {
        http: HttpEffect::new(provider.binding(&start)).unwrap(),
        crash_marker: None,
    };
    assert_eq!(
        work_effects_once(&worker_client, &adapter, 100)
            .unwrap()
            .completed,
        1
    );
    assert_eq!(
        work_effects_once(&worker_client, &adapter, 100)
            .unwrap()
            .completed,
        0
    );
    scheduler.step(&successor_client, 60000, 100).unwrap();
    let final_state = service.get(runner.expose_secret(), &start.run_id).unwrap();
    assert_eq!(final_state.status, RunStatus::Succeeded);
    assert_eq!(
        *provider.methods.lock().unwrap(),
        vec![CallKind::Write, CallKind::Query]
    );
    let count: i64 = db()
        .query_one(
            "SELECT count(*) FROM public.workflow_test_effect_receipts WHERE operation_key=$1",
            &[&key],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
    assert_eq!(
        call(
            &old_client,
            Operation::ObserveEffect {
                assignment_id: assignment,
                observation: Box::new(
                    serde_json::from_value(observed["observation"].clone()).unwrap()
                )
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::LeaseConflict
    );
    let recovered = AuthenticatedService::open(db())
        .unwrap()
        .get(runner.expose_secret(), &start.run_id)
        .unwrap();
    assert_eq!(recovered, final_state);
    let ledger = service
        .effects(runner.expose_secret(), &start.run_id, 0, 100)
        .unwrap();
    assert_eq!(ledger.items[0].calls.len(), 2);
    assert_eq!(
        ledger.items[0].calls[0].attempt.intent,
        ledger.items[0].calls[1].attempt.intent
    );
    assert!(ledger.items[0].calls[1].attempt.epoch > ledger.items[0].calls[0].attempt.epoch);
    for entry in std::fs::read_dir(&h.dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".log"))
    {
        let text = std::fs::read_to_string(entry.path()).unwrap();
        assert!(!text.contains(first.expose_secret()));
        assert!(!text.contains(second.expose_secret()));
    }
    println!(
        "remote_effect_crash: recovery_ms={} lease_ms=8000 provider_writes=1 provider_queries=1",
        began.elapsed().as_millis()
    );
}
