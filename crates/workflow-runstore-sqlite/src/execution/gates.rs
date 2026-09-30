use super::*;
use std::collections::{BTreeMap, BTreeSet};
use workflow_artifacts::{ArtifactLink, ArtifactReader, ArtifactRef, Producer};
use workflow_gates::{Evidence, EvidenceSource, ExecutedCheck, Request};
use workflow_kernel::{GateContext, GateEvaluation};

/// Cache verified immutable identities before computing the observation. I/O failures
/// abort the transaction rather than recording an UNKNOWN that recovery could reinterpret.
struct Source {
    artifacts: BTreeMap<String, ArtifactRef>,
    executions: BTreeMap<String, ExecutedCheck>,
}
impl ArtifactReader for Source {
    fn verify(&self, link: &ArtifactLink) -> workflow_artifacts::Result<ArtifactRef> {
        self.artifacts
            .get(&link.artifact_id)
            .filter(|r| r.link() == *link)
            .cloned()
            .ok_or_else(|| {
                workflow_artifacts::Error::new(
                    workflow_artifacts::ErrorCode::NotFound,
                    "unverified gate artifact",
                )
            })
    }
}
impl EvidenceSource for Source {
    fn executed_check(
        &self,
        producer: &Producer,
    ) -> workflow_artifacts::Result<Option<ExecutedCheck>> {
        Ok(self.executions.get(&producer.attempt_id).cloned())
    }
}
impl Source {
    fn load(
        &mut self,
        link: &ArtifactLink,
        reader: Option<&dyn ArtifactReader>,
    ) -> Result<ArtifactRef> {
        if let Some(r) = self.artifacts.get(&link.artifact_id) {
            if r.link() != *link {
                return Err(corrupt("gate artifact identity conflict"));
            }
            return Ok(r.clone());
        }
        let r = reader
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::ArtifactUnavailable,
                    "gate evidence requires an artifact reader",
                )
            })?
            .verify(link)?;
        workflow_artifacts::validate_ref(&r)?;
        if r.link() != *link {
            return Err(Error::new(
                ErrorCode::ArtifactRejected,
                "gate reader returned a different artifact",
            ));
        }
        self.artifacts.insert(r.artifact_id.clone(), r.clone());
        Ok(r)
    }
}
pub(crate) fn evaluate(
    r: &Recovered,
    a: &Authority,
    context: &GateContext,
    reader: Option<&dyn ArtifactReader>,
    now: u64,
    prior_revision: u64,
) -> Result<GateEvaluation> {
    let expected: BTreeSet<_> = context
        .expected_instances
        .values()
        .map(|id| format!("instance-{id}"))
        .collect();
    let mut source = Source {
        artifacts: BTreeMap::new(),
        executions: BTreeMap::new(),
    };
    for state in a.attempts.values() {
        let Some(AttemptOutcome::Finished {
            result,
            event_id,
            revision,
        }) = &state.outcome
        else {
            continue;
        };
        let producer = artifact_producer(&state.prepared.request)?;
        if !expected.contains(&producer.node_instance_id) {
            continue;
        }
        let event = r
            .events
            .iter()
            .find(|e| e.revision == *revision && e.event.event_id == *event_id)
            .ok_or_else(|| corrupt("check settlement event missing"))?;
        if event.revision > prior_revision {
            return Err(corrupt("gate execution prefix includes a later settlement"));
        }
        let check = executed_check(
            &context.target.run_digest,
            &state.prepared,
            result,
            event.event.at_unix_ms,
        )?;
        for link in &check.evidence {
            source.load(link, reader)?;
        }
        source
            .executions
            .insert(check.producer.attempt_id.clone(), check);
    }
    for link in &context.target.artifacts {
        source.load(link, reader)?;
    }
    let mut evidence = vec![];
    for q in &context.policy.requirements {
        let instance = format!("instance-{}", context.expected_instances[&q.id]);
        let checks: Vec<_> = source
            .executions
            .values()
            .filter(|e| e.producer.node_instance_id == instance)
            .collect();
        if checks.len() > 1 {
            return Err(corrupt("node instance has multiple settled checks"));
        }
        if let Some(check) = checks.first() {
            let reports: Vec<_> = check
                .evidence
                .iter()
                .filter(|link| {
                    source.artifacts[&link.artifact_id]
                        .manifest
                        .spec
                        .artifact_type
                        == q.report_type
                })
                .collect();
            // One typed report has an unambiguous role. Never guess among multiple attachments.
            if reports.len() == 1 {
                evidence.push(Evidence {
                    requirement_id: q.id.clone(),
                    report: reports[0].clone(),
                });
            }
        }
    }
    let request = Request {
        policy: context.policy.clone(),
        target: context.target.clone(),
        evidence,
    };
    use workflow_gates::PolicyEvaluator;
    let decision = workflow_gates::DeterministicPolicyEvaluator.evaluate(&request, &source, now)?;
    Ok(GateEvaluation { request, decision })
}
