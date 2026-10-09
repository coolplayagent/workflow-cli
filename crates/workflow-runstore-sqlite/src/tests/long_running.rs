use super::*;

#[test]
fn long_history_uses_a_bounded_tail_but_full_audit_checks_every_event() {
    let db = Db::new();
    let s = scenario("review-approved");
    let mut store = db.store();
    let mut snapshot = store.start(&start(&s)).unwrap().snapshot;
    for i in 0..130 {
        snapshot = store
            .apply(&event(
                &snapshot,
                &format!("history-{i}"),
                EventKind::AdvanceTime,
            ))
            .unwrap()
            .snapshot;
    }
    drop(store);
    let mut store = SqliteRunStore::open_readonly(&db.path).unwrap();
    let usage = store.history_usage(&s.run_id).unwrap();
    assert_eq!(usage["state_checkpoint_revision"], 128);
    assert_eq!(usage["replayed_events"], 3);
    assert_eq!(store.get(&s.run_id).unwrap(), snapshot);
    assert_eq!(store.verify(&s.run_id).unwrap().events_checked, 130);
    let checkpoints: i64 = store
        .connection
        .query_row("SELECT count(*) FROM checkpoints", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        checkpoints, 0,
        "new runs must not retain quadratic journal prefixes"
    );
}

#[test]
fn deleting_an_intermediate_state_checkpoint_is_detected_on_ordinary_read() {
    let db = Db::new();
    let s = scenario("review-approved");
    let mut store = db.store();
    let mut snapshot = store.start(&start(&s)).unwrap().snapshot;
    for i in 0..33 {
        snapshot = store
            .apply(&event(
                &snapshot,
                &format!("history-{i}"),
                EventKind::AdvanceTime,
            ))
            .unwrap()
            .snapshot;
    }
    store.connection.execute_batch("DROP TRIGGER immutable_state_checkpoints_delete; DELETE FROM state_checkpoints WHERE revision=16").unwrap();
    assert_eq!(
        store.get(&s.run_id).unwrap_err().code,
        ErrorCode::CorruptStorage
    );
}

#[test]
fn approaching_the_event_limit_recommends_continuation_before_admission_fails() {
    let db = Db::new();
    let s = scenario("review-approved");
    let mut request = start(&s);
    request.limits.max_events = 10;
    let mut store = db.store();
    let mut snapshot = store.start(&request).unwrap().snapshot;
    assert_eq!(
        store.history_usage(&s.run_id).unwrap()["continuation_recommended"],
        false
    );
    for i in 0..8 {
        snapshot = store
            .apply(&event(
                &snapshot,
                &format!("history-{i}"),
                EventKind::AdvanceTime,
            ))
            .unwrap()
            .snapshot;
    }
    assert_eq!(
        store.history_usage(&s.run_id).unwrap()["continuation_recommended"],
        true
    );
}

