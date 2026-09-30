use super::*;
use workflow_effects::{CallKind, EffectAttempt, EffectReceipt, Observation, ReleaseReceipt};

fn request() -> StartRun {
    workflow_worker::parse_message(
        &std::fs::read(base().join("examples/gates/protected-release.json")).unwrap(),
    )
    .unwrap()
}
fn prepared(f: &Fixture, now: u64) -> EffectAttempt {
    let EffectClaim::Call { attempt } = f
        .store()
        .claim_effect(&f.lease, &Time(Cell::new(now)))
        .unwrap()
    else {
        panic!("release call")
    };
    *attempt
}
fn checked(start: StartRun, age: u64) -> Fixture {
    let f = fixture_start(start, true, true, age, true);
    f.finish();
    assert!(matches!(f.claim(), Claimed::Handled { .. }));
    f
}
fn receipt(p: &EffectAttempt) -> EffectReceipt {
    let release = p.intent.release.as_ref().unwrap();
    EffectReceipt {
        operation_key: p.intent.operation_key.clone(),
        intent_digest: digest(&p.intent).unwrap(),
        target: p.intent.policy.target.clone(),
        resource_id: "release-1".into(),
        provider_receipt: "durable-provider-receipt".into(),
        outputs: Values::from([("release_id".into(), "release-1".into())]),
        release: Some(Box::new(ReleaseReceipt {
            subject: release.subject.clone(),
            target_check: release.policy.target_check.clone(),
            authorization_digest: digest(p.release.as_ref().unwrap()).unwrap(),
            observed_at_unix_ms: p.issued_at_unix_ms,
        })),
    }
}

#[test]
fn release_rechecks_frozen_subject_and_builds_complete_historical_manifest() {
    let f = checked(request(), 60000);
    let p = prepared(&f, 1001);
    assert_eq!(p.release.as_ref().unwrap().evaluated_at_unix_ms, 1001);
    let mut s = f.store();
    s.validate_effect_delivery(&p, 1001).unwrap();
    for field in ["target", "grant", "time"] {
        let mut r = receipt(&p);
        let proof = r.release.as_mut().unwrap();
        match field {
            "target" => proof.subject.source_revision.revision = "b".repeat(40),
            "grant" => proof.authorization_digest = format!("sha256:{}", "b".repeat(64)),
            _ => proof.observed_at_unix_ms = p.deadline_unix_ms,
        }
        assert!(
            s.observe_effect(
                &f.lease,
                &p.attempt_id,
                &Observation::Applied { receipt: r },
                &Time(Cell::new(1002))
            )
            .is_err()
        );
    }
    s.observe_effect(
        &f.lease,
        &p.attempt_id,
        &Observation::Applied {
            receipt: receipt(&p),
        },
        &Time(Cell::new(1002)),
    )
    .unwrap();
    assert!(matches!(
        s.claim_next(&f.lease, &Time(Cell::new(1002))).unwrap(),
        Claimed::Handled { .. }
    ));
    let m = s.acceptance(&f.start.run_id).unwrap();
    assert_eq!(m.status, AcceptanceStatus::Accepted);
    assert_eq!(m.requirements.len(), 2);
    assert_eq!(m.checks.len(), 2);
    assert_eq!(m.artifacts.len(), 1);
    assert_eq!(m.effects.len(), 1);
    m.verify().unwrap();
    let mut altered = m.clone();
    altered.checks.clear();
    assert!(altered.verify().is_err());
    assert_eq!(f.store().acceptance(&f.start.run_id).unwrap(), m);
}

#[test]
fn new_untested_revision_or_input_and_stale_evidence_never_admit_writes() {
    for case in ["revision", "inputs", "expiry"] {
        let mut start = request();
        if case == "revision" {
            start.inputs.get_mut("delivery").unwrap()["source_revision"]["revision"] =
                "b".repeat(40).into();
        }
        if case == "inputs" {
            start.inputs.get_mut("delivery").unwrap()["input_digest"] =
                format!("sha256:{}", "b".repeat(64)).into();
        }
        let f = checked(start, 100);
        assert!(
            f.store()
                .claim_effect(
                    &f.lease,
                    &Time(Cell::new(if case == "expiry" { 1100 } else { 1001 }))
                )
                .is_err(),
            "{case}"
        );
        assert!(
            f.store()
                .effects(&f.start.run_id, 0, 100)
                .unwrap()
                .items
                .is_empty()
        );
    }
}

#[test]
fn grant_expiry_and_pause_fence_dispatch_but_query_remains_possible() {
    let f = checked(request(), 100);
    let p = prepared(&f, 1001);
    assert_eq!(p.deadline_unix_ms, 1100);
    let mut s = f.store();
    assert!(s.validate_effect_delivery(&p, 1100).is_err());
    let current = s.get(&f.start.run_id).unwrap();
    s.apply(&event(
        &current,
        "pause",
        EventKind::Pause {
            reason: "hold delivery".into(),
        },
    ))
    .unwrap();
    assert!(s.validate_effect_delivery(&p, 1001).is_err());
    let snapshot = s.get(&f.start.run_id).unwrap();
    let migration = MigrationRequest {
        migration_id: "remove-gates".into(),
        target_bundle: request().bundle,
        target_inputs: request().inputs,
        execution_policy: workflow_kernel::MigrationExecutionPolicy::RestartWithFreshEvidence,
        timer_policy: workflow_kernel::MigrationTimerPolicy::CancelAndRearmOnResume,
        node_mapping: vec![],
        decision_summary: "delete protected checks".into(),
    };
    assert!(s.plan_migration(&f.start.run_id, &migration).is_err());
    s.apply(&event(
        &snapshot,
        "resume",
        EventKind::Resume {
            reason: "reconcile only".into(),
        },
    ))
    .unwrap();
    let clock = Time(Cell::new(11001));
    let lease = s
        .acquire(
            &LeaseRequest {
                run_id: f.start.run_id.clone(),
                owner: "successor".into(),
                acquisition_id: "next".into(),
                ttl_ms: 10000,
            },
            &clock,
        )
        .unwrap();
    let EffectClaim::Call { attempt } = s.claim_effect(&lease, &clock).unwrap() else {
        panic!("query")
    };
    assert_eq!(attempt.kind, CallKind::Query);
    assert!(attempt.release.is_none());
    s.validate_effect_delivery(&attempt, 11001).unwrap();
}

#[test]
fn report_bytes_modified_after_pass_block_claim_and_manifest() {
    let f = checked(request(), 60000);
    let p = prepared(&f, 1001);
    assert_eq!(
        f.store()
            .acceptance(&f.start.run_id)
            .unwrap()
            .artifacts
            .len(),
        1
    );
    fn corrupt_payload(dir: &std::path::Path) -> bool {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if corrupt_payload(&path) {
                    return true;
                }
            } else if std::fs::read(&path).unwrap() == br#"{"valid":true}"# {
                std::fs::write(path, b"altered").unwrap();
                return true;
            }
        }
        false
    }
    assert!(corrupt_payload(&f.root()));
    assert!(f.store().validate_effect_delivery(&p, 1001).is_err());
    assert!(f.store().acceptance(&f.start.run_id).is_err());
}
