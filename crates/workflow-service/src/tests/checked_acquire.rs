use super::*;

fn checked(baseline: &AcceptanceManifest, owner: &str) -> AcquireCheckedV1 {
    AcquireCheckedV1 {
        schema_version: 1,
        lease: LeaseRequest {
            run_id: baseline.run_id.clone(),
            owner: owner.into(),
            acquisition_id: "checked-acquisition".into(),
            ttl_ms: 30000,
        },
        expected_run_digest: baseline.run_digest.clone(),
        expected_bundle_digest: baseline.bundle_digest.clone(),
        expected_snapshot_digest: baseline.snapshot_digest.clone(),
        expected_execution_digest: baseline.execution_digest.clone(),
        expected_snapshot_revision: baseline.revision,
    }
}

#[test]
fn checked_acquire_wire_is_additive_strict_and_returns_existing_lease() {
    let lease = Lease {
        generation: None,
        run_id: "run".into(),
        owner: "owner".into(),
        acquisition_id: "claim".into(),
        epoch: 1,
        issued_at_unix_ms: 1000,
        expires_at_unix_ms: 2000,
    };
    let body = json!({"schema_version":1,"lease":{"run_id":"run","owner":"owner","acquisition_id":"claim","ttl_ms":1000},"expected_run_digest":format!("sha256:{}","a".repeat(64)),"expected_bundle_digest":format!("sha256:{}","b".repeat(64)),"expected_snapshot_digest":format!("sha256:{}","c".repeat(64)),"expected_execution_digest":format!("sha256:{}","d".repeat(64)),"expected_snapshot_revision":1});
    let value = json!({"protocol_version":1,"request_id":"test","operation":{"type":"acquire_checked_v1","request":body}});
    let request: Request = serde_json::from_value(value.clone()).unwrap();
    request.validate().unwrap();
    assert_eq!(serde_json::to_value(&request).unwrap(), value);
    assert!(request.accepts(&Response::Lease(lease.clone())));
    let mut foreign = lease.clone();
    foreign.run_id = "other-run".into();
    assert!(!request.accepts(&Response::Lease(foreign)));
    assert!(!request.accepts(&Response::Unit));
    let old = json!({"protocol_version":1,"request_id":"old","operation":{"type":"acquire","run_id":"run","acquisition_id":"claim","ttl_ms":1000}});
    let legacy: Request = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(&legacy).unwrap(), old);
    assert!(legacy.accepts(&Response::Lease(lease)));
    for edit in ["missing", "unknown", "version"] {
        let mut value = value.clone();
        match edit {
            "missing" => {
                value["operation"]["request"]
                    .as_object_mut()
                    .unwrap()
                    .remove("expected_execution_digest");
            }
            "unknown" => {
                value["operation"]["request"]["ignore_baseline"] = json!(true);
            }
            _ => {
                value["operation"]["request"]["schema_version"] = json!(2);
            }
        }
        assert!(serde_json::from_value::<Request>(value).map_or(true, |r| r.validate().is_err()));
    }
    let reply = Reply {
        protocol_version: 1,
        request_id: "test".into(),
        result: Err(Error::new(ErrorCode::AdmissionConflict, "baseline changed")),
    };
    assert_eq!(
        transport::accept_reply(&request, reply, false)
            .unwrap_err()
            .code,
        ErrorCode::AdmissionConflict
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn tls_checked_acquire_uses_authenticated_owner_and_fences_replay() {
    let h = Harness::new(true);
    let tenant = format!(
        "checked-tls-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let admin =
        AuthenticatedService::bootstrap(&mut db(), &tenant, "project", "admin", 3600000).unwrap();
    let mut service = AuthenticatedService::open(db()).unwrap();
    let runner = credential(&mut service, &admin, "runner", Role::Runner);
    let author = credential(&mut service, &admin, "author", Role::DefinitionMaintainer);
    let scheduler = credential(&mut service, &admin, "scheduler", Role::Scheduler);
    let (_, client) = h.client(&scheduler);
    let mut start = fixture("checked-tls");
    start.started_at_unix_ms =
        workflow_worker::Clock::now_unix_ms(&workflow_worker::SystemClock).unwrap();
    service
        .publish(author.expose_secret(), &start.bundle)
        .unwrap();
    service.start(runner.expose_secret(), &start).unwrap();
    let Response::Acceptance(baseline) = call(
        &client,
        Operation::Acceptance {
            run_id: start.run_id.clone(),
        },
    )
    .unwrap() else {
        panic!("acceptance response required")
    };
    let request = checked(&baseline, &scheduler.id);
    let mut wrong = request.clone();
    wrong.lease.owner = "other-owner".into();
    assert_eq!(
        call(
            &client,
            Operation::AcquireCheckedV1 {
                request: Box::new(wrong)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Unauthorized
    );
    let operation = Operation::AcquireCheckedV1 {
        request: Box::new(request),
    };
    let Response::Lease(lease) = call(&client, operation.clone()).unwrap() else {
        panic!("lease response required")
    };
    assert_eq!(lease.owner, scheduler.id);
    assert_eq!(
        call(&client, operation).unwrap_err().code,
        ErrorCode::AdmissionConflict
    );
    call(&client, Operation::Release { lease }).unwrap();
    let Response::Lease(legacy) = call(
        &client,
        Operation::Acquire {
            run_id: start.run_id,
            acquisition_id: "legacy-after-checked".into(),
            ttl_ms: 30000,
        },
    )
    .unwrap() else {
        panic!("legacy lease response required")
    };
    assert_eq!(legacy.epoch, 2);
    call(&client, Operation::Release { lease: legacy }).unwrap();
    let Response::Acceptance(before) = call(
        &client,
        Operation::Acceptance {
            run_id: "checked-tls".into(),
        },
    )
    .unwrap() else {
        panic!("acceptance required")
    };
    let mut request = checked(&before, &scheduler.id);
    request.lease.acquisition_id = "lost-reply".into();
    struct DropCompletedReply<'a> {
        client: &'a RemoteClient,
        calls: std::cell::Cell<u32>,
    }
    impl TaskTransport for DropCompletedReply<'_> {
        fn call(&self, request: &Request) -> Result<Response> {
            self.calls.set(self.calls.get() + 1);
            self.client.call(request)?;
            // Deliberate test transport loss after the real HTTPS owner commit.
            // This is not a claim to reproduce kernel-level network packet loss.
            Err(unavailable())
        }
    }
    let dropped = DropCompletedReply {
        client: &client,
        calls: std::cell::Cell::new(0),
    };
    assert_eq!(
        call(
            &dropped,
            Operation::AcquireCheckedV1 {
                request: Box::new(request)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Storage
    );
    assert_eq!(dropped.calls.get(), 1);
    let Response::Acceptance(after) = call(
        &client,
        Operation::Acceptance {
            run_id: "checked-tls".into(),
        },
    )
    .unwrap() else {
        panic!("acceptance required")
    };
    assert_eq!(after.snapshot_digest, before.snapshot_digest);
    assert_ne!(after.execution_digest, before.execution_digest);
}
