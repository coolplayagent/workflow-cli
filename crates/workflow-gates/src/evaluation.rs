use crate::*;
use workflow_artifacts::{AccessScope, ArtifactLink, ArtifactRef, validate_ref};

fn verified(source: &dyn EvidenceSource, link: &ArtifactLink) -> Result<ArtifactRef> {
    let r = source.verify(link)?;
    validate_ref(&r)?;
    if r.link() != *link {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "reader returned another artifact",
        ));
    }
    Ok(r)
}
fn finding(q: &Requirement, evidence: Option<&Evidence>) -> Finding {
    Finding {
        requirement_id: q.id.clone(),
        report: evidence.map(|e| e.report.clone()),
        verdict: Verdict::Unknown,
        reason: Reason::MissingEvidence,
        producer: None,
        completed_at_unix_ms: None,
        expires_at_unix_ms: None,
    }
}
fn check(r: &Request, q: &Requirement, source: &dyn EvidenceSource, now: u64) -> Finding {
    let evidence = r.evidence.iter().find(|e| e.requirement_id == q.id);
    let mut f = finding(q, evidence);
    let Some(e) = evidence else {
        return f;
    };
    let report = match verified(source, &e.report) {
        Ok(r) => r,
        Err(_) => {
            f.reason = Reason::EvidenceUnavailable;
            return f;
        }
    };
    let spec = &report.manifest.spec;
    let reason = if spec.artifact_type != q.report_type {
        Some(Reason::TypeMismatch)
    } else if spec.producer.run_id != r.target.run_id
        || spec.source_revision != r.target.source_revision
        || spec.producer.input_digest != r.target.input_digest
        || !validation::same_links(&spec.inputs, &r.target.artifacts)
    {
        Some(Reason::TargetMismatch)
    } else {
        None
    };
    if let Some(reason) = reason {
        f.reason = reason;
        return f;
    }
    let execution = match source.executed_check(&spec.producer) {
        Ok(Some(x)) => x,
        Ok(None) => {
            f.reason = Reason::UnrecordedEvidence;
            return f;
        }
        Err(_) => {
            f.reason = Reason::EvidenceUnavailable;
            return f;
        }
    };
    let reason =
        if execution.producer != spec.producer || execution.run_digest != r.target.run_digest {
            Some(Reason::ProducerMismatch)
        } else if execution.node_id != q.node_id
            || execution.capability != q.capability
            || execution.contract_digest != q.contract_digest
        {
            Some(Reason::ToolMismatch)
        } else if !execution.evidence.contains(&e.report) {
            Some(Reason::UnrecordedEvidence)
        } else {
            None
        };
    if let Some(reason) = reason {
        f.reason = reason;
        return f;
    }
    f.producer = Some(execution.producer);
    f.completed_at_unix_ms = Some(execution.completed_at_unix_ms);
    let expires = execution.completed_at_unix_ms.checked_add(q.max_age_ms);
    if execution.completed_at_unix_ms == 0
        || execution.completed_at_unix_ms > execution.settled_at_unix_ms
        || execution.settled_at_unix_ms > now
        || expires.is_none()
    {
        f.reason = Reason::InvalidTime;
        return f;
    }
    f.expires_at_unix_ms = expires;
    if now >= expires.expect("checked") {
        f.reason = Reason::Expired;
        return f;
    }
    (f.verdict, f.reason) = match &execution.outcome {
        CheckOutcome::Failed { .. } => (Verdict::Fail, Reason::CheckFailed),
        CheckOutcome::Inconclusive { .. } => (Verdict::Unknown, Reason::CheckInconclusive),
        CheckOutcome::Succeeded { outputs } => match outputs
            .get(&q.pass_field)
            .and_then(serde_json::Value::as_bool)
        {
            Some(true) => (Verdict::Pass, Reason::Passed),
            Some(false) => (Verdict::Fail, Reason::CheckFailed),
            None => (Verdict::Unknown, Reason::MissingBoolean),
        },
    };
    f
}
/// Evaluates the exact request using only a trusted evidence port and an explicit host time.
/// Required checks are ANDed. Any confirmed failure wins over UNKNOWN; only all PASS qualifies.
pub fn evaluate(r: &Request, source: &dyn EvidenceSource, now: u64) -> Result<Decision> {
    validate(r)?;
    if now == 0 {
        return Err(Error::new(
            ErrorCode::InvalidContract,
            "positive host time required",
        ));
    }
    let mut checks: Vec<_> = r
        .policy
        .requirements
        .iter()
        .map(|q| check(r, q, source, now))
        .collect();
    checks.sort_by(|a, b| a.requirement_id.cmp(&b.requirement_id));
    let mut artifacts: Vec<_> = r
        .target
        .artifacts
        .iter()
        .map(|link| {
            let pass = verified(source, link).is_ok_and(|artifact| {
                artifact.manifest.spec.access
                    == (AccessScope::Run {
                        run_id: r.target.run_id.clone(),
                    })
            });
            ArtifactFinding {
                artifact: link.clone(),
                verdict: if pass {
                    Verdict::Pass
                } else {
                    Verdict::Unknown
                },
                reason: if pass {
                    Reason::Passed
                } else {
                    Reason::ArtifactUnavailable
                },
            }
        })
        .collect();
    artifacts.sort_by(|a, b| a.artifact.artifact_id.cmp(&b.artifact.artifact_id));
    let verdict = if checks.iter().any(|x| x.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if checks.iter().all(|x| x.verdict == Verdict::Pass)
        && artifacts.iter().all(|x| x.verdict == Verdict::Pass)
    {
        Verdict::Pass
    } else {
        Verdict::Unknown
    };
    let expires_at_unix_ms = if verdict == Verdict::Pass {
        checks.iter().filter_map(|x| x.expires_at_unix_ms).min()
    } else {
        None
    };
    Ok(Decision {
        schema_version: 1,
        request_digest: digest(r)?,
        policy_digest: digest(&r.policy)?,
        target_digest: digest(&r.target)?,
        evaluated_at_unix_ms: now,
        expires_at_unix_ms,
        verdict,
        checks,
        artifacts,
    })
}
/// Recomputes the old observation (detecting fabricated fields), then checks fresh evidence.
/// The caller must freshly observe the actual target and atomically bind any effect itself.
pub fn revalidate(
    r: &Request,
    prior: &Decision,
    source: &dyn EvidenceSource,
    now: u64,
) -> Result<Decision> {
    validate(r)?;
    if prior.verdict != Verdict::Pass
        || prior.request_digest != digest(r)?
        || now < prior.evaluated_at_unix_ms
        || evaluate(r, source, prior.evaluated_at_unix_ms)? != *prior
    {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "prior PASS does not match the exact request and verified execution evidence",
        ));
    }
    evaluate(r, source, now)
}
