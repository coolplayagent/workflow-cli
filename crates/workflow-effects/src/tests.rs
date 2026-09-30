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
        release: None,
        dependencies: vec![],
        compensates: None,
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
                irreversible: false,
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
        release: None,
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

fn record_applied(ledger: &mut BTreeMap<String, EffectState>, i: EffectIntent) -> EffectReceipt {
    apply(ledger, &prepare(i.clone())).unwrap();
    let receipt = EffectReceipt {
        release: None,
        operation_key: i.operation_key.clone(),
        intent_digest: digest(&i).unwrap(),
        target: i.policy.target.clone(),
        resource_id: format!("resource-{}", i.instance_id),
        provider_receipt: format!("provider-{}", i.instance_id),
        outputs: Values::new(),
    };
    apply(
        ledger,
        &EffectRecord {
            epoch: 1,
            at_unix_ms: 1000,
            change: EffectChange::Observed {
                operation_key: i.operation_key,
                attempt_id: "a1".into(),
                observation: Observation::Applied {
                    receipt: receipt.clone(),
                },
            },
        },
    )
    .unwrap();
    receipt
}
fn reversible(instance: u64) -> EffectIntent {
    let mut i = intent();
    i.instance_id = instance;
    i.operation_key = i.expected_key().unwrap();
    let EffectContract::Write { compensation, .. } = &mut i.capability.effects else {
        panic!()
    };
    *compensation = Some(reference(&format!("undo-{instance}")));
    i
}
fn undo(original: &EffectIntent, receipt: &EffectReceipt) -> EffectIntent {
    let mut i = intent();
    i.instance_id = original.instance_id + 100;
    i.node_id = format!("undo-{}", original.instance_id);
    i.capability.capability = reference(&i.node_id);
    i.compensates = Some(CompensationRef {
        operation_key: original.operation_key.clone(),
        receipt: receipt.clone(),
    });
    i.operation_key = i.expected_key().unwrap();
    i
}
#[test]
fn ledger_rejects_wrong_reverse_order_receipt_substitution_and_late_dependents_atomically() {
    let mut ledger = BTreeMap::new();
    let a = reversible(1);
    let ar = record_applied(&mut ledger, a.clone());
    let mut b = reversible(2);
    b.dependencies = vec![a.operation_key.clone()];
    let br = record_applied(&mut ledger, b.clone());
    let undo_a = undo(&a, &ar);
    let before = ledger.clone();
    assert!(apply(&mut ledger, &prepare(undo_a.clone())).is_err());
    assert_eq!(ledger, before);
    let mut forged = undo(&b, &br);
    forged.compensates.as_mut().unwrap().receipt.resource_id = "wrong-resource".into();
    assert!(apply(&mut ledger, &prepare(forged)).is_err());
    assert_eq!(ledger, before);
    let undo_b = undo(&b, &br);
    record_applied(&mut ledger, undo_b.clone());
    assert_eq!(
        ledger[&b.operation_key].compensated_by,
        Some(undo_b.operation_key.clone())
    );
    apply(&mut ledger, &prepare(undo_a.clone())).unwrap();
    let before = ledger.clone();
    let mut late = reversible(3);
    late.dependencies = vec![a.operation_key.clone()];
    assert!(apply(&mut ledger, &prepare(late)).is_err());
    assert_eq!(ledger, before);
    assert!(apply(&mut ledger, &prepare(undo_b)).is_err());
    assert_eq!(ledger, before);
}
#[test]
fn unresolved_dependents_and_irreversible_originals_cannot_be_compensated() {
    let mut ledger = BTreeMap::new();
    let a = reversible(1);
    let ar = record_applied(&mut ledger, a.clone());
    let mut b = reversible(2);
    b.dependencies = vec![a.operation_key.clone()];
    apply(&mut ledger, &prepare(b)).unwrap();
    let before = ledger.clone();
    assert!(apply(&mut ledger, &prepare(undo(&a, &ar))).is_err());
    assert_eq!(ledger, before);
    let mut irreversible = reversible(3);
    let EffectContract::Write {
        irreversible: flag,
        compensation,
        ..
    } = &mut irreversible.capability.effects
    else {
        panic!()
    };
    *flag = true;
    *compensation = None;
    let receipt = record_applied(&mut ledger, irreversible.clone());
    let before = ledger.clone();
    assert!(apply(&mut ledger, &prepare(undo(&irreversible, &receipt))).is_err());
    assert_eq!(ledger, before);
}
#[test]
fn compensation_retry_window_exhaustion_preserves_known_absence_but_never_hides_unknown_writes() {
    for known in [true, false] {
        let mut ledger = BTreeMap::new();
        let a = reversible(1);
        let ar = record_applied(&mut ledger, a.clone());
        let u = undo(&a, &ar);
        apply(&mut ledger, &prepare(u.clone())).unwrap();
        if known {
            apply(
                &mut ledger,
                &EffectRecord {
                    epoch: 1,
                    at_unix_ms: 1001,
                    change: EffectChange::Observed {
                        operation_key: u.operation_key.clone(),
                        attempt_id: "a1".into(),
                        observation: Observation::NotApplied {
                            code: "retry".into(),
                            class: FailureClass::Transient,
                            message: "no mutation".into(),
                        },
                    },
                },
            )
            .unwrap();
        }
        assert!(matches!(
            decision(&ledger[&u.operation_key], 2, 2000, true),
            Decision::Manual(_)
        ));
        apply(
            &mut ledger,
            &EffectRecord {
                epoch: 2,
                at_unix_ms: 2000,
                change: EffectChange::Stopped {
                    operation_key: u.operation_key.clone(),
                    reason: "write window closed".into(),
                },
            },
        )
        .unwrap();
        assert_eq!(
            matches!(
                ledger[&u.operation_key].status,
                EffectStatus::NeedsAttention { .. }
            ),
            known
        );
        assert_eq!(
            matches!(
                ledger[&u.operation_key].status,
                EffectStatus::Uncertain { .. }
            ),
            !known
        );
        assert_eq!(ledger[&a.operation_key].compensated_by, None);
    }
}