fn value_type(value: &serde_json::Value) -> workflow_ir::ValueType {
    use workflow_ir::ValueType as T;
    match value {
        serde_json::Value::String(_) => T::String,
        serde_json::Value::Number(_) => T::Integer,
        serde_json::Value::Bool(_) => T::Boolean,
        serde_json::Value::Array(values) => T::Array {
            items: Box::new(values.first().map(value_type).unwrap_or(T::String)),
        },
        serde_json::Value::Object(values) => T::Object {
            fields: values
                .iter()
                .map(|(k, v)| (k.clone(), value_type(v)))
                .collect(),
        },
        _ => panic!("handoff cannot contain null"),
    }
}
#[test]
fn continuation_is_reserved_once_and_reopens_after_the_prepare_start_crash_boundary() {
    use workflow_runstore::{ContinuationPlan, ContinuationStore, Handoff, VerifiedFact};
    struct Time(u64);
    impl workflow_worker::Clock for Time {
        fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
            Ok(self.0)
        }
    }
    let db = Db::new();
    let mut store = db.store();
    let source: StartRun = serde_json::from_value(serde_json::json!({
        "schema_version":1,"run_id":"segment-1","inputs":{},"started_at_unix_ms":100,
        "bundle":{"schema_version":1,"root":{"id":"segment","version":"1"},"capabilities":[],
        "workflows":[{"schema_version":1,"id":"segment","version":"1","entry":"done","inputs":{},
            "nodes":[{"id":"done","kind":{"type":"terminal","outcome":"succeeded"},
                "inputs":{"diagnostic":{"value_type":{"type":"string"},"required":true}},
                "bindings":{"diagnostic":{"source":"literal","value":"validated output"}}}],"edges":[]}]}
    })).unwrap();
    let snapshot = store.start(&source).unwrap().snapshot;
    let lease = store
        .acquire(
            &LeaseRequest {
                run_id: source.run_id.clone(),
                owner: "owner".into(),
                acquisition_id: "continue".into(),
                ttl_ms: 1000,
            },
            &Time(101),
        )
        .unwrap();
    while !matches!(store.claim_next(&lease, &Time(101)).unwrap(), Claimed::Idle) {}
    let handoff = Handoff {
        schema_version: 1,
        source_run_id: snapshot.run_id.clone(),
        source_run_digest: snapshot.run_digest,
        source_revision: snapshot.revision,
        objective: "Finish remaining inspection".into(),
        plan: vec!["Inspect the next segment".into()],
        verified_facts: vec![VerifiedFact {
            instance_id: snapshot.frames[&1].nodes["done"].instance_id,
            field: "diagnostic".into(),
            value: serde_json::json!("validated output"),
        }],
        artifacts: vec![],
        failures: vec![],
        remaining_work: vec!["Inspect remaining files".into()],
    };
    let value = serde_json::to_value(&handoff).unwrap();
    let mut successor = source.clone();
    successor.run_id = "segment-2".into();
    successor.started_at_unix_ms = 102;
    successor.bundle.root.version = "2".into();
    successor.bundle.workflows[0].version = "2".into();
    successor.bundle.workflows[0].inputs.insert(
        "handoff".into(),
        workflow_ir::Field {
            required: true,
            value_type: value_type(&value),
        },
    );
    successor.inputs.insert("handoff".into(), value);
    let plan = ContinuationPlan {
        schema_version: 1,
        handoff,
        handoff_input: "handoff".into(),
        successor,
    };
    let mut forged = plan.clone();
    forged.handoff.verified_facts[0].value = serde_json::json!("invented success");
    forged.successor.inputs.insert(
        "handoff".into(),
        serde_json::to_value(&forged.handoff).unwrap(),
    );
    assert!(
        store
            .prepare_continuation(&lease, &forged, &Time(102))
            .is_err()
    );
    store
        .prepare_continuation(&lease, &plan, &Time(102))
        .unwrap();
    drop(store); // Simulated crash after reservation, before starting the successor.
    let mut store = SqliteRunStore::open(&db.path).unwrap();
    let saved = store.continuation(&source.run_id).unwrap().unwrap();
    assert_eq!(saved, plan);
    assert!(store.get("segment-2").is_err());
    assert_eq!(
        store
            .prepare_continuation(&lease, &plan, &Time(5000))
            .unwrap(),
        plan,
        "exact reservation retries do not need new authority"
    );
    let mut alternate = plan.clone();
    alternate.successor.run_id = "segment-3".into();
    assert!(
        store
            .prepare_continuation(&lease, &alternate, &Time(5000))
            .is_err()
    );
    let image = store.export_image("segment-1").unwrap();
    let started = store.start(&saved.successor).unwrap();
    assert_eq!(started.snapshot.revision, 1);
    assert!(store.start(&saved.successor).unwrap().transition.duplicate);
    store.verify("segment-1").unwrap();
    store.verify("segment-2").unwrap();
    let mut restored = SqliteRunStore::from_image(&image, None).unwrap();
    assert_eq!(restored.continuation("segment-1").unwrap(), Some(plan));
}
