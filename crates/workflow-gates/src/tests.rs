use super::*;
use std::collections::BTreeMap;
use workflow_artifacts::*;
use workflow_ir::{ValueType, VersionRef};
fn version(id: &str) -> VersionRef {
    VersionRef {
        id: id.into(),
        version: "1.0.0".into(),
    }
}
fn sha(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
#[derive(Clone)]
struct Source {
    report: ArtifactRef,
    execution: Option<ExecutedCheck>,
    unavailable: bool,
    artifacts: BTreeMap<String, ArtifactRef>,
}

#[test]
fn input_invalidation_revokes_prior_gate_pass_until_fresh_evidence_is_bound() {
    let (mut request, mut source) = fixture();
    let mut spec = source.report.manifest.spec.clone();
    spec.artifact_type = ArtifactType {
        identity: version("requirements"),
        content: ContentSchema::Utf8,
    };
    let old = reference(&spec, b"old requirement").unwrap();
    let new = reference(&spec, b"changed requirement").unwrap();
    source
        .artifacts
        .insert(old.artifact_id.clone(), old.clone());
    source
        .artifacts
        .insert(new.artifact_id.clone(), new.clone());
    let mut report_spec = source.report.manifest.spec.clone();
    report_spec.inputs = vec![old.link()];
    source.report = reference(&report_spec, br#"{"passed":true}"#).unwrap();
    source.execution.as_mut().unwrap().evidence = vec![source.report.link()];
    request.target.artifacts = vec![old.link()];
    request.evidence[0].report = source.report.link();
    let prior = evaluate(&request, &source, 200).unwrap();
    assert_eq!(prior.verdict, Verdict::Pass);
    let plan = RevalidationPlan {
        schema_version: 1,
        policy: RevalidationPolicy::RequireFreshEvidence,
        inventory_digest: sha('f'),
        replacements: vec![Replacement {
            old: old.link(),
            new: new.link(),
        }],
        affected: vec![],
        decision_summary: "Changed requirement requires new test execution".into(),
    };
    struct Current {
        reader: InvalidatedReader,
        execution: Source,
    }
    impl ArtifactReader for Current {
        fn verify(&self, link: &ArtifactLink) -> Result<ArtifactRef> {
            self.reader.verify(link)
        }
    }
    impl EvidenceSource for Current {
        fn executed_check(&self, producer: &Producer) -> Result<Option<ExecutedCheck>> {
            self.execution.executed_check(producer)
        }
    }
    let current = Current {
        reader: InvalidatedReader::new(Box::new(source.clone()), &plan).unwrap(),
        execution: source.clone(),
    };
    assert_eq!(
        evaluate(&request, &current, 201).unwrap().verdict,
        Verdict::Unknown
    );
    assert_eq!(
        revalidate(&request, &prior, &current, 201)
            .unwrap_err()
            .code,
        ErrorCode::InvalidReference
    );
    assert_eq!(
        evaluate(&request, &source, 201).unwrap().verdict,
        Verdict::Pass
    );
    // Fresh execution under the changed common input restores eligibility.
    report_spec.inputs = vec![new.link()];
    report_spec.producer.attempt_id = "attempt-fresh".into();
    report_spec.producer.input_digest = sha('e');
    source.report = reference(&report_spec, br#"{"passed":true}"#).unwrap();
    let execution = source.execution.as_mut().unwrap();
    execution.producer = report_spec.producer.clone();
    execution.evidence = vec![source.report.link()];
    request.target.input_digest = sha('e');
    request.target.artifacts = vec![new.link()];
    request.evidence[0].report = source.report.link();
    let current = Current {
        reader: InvalidatedReader::new(Box::new(source.clone()), &plan).unwrap(),
        execution: source,
    };
    assert_eq!(
        evaluate(&request, &current, 202).unwrap().verdict,
        Verdict::Pass
    );
}
impl ArtifactReader for Source {
    fn verify(&self, link: &ArtifactLink) -> Result<ArtifactRef> {
        if self.unavailable {
            return Err(Error::new(ErrorCode::NotFound, "missing"));
        }
        if *link == self.report.link() {
            return Ok(self.report.clone());
        }
        self.artifacts
            .get(&link.artifact_id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "missing"))
    }
}
impl EvidenceSource for Source {
    fn executed_check(&self, _: &Producer) -> Result<Option<ExecutedCheck>> {
        Ok(self.execution.clone())
    }
}
fn fixture() -> (Request, Source) {
    let ty = ArtifactType {
        identity: version("tests.report"),
        content: ContentSchema::Json {
            value_type: ValueType::Object {
                fields: BTreeMap::from([("passed".into(), ValueType::Boolean)]),
            },
        },
    };
    let producer = Producer {
        run_id: "run-1".into(),
        node_instance_id: "instance-1".into(),
        attempt_id: "attempt-1".into(),
        request_digest: sha('a'),
        input_digest: sha('b'),
    };
    let source_revision = SourceRevision {
        repository: "repository".into(),
        revision: "a".repeat(40),
    };
    // This payload falsely says PASS in the failure tests. The accepted output decides.
    let report = reference(
        &PublishSpec {
            schema_version: 1,
            artifact_type: ty.clone(),
            producer: producer.clone(),
            source_revision: source_revision.clone(),
            inputs: vec![],
            access: AccessScope::Run {
                run_id: producer.run_id.clone(),
            },
            retention: Retention::RunDependency,
        },
        br#"{"passed":true}"#,
    )
    .unwrap();
    let execution = ExecutedCheck {
        producer: producer.clone(),
        run_digest: sha('c'),
        node_id: "test".into(),
        capability: version("tests.execute"),
        contract_digest: sha('d'),
        completed_at_unix_ms: 100,
        settled_at_unix_ms: 110,
        outcome: CheckOutcome::Succeeded {
            outputs: BTreeMap::from([("passed".into(), true.into())]),
        },
        evidence: vec![report.link()],
    };
    let request = Request {
        policy: Policy {
            schema_version: 1,
            identity: version("release.quality"),
            requirements: vec![Requirement {
                id: "tests".into(),
                node_id: "test".into(),
                capability: execution.capability.clone(),
                contract_digest: execution.contract_digest.clone(),
                report_type: ty,
                pass_field: "passed".into(),
                max_age_ms: 1000,
            }],
        },
        target: Target {
            run_id: producer.run_id,
            run_digest: execution.run_digest.clone(),
            action: version("release.publish"),
            source_revision,
            input_digest: producer.input_digest,
            artifacts: vec![],
        },
        evidence: vec![Evidence {
            requirement_id: "tests".into(),
            report: report.link(),
        }],
    };
    (
        request,
        Source {
            report,
            execution: Some(execution),
            unavailable: false,
            artifacts: BTreeMap::new(),
        },
    )
}
fn reason(r: &Request, s: &Source, now: u64, verdict: Verdict, expected: Reason) {
    let evaluator: &dyn PolicyEvaluator = &DeterministicPolicyEvaluator;
    let d = evaluator.evaluate(r, s, now).unwrap();
    assert_eq!(d.verdict, verdict);
    assert_eq!(d.checks[0].reason, expected);
    if verdict != Verdict::Pass {
        assert_eq!(d.expires_at_unix_ms, None);
    }
}
#[test]
fn pass_binds_the_complete_request_and_has_an_exclusive_expiry() {
    let (r, s) = fixture();
    let d = evaluate(&r, &s, 200).unwrap();
    assert_eq!(d.verdict, Verdict::Pass);
    assert_eq!(d.expires_at_unix_ms, Some(1100));
    assert_eq!(d.request_digest, digest(&r).unwrap());
    assert_eq!(revalidate(&r, &d, &s, 1099).unwrap().verdict, Verdict::Pass);
    assert_eq!(
        revalidate(&r, &d, &s, 1100).unwrap().verdict,
        Verdict::Unknown
    );
    let roundtrip: Request = parse_message(&to_message(&r).unwrap()).unwrap();
    assert_eq!(evaluate(&roundtrip, &s, 200).unwrap(), d);
}
#[test]
fn payload_claims_cannot_override_accepted_false_or_failed_work() {
    let (r, mut s) = fixture();
    s.execution.as_mut().unwrap().outcome = CheckOutcome::Succeeded {
        outputs: BTreeMap::from([("passed".into(), false.into())]),
    };
    reason(&r, &s, 200, Verdict::Fail, Reason::CheckFailed);
    s.execution.as_mut().unwrap().outcome = CheckOutcome::Failed {
        code: "test_failed".into(),
    };
    reason(&r, &s, 200, Verdict::Fail, Reason::CheckFailed);
    s.execution.as_mut().unwrap().outcome = CheckOutcome::Inconclusive {
        code: "timed_out".into(),
    };
    reason(&r, &s, 200, Verdict::Unknown, Reason::CheckInconclusive);
    for value in [serde_json::Value::Null, "PASS".into(), 1.into()] {
        s.execution.as_mut().unwrap().outcome = CheckOutcome::Succeeded {
            outputs: BTreeMap::from([("passed".into(), value)]),
        };
        reason(&r, &s, 200, Verdict::Unknown, Reason::MissingBoolean);
    }
    s.execution.as_mut().unwrap().outcome = CheckOutcome::Succeeded {
        outputs: BTreeMap::new(),
    };
    reason(&r, &s, 200, Verdict::Unknown, Reason::MissingBoolean);
}
#[test]
fn absent_uncommitted_or_substituted_evidence_never_passes() {
    let (r, s) = fixture();
    let mut absent = r.clone();
    absent.evidence.clear();
    reason(&absent, &s, 200, Verdict::Unknown, Reason::MissingEvidence);
    let mut missing = s.clone();
    missing.unavailable = true;
    reason(
        &r,
        &missing,
        200,
        Verdict::Unknown,
        Reason::EvidenceUnavailable,
    );
    let mut missing = s.clone();
    missing.execution = None;
    reason(
        &r,
        &missing,
        200,
        Verdict::Unknown,
        Reason::UnrecordedEvidence,
    );
    let mut missing = s.clone();
    missing.execution.as_mut().unwrap().evidence.clear();
    reason(
        &r,
        &missing,
        200,
        Verdict::Unknown,
        Reason::UnrecordedEvidence,
    );
    let mut wrong = s.clone();
    wrong.execution.as_mut().unwrap().producer.attempt_id = "other-attempt".into();
    reason(&r, &wrong, 200, Verdict::Unknown, Reason::ProducerMismatch);
    let mut wrong = s.clone();
    wrong.report.manifest.content_digest = sha('0');
    reason(
        &r,
        &wrong,
        200,
        Verdict::Unknown,
        Reason::EvidenceUnavailable,
    );
}
#[test]
fn changed_source_inputs_run_or_tool_invalidates_old_evidence() {
    let (r, s) = fixture();
    for mutate in [
        |r: &mut Request| r.target.source_revision.revision = "b".repeat(40),
        |r: &mut Request| r.target.source_revision.repository = "other".into(),
        |r: &mut Request| r.target.input_digest = sha('e'),
        |r: &mut Request| r.target.run_id = "run-2".into(),
    ] {
        let mut x = r.clone();
        mutate(&mut x);
        reason(&x, &s, 200, Verdict::Unknown, Reason::TargetMismatch);
    }
    let mut x = r.clone();
    x.target.run_digest = sha('e');
    reason(&x, &s, 200, Verdict::Unknown, Reason::ProducerMismatch);
    for mutate in [
        |r: &mut Request| r.policy.requirements[0].capability.version = "2.0.0".into(),
        |r: &mut Request| r.policy.requirements[0].contract_digest = sha('e'),
        |r: &mut Request| r.policy.requirements[0].node_id = "other".into(),
    ] {
        let mut x = r.clone();
        mutate(&mut x);
        reason(&x, &s, 200, Verdict::Unknown, Reason::ToolMismatch);
    }
    let mut x = r.clone();
    x.policy.requirements[0].report_type.identity.version = "2.0.0".into();
    reason(&x, &s, 200, Verdict::Unknown, Reason::TypeMismatch);
}
#[test]
fn future_late_expired_and_overflowed_timestamps_are_unknown() {
    let (r, s) = fixture();
    reason(&r, &s, 100, Verdict::Unknown, Reason::InvalidTime);
    reason(&r, &s, 1100, Verdict::Unknown, Reason::Expired);
    for (completed, settled) in [(0, 110), (111, 110), (u64::MAX, u64::MAX)] {
        let mut x = s.clone();
        let e = x.execution.as_mut().unwrap();
        e.completed_at_unix_ms = completed;
        e.settled_at_unix_ms = settled;
        reason(&r, &x, u64::MAX, Verdict::Unknown, Reason::InvalidTime);
    }
    assert!(evaluate(&r, &s, 0).is_err());
}
#[test]
fn changed_action_policy_evidence_or_fabricated_decision_cannot_be_reused() {
    let (r, s) = fixture();
    let d = evaluate(&r, &s, 200).unwrap();
    for mutate in [
        |r: &mut Request| r.target.action.version = "2.0.0".into(),
        |r: &mut Request| r.policy.requirements[0].max_age_ms = 2000,
        |r: &mut Request| r.evidence.clear(),
    ] {
        let mut x = r.clone();
        mutate(&mut x);
        assert!(revalidate(&x, &d, &s, 201).is_err());
    }
    let mut fake = d.clone();
    fake.expires_at_unix_ms = Some(u64::MAX);
    assert!(revalidate(&r, &fake, &s, 201).is_err());
    let mut fake = d.clone();
    fake.checks.clear();
    assert!(revalidate(&r, &fake, &s, 201).is_err());
    assert!(revalidate(&r, &d, &s, 199).is_err());
    let mut corrupt = s.clone();
    corrupt.unavailable = true;
    assert!(revalidate(&r, &d, &corrupt, 201).is_err());
}
#[test]
fn all_requirements_are_mandatory_and_failure_wins_over_unknown() {
    let (mut r, mut s) = fixture();
    let mut q = r.policy.requirements[0].clone();
    q.id = "lint".into();
    r.policy.requirements.push(q);
    assert_eq!(evaluate(&r, &s, 200).unwrap().verdict, Verdict::Unknown);
    s.execution.as_mut().unwrap().outcome = CheckOutcome::Failed {
        code: "test_failed".into(),
    };
    assert_eq!(evaluate(&r, &s, 200).unwrap().verdict, Verdict::Fail);
    r.policy.requirements.clear();
    assert!(evaluate(&r, &s, 200).is_err());
}
#[test]
fn strict_shapes_and_budgets_reject_ambiguous_or_vacuous_policies() {
    let (r, s) = fixture();
    for mutate in [
        |r: &mut Request| r.evidence.push(r.evidence[0].clone()),
        |r: &mut Request| r.evidence[0].requirement_id = "unknown".into(),
        |r: &mut Request| r.policy.requirements.push(r.policy.requirements[0].clone()),
        |r: &mut Request| r.policy.requirements[0].max_age_ms = 0,
        |r: &mut Request| r.policy.requirements[0].max_age_ms = u64::MAX,
    ] {
        let mut x = r.clone();
        mutate(&mut x);
        assert!(evaluate(&x, &s, 200).is_err());
    }
    assert!(parse_message::<Request>(br#"{"policy":{},"policy":{}}"#).is_err());
    let mut value = serde_json::to_value(&r).unwrap();
    value["approved"] = true.into();
    assert!(parse_message::<Request>(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut deep = ValueType::Boolean;
    for _ in 0..200 {
        deep = ValueType::Array {
            items: Box::new(deep),
        };
    }
    let mut x = r;
    x.policy.requirements[0].report_type.content = ContentSchema::Json { value_type: deep };
    assert_eq!(evaluate(&x, &s, 200).unwrap_err().code, ErrorCode::Budget);
}

#[test]
fn target_artifacts_must_be_verified_in_scope_and_be_exact_report_inputs() {
    let (mut r, mut s) = fixture();
    let mut spec = s.report.manifest.spec.clone();
    spec.artifact_type = ArtifactType {
        identity: version("build.binary"),
        content: ContentSchema::Bytes,
    };
    let artifact = reference(&spec, b"build output").unwrap();
    r.target.artifacts.push(artifact.link());
    s.artifacts
        .insert(artifact.artifact_id.clone(), artifact.clone());
    reason(&r, &s, 200, Verdict::Unknown, Reason::TargetMismatch);
    s.report.manifest.spec.inputs = r.target.artifacts.clone();
    s.report = from_manifest(s.report.manifest.clone()).unwrap();
    r.evidence[0].report = s.report.link();
    s.execution.as_mut().unwrap().evidence = vec![s.report.link()];
    let d = evaluate(&r, &s, 200).unwrap();
    assert_eq!(d.verdict, Verdict::Pass);
    assert_eq!(d.artifacts.len(), 1);
    s.artifacts.clear();
    let d = evaluate(&r, &s, 200).unwrap();
    assert_eq!(d.verdict, Verdict::Unknown);
    assert_eq!(d.artifacts[0].reason, Reason::ArtifactUnavailable);
    // Even a reader substituting another valid manifest cannot satisfy this link.
    let mut other = artifact.manifest.spec.clone();
    other.producer.run_id = "other-run".into();
    other.access = AccessScope::Run {
        run_id: "other-run".into(),
    };
    s.artifacts.insert(
        artifact.artifact_id,
        reference(&other, b"build output").unwrap(),
    );
    assert_eq!(evaluate(&r, &s, 200).unwrap().verdict, Verdict::Unknown);
    r.target.artifacts.push(r.target.artifacts[0].clone());
    assert!(evaluate(&r, &s, 200).is_err());
}
#[test]
fn empty_or_untyped_reports_cannot_satisfy_a_requirement() {
    let (mut r, s) = fixture();
    for bytes in [
        b"".as_slice(),
        b"{}".as_slice(),
        br#"{"passed":"true"}"#.as_slice(),
    ] {
        assert!(reference(&s.report.manifest.spec, bytes).is_err());
    }
    r.policy.requirements[0].report_type.content = ContentSchema::Bytes;
    assert!(evaluate(&r, &s, 200).is_err());
    r.policy.requirements[0].report_type.content = ContentSchema::Json {
        value_type: ValueType::Object {
            fields: BTreeMap::new(),
        },
    };
    assert!(evaluate(&r, &s, 200).is_err());
}
