use super::*;
use std::collections::BTreeMap;
use workflow_ir::VersionRef;
use workflow_worker::{CapabilityDescriptor, EffectContract, FailureClass, Idempotency};
fn reference(id: &str) -> VersionRef {
    VersionRef {
        id: id.into(),
        version: "1.0.0".into(),
    }
}
fn intent() -> EffectIntent {
    let run_digest = digest(&"run-one").unwrap();
    EffectIntent {
        schema_version: 1,
        operation_key: operation_key(&run_digest, 7).unwrap(),
        run_id: "run-one".into(),
        run_digest,
        instance_id: 7,
        workflow: reference("workflow"),
        node_id: "write".into(),
        command_id: digest(&"command").unwrap(),
        command_digest: digest(&"body").unwrap(),
        capability: CapabilityDescriptor {
            schema_version: 1,
            capability: reference("write"),
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            timeout_ms: 100,
            error_codes: [("retry".into(), FailureClass::Transient)].into(),
            effects: EffectContract::Write {
                idempotency: Idempotency::Key {
                    scope: "release".into(),
                    retention_ms: 1000,
                },
                query: None,
                compensation: None,
            },
            usage: "unit contract".into(),
            skill: None,
        },
        inputs: BTreeMap::new(),
        input_digest: digest(&Values::new()).unwrap(),
        policy: EffectPolicy {
            identity: reference("policy"),
            target: reference("provider"),
            call_identity: reference("principal"),
            retry: RetryPolicy {
                max_calls: 3,
                initial_backoff_ms: 10,
                max_backoff_ms: 20,
                total_write_ms: 10000,
            },
        },
        created_at_unix_ms: 1000,
    }
}
fn prepare(i: EffectIntent) -> EffectRecord {
    let attempt = EffectAttempt {
        intent: i,
        attempt_id: "a1".into(),
        epoch: 1,
        number: 1,
        kind: CallKind::Write,
        prepared_revision: 1,
        issued_at_unix_ms: 1000,
        deadline_unix_ms: 1100,
    };
    EffectRecord {
        epoch: 1,
        at_unix_ms: 1000,
        change: EffectChange::Prepared {
            request_digest: digest(&attempt).unwrap(),
            attempt: Box::new(attempt),
        },
    }
}

#[test]
fn deduplication_retention_closes_unknown_retry_even_when_run_budget_remains() {
    let mut ledger = BTreeMap::new();
    let i = intent();
    let key = i.operation_key.clone();
    apply(&mut ledger, &prepare(i)).unwrap();
    assert_eq!(decision(&ledger[&key], 1, 1001, true), Decision::InProgress);
    assert_eq!(
        decision(&ledger[&key], 2, 1999, true),
        Decision::Call(CallKind::Write)
    );
    assert!(matches!(
        decision(&ledger[&key], 2, 2000, true),
        Decision::Manual(_)
    ));
    assert!(matches!(
        decision(&ledger[&key], 2, 1001, false),
        Decision::Manual(_)
    ));
}
#[test]
fn invalid_observations_never_mutate_the_ledger_or_invent_effect_absence() {
    let mut ledger = BTreeMap::new();
    let i = intent();
    let key = i.operation_key.clone();
    apply(&mut ledger, &prepare(i)).unwrap();
    let before = ledger.clone();
    for observation in [
        Observation::Absent,
        Observation::NotApplied {
            code: "retry".into(),
            class: FailureClass::PermissionDenied,
            message: "wrong class".into(),
        },
        Observation::Unknown { reason: "".into() },
    ] {
        assert!(
            apply(
                &mut ledger,
                &EffectRecord {
                    epoch: 1,
                    at_unix_ms: 1001,
                    change: EffectChange::Observed {
                        operation_key: key.clone(),
                        attempt_id: "a1".into(),
                        observation
                    }
                }
            )
            .is_err()
        );
        assert_eq!(ledger, before);
    }
}
#[test]
fn new_instances_and_runs_never_reuse_the_previous_operation_key() {
    let i = intent();
    let mut keys = std::collections::BTreeSet::new();
    for run in [i.run_digest, digest(&"run-two").unwrap()] {
        for instance in 1..=8 {
            assert!(keys.insert(operation_key(&run, instance).unwrap()));
        }
    }
    assert_eq!(keys.len(), 16);
}
