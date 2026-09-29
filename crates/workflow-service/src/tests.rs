use super::*;
mod artifacts;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use workflow_runstore::*;
use workflow_runstore_postgres::access::{
    AuthenticatedService, CapabilityRule, IssuedCredential, Role,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn private(path: &Path, bytes: &[u8]) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}
fn fixture(id: &str) -> StartRun {
    let root = if let Ok(r) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    };
    let mut r: StartRun = serde_json::from_slice(
        &std::fs::read(root.join("examples/execution/offline-start.json")).unwrap(),
    )
    .unwrap();
    r.run_id = id.into();
    r
}
fn db() -> postgres::Client {
    postgres::Client::connect(
        &std::env::var("WORKFLOW_TEST_POSTGRES").expect("disposable database required"),
        postgres::NoTls,
    )
    .unwrap()
}
fn call(c: &RemoteClient, operation: Operation) -> Result<Response> {
    c.call(&Request {
        protocol_version: 1,
        request_id: "test".into(),
        operation,
    })
}
struct Harness {
    dir: PathBuf,
    children: Vec<Child>,
    endpoint: String,
}
impl Harness {
    fn new(use_database: bool) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!(
            "workflow-tls-{}-{}-{}",
            std::process::id(),
            workflow_worker::Clock::now_unix_ms(&workflow_worker::SystemClock).unwrap(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(dir.join("cert.pem"), certified.cert.pem()).unwrap();
        private(
            &dir.join("key.pem"),
            certified.signing_key.serialize_pem().as_bytes(),
        );
        private(
            &dir.join("database"),
            if use_database {
                std::env::var("WORKFLOW_TEST_POSTGRES").unwrap()
            } else {
                "host=127.0.0.1 port=1 user=nobody connect_timeout=1".into()
            }
            .as_bytes(),
        );
        let binding = ServerBinding {
            listen: "127.0.0.1:0".parse().unwrap(),
            certificate_file: dir.join("cert.pem"),
            private_key: SecretRef::File {
                path: dir.join("key.pem"),
            },
            database: DatabaseBinding {
                connection: SecretRef::File {
                    path: dir.join("database"),
                },
                transport: DatabaseTransport::Local,
            },
            max_connections: 16,
            max_operations: 8,
        };
        std::fs::write(
            dir.join("server.json"),
            serde_json::to_vec(&binding).unwrap(),
        )
        .unwrap();
        let mut h = Self {
            dir,
            children: vec![],
            endpoint: String::new(),
        };
        h.spawn(json!({"kind":"server","config":h.dir.join("server.json"),"ready":h.dir.join("server.ready")}));
        h.wait_file("server.ready");
        let address: Value =
            serde_json::from_slice(&std::fs::read(h.dir.join("server.ready")).unwrap()).unwrap();
        let port = address["port"].as_u64().unwrap();
        h.endpoint = format!("https://localhost:{port}/v1/operations");
        h
    }
    fn spawn(&mut self, spec: Value) -> usize {
        let i = self.children.len();
        let log = std::fs::File::create(self.dir.join(format!("child-{i}.log"))).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::service_child", "--nocapture"])
            .env("WORKFLOW_SERVICE_CHILD", spec.to_string())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        self.children.push(child);
        i
    }
    fn wait_file(&mut self, name: &str) {
        let until = Instant::now() + Duration::from_secs(90);
        while !self.dir.join(name).exists() {
            for (i, c) in self.children.iter_mut().enumerate() {
                if let Some(s) = c.try_wait().unwrap() {
                    assert!(
                        s.success(),
                        "child {i} failed: {}",
                        std::fs::read_to_string(self.dir.join(format!("child-{i}.log"))).unwrap()
                    );
                }
            }
            assert!(Instant::now() < until, "missing {name}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn client(&self, credential: &IssuedCredential) -> (ClientBinding, RemoteClient) {
        let path = self.dir.join(&credential.id);
        write_credential(&path, credential).unwrap();
        let binding = ClientBinding {
            endpoint: self.endpoint.clone(),
            ca_file: self.dir.join("cert.pem"),
            credential: SecretRef::File { path },
            timeout_ms: 60000,
        };
        let client = RemoteClient::new(binding.clone()).unwrap();
        (binding, client)
    }
    fn wait_child(&mut self, i: usize) {
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = self.children[i].try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{}",
                    std::fs::read_to_string(self.dir.join(format!("child-{i}.log"))).unwrap()
                );
                return;
            }
            assert!(Instant::now() < until, "child {i} did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn service_child() {
    let Ok(spec) = std::env::var("WORKFLOW_SERVICE_CHILD") else {
        return;
    };
    let s: Value = serde_json::from_str(&spec).unwrap();
    match s["kind"].as_str().unwrap() {
        "server" => {
            let binding: ServerBinding =
                serde_json::from_slice(&std::fs::read(s["config"].as_str().unwrap()).unwrap())
                    .unwrap();
            serve_foreground(binding, |address| {
                std::fs::write(
                    s["ready"].as_str().unwrap(),
                    json!({"port":address.port()}).to_string(),
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        }
        "scheduler" => {
            let binding: ClientBinding = serde_json::from_value(s["binding"].clone()).unwrap();
            let client = RemoteClient::new(binding).unwrap();
            let workers = serde_json::from_value(s["workers"].clone()).unwrap();
            let mut scheduler = Scheduler::new(s["id"].as_str().unwrap(), workers).unwrap();
            for _ in 0..500 {
                if Path::new(s["stop"].as_str().unwrap()).exists() {
                    break;
                }
                let report = scheduler
                    .step(&client, s["ttl"].as_u64().unwrap(), 100)
                    .unwrap();
                if report.dispatched > 0 {
                    std::fs::write(s["ready"].as_str().unwrap(), b"dispatched").unwrap();
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        }
        "worker" => {
            let client =
                RemoteClient::new(serde_json::from_value(s["binding"].clone()).unwrap()).unwrap();
            let worker = workflow_builtin_capabilities::worker().unwrap();
            let mut completed = 0;
            for _ in 0..1000 {
                if Path::new(s["stop"].as_str().unwrap()).exists() {
                    break;
                }
                completed += work_once(&client, &worker, 100).unwrap().completed;
                std::thread::sleep(Duration::from_millis(40));
            }
            std::fs::write(
                s["out"].as_str().unwrap(),
                json!({"completed":completed}).to_string(),
            )
            .unwrap();
        }
        "partial_uploader" => {
            let client =
                RemoteClient::new(serde_json::from_value(s["binding"].clone()).unwrap()).unwrap();
            let request = serde_json::from_value(s["request"].clone()).unwrap();
            let content = std::fs::read(s["content"].as_str().unwrap()).unwrap();
            let Response::ArtifactUpload(upload) = call(
                &client,
                Operation::ArtifactBegin {
                    request: Box::new(request),
                },
            )
            .unwrap() else {
                panic!("upload")
            };
            call(
                &client,
                Operation::ArtifactPut {
                    upload_id: upload.upload_id,
                    offset: 0,
                    content: content[..65536].to_vec(),
                },
            )
            .unwrap();
            std::fs::write(s["ready"].as_str().unwrap(), b"durable first chunk").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "held_worker" => {
            let client =
                RemoteClient::new(serde_json::from_value(s["binding"].clone()).unwrap()).unwrap();
            let until = Instant::now() + Duration::from_secs(120);
            let assignment = loop {
                let Response::Pending(p) = call(
                    &client,
                    Operation::Pending {
                        after: String::new(),
                        limit: 100,
                    },
                )
                .unwrap() else {
                    panic!()
                };
                if let Some(id) = p.items.first() {
                    break id.clone();
                }
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(20));
            };
            let Response::Assignment(task) = call(
                &client,
                Operation::Assignment {
                    assignment_id: assignment.clone(),
                },
            )
            .unwrap() else {
                panic!()
            };
            let result = workflow_builtin_capabilities::worker()
                .unwrap()
                .execute(&task.request, &task.grant)
                .unwrap()
                .into_result();
            std::fs::write(
                s["ready"].as_str().unwrap(),
                json!({"assignment":assignment,"epoch":task.epoch}).to_string(),
            )
            .unwrap();
            while !Path::new(s["resume"].as_str().unwrap()).exists() {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(10));
            }
            let result = call(
                &client,
                Operation::Finish {
                    assignment_id: assignment,
                    result: Box::new(result),
                },
            );
            std::fs::write(
                s["out"].as_str().unwrap(),
                serde_json::to_vec(&result).unwrap(),
            )
            .unwrap();
        }
        _ => panic!("invalid child mode"),
    }
}
#[test]
fn strict_protocol_tls_and_private_secret_contract() {
    let mut h = Harness::new(false);
    let token = format!("wf1_{}", "0".repeat(64));
    private(&h.dir.join("token"), token.as_bytes());
    let binding = ClientBinding {
        endpoint: h.endpoint.clone(),
        ca_file: h.dir.join("cert.pem"),
        credential: SecretRef::File {
            path: h.dir.join("token"),
        },
        timeout_ms: 10000,
    };
    let client = RemoteClient::new(binding.clone()).unwrap();
    // Valid TLS reaches the deliberately unavailable database; errors omit secrets.
    let e = call(
        &client,
        Operation::Get {
            run_id: "absent".into(),
        },
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Storage);
    assert!(!e.message.contains(&token));
    let mut bad = binding.clone();
    bad.endpoint = bad.endpoint.replace("localhost", "127.0.0.1");
    assert!(
        call(
            &RemoteClient::new(bad).unwrap(),
            Operation::Get {
                run_id: "absent".into()
            }
        )
        .is_err()
    );
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    std::fs::write(h.dir.join("other.pem"), other.cert.pem()).unwrap();
    let mut bad = binding.clone();
    bad.ca_file = h.dir.join("other.pem");
    assert!(
        call(
            &RemoteClient::new(bad).unwrap(),
            Operation::Get {
                run_id: "absent".into()
            }
        )
        .is_err()
    );
    for endpoint in [
        "http://localhost/v1/operations",
        "https://user:password@localhost/v1/operations",
        "https://localhost/v1/operations?token=secret",
    ] {
        let mut b = binding.clone();
        b.endpoint = endpoint.into();
        assert!(RemoteClient::new(b).is_err());
    }
    let tls = rustls::ClientConfig::builder_with_provider(binding::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(binding::roots(&h.dir.join("cert.pem")).unwrap())
        .with_no_client_auth();
    let raw = reqwest::blocking::Client::builder()
        .use_preconfigured_tls(tls)
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let send = |body: Vec<u8>| {
        raw.post(&h.endpoint)
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .unwrap()
    };
    for body in [
        json!({"protocol_version":99,"request_id":"test","operation":{"type":"get","run_id":"x"}}),
        json!({"protocol_version":1,"request_id":"test","operation":{"type":"get","run_id":"x","tenant":"forged"}}),
        json!({"protocol_version":1,"request_id":"test","operation":{"type":"apply","event":{}}}),
    ] {
        assert_eq!(send(serde_json::to_vec(&body).unwrap()).status(), 400);
    }
    assert_eq!(send(vec![b'x'; MAX_REQUEST_BYTES + 1]).status(), 413);
    assert_eq!(
        raw.post(&h.endpoint).body("{}").send().unwrap().status(),
        401
    );
    assert!(!format!("{:?}", binding.credential.resolve().unwrap()).contains(&token));
    use std::os::unix::fs::{PermissionsExt, symlink};
    std::fs::set_permissions(h.dir.join("token"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(binding.credential.resolve().is_err());
    symlink(h.dir.join("key.pem"), h.dir.join("key-link")).unwrap();
    assert!(read_private(&h.dir.join("key-link"), 65536).is_err());
    let fifo = h.dir.join("input-fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated path owned by this fixture.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(read_bounded(&fifo, 1024).is_err());
    assert!(read_private(&fifo, 1024).is_err());
    let output = h.dir.join("private-output");
    write_private_output(&output, b"validated bytes").unwrap();
    assert_eq!(
        std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(write_private_output(&output, b"replacement").is_err());
    assert_eq!(std::fs::read(output).unwrap(), b"validated bytes");
    // Graceful termination of the real TLS server process.
    unsafe { libc::kill(h.children[0].id() as i32, libc::SIGTERM) };
    h.wait_child(0);
}

fn credential(
    service: &mut AuthenticatedService,
    admin: &IssuedCredential,
    actor: &str,
    role: Role,
) -> IssuedCredential {
    let capabilities = if role == Role::Worker {
        fixture("x")
            .bundle
            .capabilities
            .into_iter()
            .map(|d| {
                let c = workflow_worker::Capability::new(d).unwrap();
                CapabilityRule {
                    id: c.descriptor().capability.id.clone(),
                    version: c.descriptor().capability.version.clone(),
                    contract_digest: c.digest().into(),
                    artifacts: None,
                }
            })
            .collect()
    } else {
        vec![]
    };
    service
        .issue(admin.expose_secret(), actor, role, &capabilities, 3600000)
        .unwrap()
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn tls_two_schedulers_three_workers_owner_kill_and_stale_result_contract() {
    let mut h = Harness::new(true);
    let tenant = format!(
        "remote-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let author = credential(&mut service, &admin, "author", Role::DefinitionMaintainer);
    let approver = credential(&mut service, &admin, "approver", Role::Approver);
    let sa = credential(&mut service, &admin, "scheduler-a", Role::Scheduler);
    let sb = credential(&mut service, &admin, "scheduler-b", Role::Scheduler);
    let workers: Vec<_> = (0..3)
        .map(|i| credential(&mut service, &admin, &format!("worker-{i}"), Role::Worker))
        .collect();
    let (runner_binding, runner_client) = h.client(&runner);
    let (_, author_client) = h.client(&author);
    let (_, approver_client) = h.client(&approver);
    let (sa_binding, _) = h.client(&sa);
    let (sb_binding, _) = h.client(&sb);
    let worker_clients: Vec<_> = workers.iter().map(|w| h.client(w)).collect();
    let request = fixture("fault-run");
    call(
        &author_client,
        Operation::Publish {
            bundle: Box::new(request.bundle.clone()),
        },
    )
    .unwrap();
    call(
        &runner_client,
        Operation::Start {
            request: Box::new(request.clone()),
        },
    )
    .unwrap();
    assert!(
        matches!(call(&runner_client,Operation::Runs{after:None,limit:100}).unwrap(),Response::Runs(p) if p.items.len()==1)
    );
    assert!(
        call(
            &worker_clients[0].1,
            Operation::Get {
                run_id: request.run_id.clone()
            }
        )
        .is_err()
    );
    let owner=h.spawn(json!({"kind":"scheduler","id":"owner","binding":sa_binding,"workers":[workers[0].id],"ttl":5000,"ready":h.dir.join("owner.ready"),"stop":h.dir.join("stop")}));
    let held=h.spawn(json!({"kind":"held_worker","binding":worker_clients[0].0,"ready":h.dir.join("held.ready"),"resume":h.dir.join("resume"),"out":h.dir.join("held.out")}));
    h.wait_file("held.ready");
    let foreign_admin = AuthenticatedService::bootstrap(
        &mut db(),
        &format!("{tenant}-foreign"),
        "project",
        "admin",
        3600000,
    )
    .unwrap();
    let foreign_viewer = credential(&mut service, &foreign_admin, "viewer", Role::Viewer);
    let foreign_worker = credential(&mut service, &foreign_admin, "worker", Role::Worker);
    let (_, foreign_viewer_client) = h.client(&foreign_viewer);
    let (_, foreign_worker_client) = h.client(&foreign_worker);
    for operation in [
        Operation::Get {
            run_id: request.run_id.clone(),
        },
        Operation::History {
            run_id: request.run_id.clone(),
            after: 0,
            limit: 100,
        },
        Operation::Inbox {
            run_id: request.run_id.clone(),
            after: 0,
            limit: 100,
        },
    ] {
        assert_eq!(
            call(&foreign_viewer_client, operation).unwrap_err().code,
            ErrorCode::NotFound
        );
    }
    let held_meta: Value =
        serde_json::from_slice(&std::fs::read(h.dir.join("held.ready")).unwrap()).unwrap();
    assert_eq!(
        call(
            &foreign_worker_client,
            Operation::Assignment {
                assignment_id: held_meta["assignment"].as_str().unwrap().into()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Unauthorized
    );

    h.wait_file("owner.ready");
    // Wait for the kernel group-stop notification, then kill the actual owner.
    let pid = h.children[held].id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if r == pid {
            assert!(libc::WIFSTOPPED(status));
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    h.children[owner].kill().unwrap();
    h.children[owner].wait().unwrap();
    let takeover_started = Instant::now();
    let successor=h.spawn(json!({"kind":"scheduler","id":"successor","binding":sb_binding,"workers":[workers[1].id,workers[2].id],"ttl":10000,"ready":h.dir.join("successor.ready"),"stop":h.dir.join("stop")}));
    let mut active_workers = vec![];
    for (i, worker_client) in worker_clients.iter().enumerate().skip(1) {
        active_workers.push(h.spawn(json!({"kind":"worker","binding":worker_client.0,"out":h.dir.join(format!("worker-{i}.out")),"stop":h.dir.join("stop")})));
    }
    let until = Instant::now() + Duration::from_secs(60);
    let target = loop {
        let Response::Waits(page) = call(
            &approver_client,
            Operation::Waits {
                run_id: request.run_id.clone(),
                after: 0,
                limit: 100,
            },
        )
        .unwrap() else {
            panic!()
        };
        if let Some(w) = page.items.into_iter().next() {
            break w;
        }
        assert!(Instant::now() < until, "takeover failed");
        std::thread::sleep(Duration::from_millis(50));
    };
    h.wait_file("successor.ready");
    assert!(takeover_started.elapsed() < Duration::from_secs(60));
    let Response::Snapshot(before) = call(
        &runner_client,
        Operation::Get {
            run_id: request.run_id.clone(),
        },
    )
    .unwrap() else {
        panic!()
    };
    std::fs::write(h.dir.join("resume"), b"go").unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    h.wait_child(held);
    let old: Result<Response> =
        serde_json::from_slice(&std::fs::read(h.dir.join("held.out")).unwrap()).unwrap();
    assert!(matches!(
        old.unwrap_err().code,
        ErrorCode::Unauthorized | ErrorCode::LeaseConflict
    ));
    let old_meta: Value =
        serde_json::from_slice(&std::fs::read(h.dir.join("held.ready")).unwrap()).unwrap();
    let rows = db()
        .query(
            "SELECT lease FROM workflow_access.assignments WHERE tenant=$1 AND settled",
            &[&tenant],
        )
        .unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        let lease: Lease = serde_json::from_str(row.get(0)).unwrap();
        assert!(lease.epoch > old_meta["epoch"].as_u64().unwrap());
        assert_eq!(lease.owner, sb.id);
    }
    println!(
        "cluster_readonly_fault: takeover_to_approval_ms={} lease_ms=5000 scan_ms=40 schedulers=2 workers=3",
        takeover_started.elapsed().as_millis()
    );
    let Response::Snapshot(after) = call(
        &runner_client,
        Operation::Get {
            run_id: request.run_id.clone(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(before, after);
    let signal = SignalSubmission {
        schema_version: 1,
        run_id: request.run_id.clone(),
        run_digest: after.run_digest.clone(),
        message: workflow_kernel::SignalMessage {
            schema_version: 1,
            message_id: "approval".into(),
            correlation_id: target.correlation_id,
            target: target.target,
            source: "forged".into(),
            decision: workflow_kernel::SignalDecision::Approve,
            reason: "review complete".into(),
            outputs: Default::default(),
            expires_at_unix_ms: target.deadline_unix_ms,
        },
    };
    call(
        &approver_client,
        Operation::Approve {
            request: Box::new(signal),
        },
    )
    .unwrap();
    let Response::Snapshot(final_state) = call(
        &runner_client,
        Operation::Get {
            run_id: request.run_id.clone(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(final_state.status, RunStatus::Succeeded);
    let mut local = workflow_runstore_sqlite::SqliteRunStore::image_reducer(None).unwrap();
    let mut local_request = request.clone();
    // The run digest includes the immutable start time; recover it from the
    // committed PostgreSQL image rather than selecting a new local timestamp.
    let row=db().query_one("SELECT image FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2", &[&tenant,&request.run_id]).unwrap();
    let bytes: Vec<u8> = row.get(0);
    let image = workflow_runstore_sqlite::RunImage::parse(&bytes).unwrap();
    let mut recovered = workflow_runstore_sqlite::SqliteRunStore::from_image(&image, None).unwrap();
    local_request.started_at_unix_ms = recovered.started_at(&request.run_id).unwrap();
    local.start(&local_request).unwrap();
    workflow_runtime::drive(
        &mut local,
        &workflow_builtin_capabilities::worker().unwrap(),
        &request.run_id,
        &workflow_runtime::DriveOptions {
            owner: "local".into(),
            acquisition_id: "local".into(),
            lease_ms: 120000,
            max_commands: 100,
        },
        &workflow_worker::SystemClock,
    )
    .unwrap();
    let target = local
        .waits(&request.run_id, 0, 100)
        .unwrap()
        .items
        .remove(0);
    local
        .receive_signal(
            &SignalSubmission {
                schema_version: 1,
                run_id: request.run_id.clone(),
                run_digest: final_state.run_digest.clone(),
                message: workflow_kernel::SignalMessage {
                    schema_version: 1,
                    message_id: "approval".into(),
                    correlation_id: target.correlation_id,
                    target: target.target,
                    source: "approver".into(),
                    decision: workflow_kernel::SignalDecision::Approve,
                    reason: "review complete".into(),
                    outputs: Default::default(),
                    expires_at_unix_ms: target.deadline_unix_ms,
                },
            },
            &workflow_worker::SystemClock,
        )
        .unwrap();
    let local_state = local.get(&request.run_id).unwrap();
    assert_eq!(local_state.status, final_state.status);
    assert_eq!(local_state.run_digest, final_state.run_digest);
    assert_eq!(local_state.frames, final_state.frames);

    std::fs::write(h.dir.join("stop"), b"stop").unwrap();
    h.wait_child(successor);
    for i in active_workers {
        h.wait_child(i);
    }
    let total: u64 = (1..3)
        .map(|i| {
            serde_json::from_slice::<Value>(
                &std::fs::read(h.dir.join(format!("worker-{i}.out"))).unwrap(),
            )
            .unwrap()["completed"]
                .as_u64()
                .unwrap()
        })
        .sum();
    assert_eq!(total, 2);
    // Database cannot write: a start must not be acknowledged or appear later.
    let original = std::fs::read(h.dir.join("database")).unwrap();
    let readonly = format!(
        "{} options='-c default_transaction_read_only=on'",
        String::from_utf8(original.clone()).unwrap()
    );
    std::fs::write(h.dir.join("database"), readonly).unwrap();
    assert!(
        call(
            &runner_client,
            Operation::Start {
                request: Box::new(fixture("unacknowledged"))
            }
        )
        .is_err()
    );
    std::fs::write(h.dir.join("database"), original).unwrap();
    assert_eq!(
        call(
            &runner_client,
            Operation::Get {
                run_id: "unacknowledged".into()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );
    // Rotating the referenced file takes effect on the existing HTTP client.
    let replacement = service
        .rotate(admin.expose_secret(), &runner.id, 3600000)
        .unwrap();
    assert_eq!(
        call(
            &runner_client,
            Operation::Get {
                run_id: request.run_id.clone()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Unauthorized
    );
    let SecretRef::File { path } = runner_binding.credential else {
        panic!()
    };
    let temp = path.with_extension("new");
    write_credential(&temp, &replacement).unwrap();
    std::fs::rename(temp, path).unwrap();
    assert!(
        call(
            &runner_client,
            Operation::Get {
                run_id: request.run_id.clone()
            }
        )
        .is_ok()
    );
    for i in 0..h.children.len() {
        let log = std::fs::read_to_string(h.dir.join(format!("child-{i}.log"))).unwrap();
        assert!(!log.contains(runner.expose_secret()));
        assert!(!log.contains(admin.expose_secret()));
    }
    unsafe { libc::kill(h.children[0].id() as i32, libc::SIGTERM) };
    h.wait_child(0);
}
