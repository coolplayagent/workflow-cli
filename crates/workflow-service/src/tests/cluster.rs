use super::*;
use workflow_runstore_postgres::access::SchedulingPolicy;

fn root() -> PathBuf {
    if let Ok(r) = std::env::var("TEST_SRCDIR") {
        PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap())
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
}
fn simple(id: &str) -> StartRun {
    let mut r: StartRun = serde_json::from_slice(
        &std::fs::read(root().join("examples/execution/valid-start.json")).unwrap(),
    )
    .unwrap();
    r.run_id = id.into();
    r
}
pub(super) fn worker(s: &Value) {
    let mut binding: ClientBinding = serde_json::from_value(s["binding"].clone()).unwrap();
    binding.timeout_ms = 10000;
    let client = RemoteClient::new(binding.clone()).unwrap();
    let session =
        WorkerSession::start(binding, s["version"].as_str().unwrap().into(), 1000).unwrap();
    let worker = workflow_builtin_capabilities::worker().unwrap();
    let mut completed = 0;
    let mut fenced = 0;
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        session.check().unwrap();
        if (Path::new(s["stop"].as_str().unwrap()).exists() || session.draining())
            && session.drain().unwrap().active_assignments == 0
        {
            break;
        }
        assert!(Instant::now() < until, "worker failed to drain");
        let report = work_once(&client, &worker, 1).unwrap();
        completed += report.completed;
        fenced += report.fenced;
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::write(
        s["out"].as_str().unwrap(),
        json!({"completed":completed,"fenced":fenced,"drained":true}).to_string(),
    )
    .unwrap();
}
struct Actors {
    tenant: String,
    runner: RemoteClient,
    owner_binding: ClientBinding,
    successor_binding: ClientBinding,
    workers: Vec<IssuedCredential>,
    bindings: Vec<ClientBinding>,
    scheduler: IssuedCredential,
}
fn actors(h: &Harness, label: &str, count: usize) -> Actors {
    let tenant = format!(
        "r09-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let policy: SchedulingPolicy = serde_json::from_slice(
        &std::fs::read(root().join("examples/cluster/policy.json")).unwrap(),
    )
    .unwrap();
    AuthenticatedService::configure_scheduling(&mut db(), &tenant, None, &policy).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let author = credential(&mut service, &admin, "author", Role::DefinitionMaintainer);
    service
        .publish(author.expose_secret(), &simple("x").bundle)
        .unwrap();
    let owner = credential(&mut service, &admin, "owner", Role::Scheduler);
    let scheduler = credential(&mut service, &admin, "successor", Role::Scheduler);
    let workers = (0..count)
        .map(|i| credential(&mut service, &admin, &format!("worker-{i}"), Role::Worker))
        .collect::<Vec<_>>();
    for w in &workers {
        service
            .worker_heartbeat(w.expose_secret(), "1.0.0", false)
            .unwrap();
    }
    Actors {
        tenant,
        runner: h.client(&runner).1,
        owner_binding: h.client(&owner).0,
        successor_binding: h.client(&scheduler).0,
        bindings: workers.iter().map(|w| h.client(w).0).collect(),
        workers,
        scheduler,
    }
}
fn start(a: &Actors, id: &str) {
    call(
        &a.runner,
        Operation::Start {
            request: Box::new(simple(id)),
        },
    )
    .unwrap();
}
fn get(a: &Actors, id: &str) -> Box<Snapshot> {
    let Response::Snapshot(s) = call(&a.runner, Operation::Get { run_id: id.into() }).unwrap()
    else {
        panic!("snapshot")
    };
    s
}
fn parity(a: &Actors, remote: &Snapshot) {
    let row=db().query_one("SELECT image FROM workflow_authority.runs WHERE tenant=$1 AND project='project' AND run_id=$2", &[&a.tenant,&remote.run_id]).unwrap();
    let bytes: Vec<u8> = row.get(0);
    let image = workflow_runstore_sqlite::RunImage::parse(&bytes).unwrap();
    let mut recovered = workflow_runstore_sqlite::SqliteRunStore::from_image(&image, None).unwrap();
    let mut request = simple(&remote.run_id);
    request.started_at_unix_ms = recovered.started_at(&remote.run_id).unwrap();
    struct Time(u64);
    impl workflow_worker::Clock for Time {
        fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
            Ok(self.0)
        }
    }
    let mut local = workflow_runstore_sqlite::SqliteRunStore::image_reducer(None).unwrap();
    local.start(&request).unwrap();
    let report = workflow_runtime::drive(
        &mut local,
        &workflow_builtin_capabilities::worker().unwrap(),
        &request.run_id,
        &workflow_runtime::DriveOptions {
            owner: "local".into(),
            acquisition_id: "local".into(),
            lease_ms: 30000,
            max_commands: 20,
        },
        &Time(request.started_at_unix_ms),
    )
    .unwrap();
    assert_eq!(report.snapshot.status, remote.status);
    assert_eq!(report.snapshot.frames, remote.frames);
    assert_eq!(report.snapshot.run_digest, remote.run_digest);
}
#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn tls_cluster_quota_fairness_owner_loss_late_result_and_rolling_drain_contract() {
    let mut h = Harness::new(true);
    let a = actors(&h, "busy", 2);
    let b = actors(&h, "small", 1);
    start(&a, "fault-first");
    let owner=h.spawn(json!({"kind":"scheduler","cluster":true,"id":"owner","binding":a.owner_binding,"workers":[a.workers[0].id],"ttl":8000,"ready":h.dir.join("owner.ready"),"stop":h.dir.join("stop-owner")}));
    let held=h.spawn(json!({"kind":"held_worker","binding":a.bindings[0],"ready":h.dir.join("held.ready"),"resume":h.dir.join("resume"),"out":h.dir.join("held.out")}));
    h.wait_file("held.ready");
    h.wait_file("owner.ready");
    let held_meta: Value =
        serde_json::from_slice(&std::fs::read(h.dir.join("held.ready")).unwrap()).unwrap();
    let pid = h.children[held].id() as i32;
    // SAFETY: this PID belongs to the fixture's retained child process.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let mut status = 0;
        // SAFETY: wait only for the owned child and inspect initialized status.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if result == pid {
            assert!(libc::WIFSTOPPED(status));
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    h.children[owner].kill().unwrap();
    h.children[owner].wait().unwrap();
    let lost = Instant::now();
    let successor=h.spawn(json!({"kind":"scheduler","cluster":true,"id":"successor","binding":a.successor_binding,"workers":[a.workers[1].id],"ttl":30000,"ready":h.dir.join("successor.ready"),"stop":h.dir.join("stop-schedulers")}));
    let scheduler_b=h.spawn(json!({"kind":"scheduler","cluster":true,"id":"small","binding":b.owner_binding,"workers":[b.workers[0].id],"ttl":30000,"ready":h.dir.join("small.ready"),"stop":h.dir.join("stop-schedulers")}));
    let worker_a=h.spawn(json!({"kind":"cluster_worker","version":"2.0.0","binding":a.bindings[1],"out":h.dir.join("a.out"),"stop":h.dir.join("drain-workers")}));
    let worker_b=h.spawn(json!({"kind":"cluster_worker","version":"1.0.0","binding":b.bindings[0],"out":h.dir.join("b.out"),"stop":h.dir.join("drain-workers")}));
    let until = Instant::now() + Duration::from_secs(60);
    let final_state = loop {
        let s = get(&a, "fault-first");
        if s.status == RunStatus::Succeeded {
            break s;
        }
        assert!(
            Instant::now() < until,
            "configured cluster failed to recover owner loss"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let recovery_ms = lost.elapsed().as_millis();
    let rows=db().query("SELECT lease FROM workflow_access.assignments WHERE tenant=$1 AND run_id='fault-first' AND settled", &[&a.tenant]).unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        let l: Lease = serde_json::from_str(row.get(0)).unwrap();
        assert!(l.epoch > held_meta["epoch"].as_u64().unwrap());
        assert_eq!(l.owner, a.scheduler.id);
    }
    std::fs::write(h.dir.join("resume"), b"resume").unwrap();
    // SAFETY: resume the same stopped child retained by this fixture.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    h.wait_child(held);
    let old: Result<Response> =
        serde_json::from_slice(&std::fs::read(h.dir.join("held.out")).unwrap()).unwrap();
    assert_eq!(old.unwrap_err().code, ErrorCode::LeaseConflict);
    assert_eq!(get(&a, "fault-first").as_ref(), final_state.as_ref());
    parity(&a, &final_state);
    let load_started = Instant::now();
    for i in 0..12 {
        start(&a, &format!("load-{i:02}"));
    }
    for i in 0..3 {
        start(&b, &format!("load-{i:02}"));
    }
    let until = Instant::now() + Duration::from_secs(90);
    loop {
        let done_a =
            (0..12).all(|i| get(&a, &format!("load-{i:02}")).status == RunStatus::Succeeded);
        let done_b =
            (0..3).all(|i| get(&b, &format!("load-{i:02}")).status == RunStatus::Succeeded);
        if done_a && done_b {
            break;
        }
        assert!(
            Instant::now() < until,
            "a busy tenant starved a scoped scheduler"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let elapsed = load_started.elapsed();
    // Stop admission while the scheduler processes remain live, then drain all
    // already assigned work. A restarted credential stays drained server-side.
    std::fs::write(h.dir.join("drain-workers"), b"drain").unwrap();
    h.wait_child(worker_a);
    h.wait_child(worker_b);
    for output in ["a.out", "b.out"] {
        let r: Value = serde_json::from_slice(&std::fs::read(h.dir.join(output)).unwrap()).unwrap();
        assert_eq!(r["drained"], true);
    }
    let mut service = AuthenticatedService::open(db()).unwrap();
    assert!(
        service
            .worker_heartbeat(a.workers[1].expose_secret(), "2.0.0", false)
            .unwrap()
            .draining
    );
    assert!(
        service
            .worker_heartbeat(b.workers[0].expose_secret(), "1.0.0", false)
            .unwrap()
            .draining
    );
    std::fs::write(h.dir.join("stop-schedulers"), b"stop").unwrap();
    h.wait_child(successor);
    h.wait_child(scheduler_b);
    let rows=db().query("SELECT a.tenant,q.run_id,min(a.created_at)-q.created_at FROM workflow_scheduling.admissions a JOIN workflow_scheduling.queue q USING(tenant,project,run_id) WHERE a.tenant IN ($1,$2) AND a.run_id LIKE 'load-%' GROUP BY a.tenant,q.run_id,q.created_at", &[&a.tenant,&b.tenant]).unwrap();
    let mut delays = rows.iter().map(|r| r.get::<_, i64>(2)).collect::<Vec<_>>();
    delays.sort_unstable();
    assert_eq!(delays.len(), 15);
    let mut small = rows
        .iter()
        .filter(|r| r.get::<_, String>(0) == b.tenant)
        .map(|r| r.get::<_, i64>(2))
        .collect::<Vec<_>>();
    small.sort_unstable();
    assert_eq!(small.len(), 3);
    let memory = std::fs::read_to_string("/proc/meminfo").unwrap();
    let cpu = std::fs::read_to_string("/proc/cpuinfo").unwrap();
    eprintln!(
        "r09_cluster_acceptance: {}",
        json!({"fault":"scheduler process loss and SIGSTOP stale read-only worker","schedulers":2,"workers":3,"lease_ms":8000,"scan_ms":40,"recovery_ms":recovery_ms,"jobs":15,"elapsed_ms":elapsed.as_millis(),"jobs_per_second":15.0/elapsed.as_secs_f64(),"queue_p95_ms":delays[14],"small_tenant_queue_p95_ms":small[2],"architecture":std::env::consts::ARCH,"logical_cpus":std::thread::available_parallelism().unwrap().get(),"memory":memory.lines().next(),"cpu_model":cpu.lines().find(|l|l.starts_with("model name")),"parity":true,"stale_result_rejected":true,"drained":true})
    );
}
