use crate::*;
use std::collections::BTreeSet;
fn rejected(message: &str) -> Error {
    Error::new(ErrorCode::InvalidContract, message)
}
/// Structural validation of a trusted host observation. Storage additionally
/// recomputes it from its own verified ledger; raw simulation events are not proof.
pub fn validate_evaluation(context: &GateContext, e: &GateEvaluation, now: u64) -> Result<()> {
    crate::validate(&e.request)?;
    if context.expected_instances.len() != context.policy.requirements.len()
        || context.policy.requirements.iter().any(|q| {
            context
                .expected_instances
                .get(&q.id)
                .is_none_or(|id| *id == 0)
        })
    {
        return Err(rejected(
            "gate context requires an exact positive instance for each requirement",
        ));
    }
    let d = &e.decision;
    let expected_exception = if d.verdict != Verdict::Pass {
        context
            .exception
            .as_ref()
            .filter(|a| a.exception.is_some() && now < a.expires_at_unix_ms)
    } else {
        None
    };
    let review_digest = crate::review_digest(&context.policy, &context.target)?;
    if e.exception.as_ref() != expected_exception
        || expected_exception
            .is_some_and(|a| a.applied_at_unix_ms > now || a.review_digest != review_digest)
    {
        return Err(rejected(
            "gate exception differs from the current scoped approval",
        ));
    }
    if e.request.policy != context.policy
        || e.request.target != context.target
        || d.schema_version != 1
        || d.evaluated_at_unix_ms != now
        || d.request_digest != crate::digest(&e.request)?
        || d.policy_digest != crate::digest(&context.policy)?
        || d.target_digest != crate::digest(&context.target)?
        || d.checks.len() != context.policy.requirements.len()
        || d.artifacts.len() != context.target.artifacts.len()
    {
        return Err(rejected(
            "gate decision does not bind the frozen policy, target, checks and time",
        ));
    }
    let mut seen = BTreeSet::new();
    for f in &d.checks {
        let q = context
            .policy
            .requirements
            .iter()
            .find(|q| q.id == f.requirement_id)
            .ok_or_else(|| rejected("unknown check finding"))?;
        let link = e
            .request
            .evidence
            .iter()
            .find(|e| e.requirement_id == q.id)
            .map(|e| &e.report);
        if !seen.insert(&q.id) || f.report.as_ref() != link {
            return Err(rejected("duplicate/mismatched check finding"));
        }
        if let Some(producer) = &f.producer
            && (producer.run_id != context.target.run_id
                || producer.input_digest != context.target.input_digest
                || producer.node_instance_id
                    != format!("instance-{}", context.expected_instances[&q.id]))
        {
            return Err(rejected(
                "gate evidence belongs to another run, input or node instance",
            ));
        }
        if f.verdict == Verdict::Pass {
            let completed = f
                .completed_at_unix_ms
                .ok_or_else(|| rejected("PASS requires completion time"))?;
            if f.reason != Reason::Passed
                || f.report.is_none()
                || f.producer.is_none()
                || completed == 0
                || completed > now
                || completed.checked_add(q.max_age_ms) != f.expires_at_unix_ms
                || f.expires_at_unix_ms.is_none_or(|expiry| now >= expiry)
            {
                return Err(rejected(
                    "PASS requires current verified evidence and exclusive expiry",
                ));
            }
        }
    }
    let mut seen = BTreeSet::new();
    for f in &d.artifacts {
        if !context.target.artifacts.contains(&f.artifact)
            || !seen.insert(&f.artifact.artifact_id)
            || (f.verdict == Verdict::Pass && f.reason != Reason::Passed)
        {
            return Err(rejected("invalid artifact finding"));
        }
    }
    let verdict = if d.checks.iter().any(|f| f.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if d.checks.iter().all(|f| f.verdict == Verdict::Pass)
        && d.artifacts.iter().all(|f| f.verdict == Verdict::Pass)
    {
        Verdict::Pass
    } else {
        Verdict::Unknown
    };
    let expiry = if verdict == Verdict::Pass {
        d.checks.iter().filter_map(|f| f.expires_at_unix_ms).min()
    } else {
        None
    };
    if verdict != d.verdict || expiry != d.expires_at_unix_ms {
        return Err(rejected(
            "gate aggregate verdict or expiry differs from mandatory checks",
        ));
    }
    Ok(())
}
