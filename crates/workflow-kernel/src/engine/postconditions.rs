use super::*;
use workflow_gates::{Reason, Verdict};
fn rejected(message: &str) -> Error {
    Error::new(ErrorCode::InvalidGateResult, message)
}
impl Engine {
    fn gate_value(&self, frame: u64, id: &str, b: &Binding) -> Result<serde_json::Value> {
        let f = &self.state.frames[&frame];
        let value = match b {
            Binding::Literal { value } => Some(value),
            Binding::WorkflowInput { field } => f.inputs.get(field),
            Binding::NodeOutput { node, field } => {
                let n = &f.nodes[node];
                if node == id || n.state == NodeState::Succeeded {
                    n.outputs.get(field)
                } else {
                    None
                }
            }
        };
        value
            .cloned()
            .ok_or_else(|| rejected("postcondition target binding is unavailable"))
    }
    pub(super) fn begin_gate(
        &mut self,
        frame: u64,
        id: &str,
        commands: &mut Vec<Command>,
    ) -> Result<bool> {
        match self.prepare_gate(frame, id, commands) {
            Err(e) if e.code == ErrorCode::InvalidGateResult => {
                // Preserve the actual task observation. Invalid dynamic target
                // data is a business failure, not a reason to repeat execution.
                self.finish(
                    frame,
                    id,
                    NodeState::Failed,
                    None,
                    Some(format!("postcondition_target: {}", e.message)),
                )?;
                Ok(true)
            }
            result => result,
        }
    }
    fn prepare_gate(&mut self, frame: u64, id: &str, commands: &mut Vec<Command>) -> Result<bool> {
        let f = &self.state.frames[&frame];
        let Some(g) = self
            .bundle
            .spec()
            .postconditions
            .iter()
            .find(|g| g.workflow == f.workflow && g.node_id == id)
            .cloned()
        else {
            return Ok(false);
        };
        let source = workflow_artifacts::SourceRevision {
            repository: self
                .gate_value(frame, id, &g.repository)?
                .as_str()
                .ok_or_else(|| rejected("repository string required"))?
                .into(),
            revision: self
                .gate_value(frame, id, &g.revision)?
                .as_str()
                .ok_or_else(|| rejected("revision string required"))?
                .into(),
        };
        let artifacts = serde_json::from_value(self.gate_value(frame, id, &g.artifacts)?)
            .map_err(|_| rejected("artifact links required"))?;
        let context = GateContext {
            expected_instances: g
                .policy
                .requirements
                .iter()
                .map(|q| (q.id.clone(), f.nodes[&q.node_id].instance_id))
                .collect(),
            target: workflow_gates::Target {
                run_id: self.state.run_id.clone(),
                run_digest: self.state.run_digest.clone(),
                action: g.action,
                source_revision: source,
                input_digest: workflow_worker::digest(&f.nodes[&g.input_node].inputs)?,
                artifacts,
            },
            policy: g.policy,
        };
        workflow_gates::validate(&workflow_gates::Request {
            policy: context.policy.clone(),
            target: context.target.clone(),
            evidence: vec![],
        })?;
        self.touch()?;
        self.record_mut(frame, id).state = NodeState::CheckingGate {
            context: Box::new(context.clone()),
            awaiting: true,
        };
        commands.push(Command::CheckGate {
            instance_id: self.record(frame, id).instance_id,
            context: Box::new(context),
        });
        Ok(true)
    }
    pub(super) fn gate_evaluated(
        &mut self,
        instance: u64,
        context_digest: &str,
        evaluation: &GateEvaluation,
    ) -> Result<()> {
        let (frame, id) = self.locate(instance)?;
        let NodeState::CheckingGate {
            context,
            awaiting: true,
        } = &self.record(frame, &id).state
        else {
            return Err(rejected("gate is not awaiting a decision"));
        };
        if context_digest != workflow_worker::digest(context)? {
            return Err(rejected("gate context changed"));
        }
        validate_evaluation(context, evaluation, self.state.now_unix_ms)?;
        let context = context.clone();
        self.touch()?;
        self.record_mut(frame, &id).gate_decision = Some(Box::new(evaluation.decision.clone()));
        match evaluation.decision.verdict {
            Verdict::Pass => self.finish(
                frame,
                &id,
                NodeState::Succeeded,
                None,
                Some("postcondition_pass".into()),
            ),
            Verdict::Fail => self.finish(
                frame,
                &id,
                NodeState::Failed,
                None,
                Some("postcondition_fail".into()),
            ),
            Verdict::Unknown => {
                let record = self.record_mut(frame, &id);
                record.state = NodeState::CheckingGate {
                    context,
                    awaiting: false,
                };
                record.reason = Some("postcondition_unknown".into());
                Ok(())
            }
        }
    }
    pub(super) fn retry_gate(
        &mut self,
        instance: u64,
        context_digest: &str,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        let (frame, id) = self.locate(instance)?;
        let NodeState::CheckingGate {
            context,
            awaiting: false,
        } = &self.record(frame, &id).state
        else {
            return Err(rejected("only an observed UNKNOWN gate can be retried"));
        };
        if context_digest != workflow_worker::digest(context)? {
            return Err(rejected("retry belongs to another gate target"));
        }
        let context = context.clone();
        self.touch()?;
        self.record_mut(frame, &id).state = NodeState::CheckingGate {
            context: context.clone(),
            awaiting: true,
        };
        commands.push(Command::CheckGate {
            instance_id: instance,
            context,
        });
        Ok(())
    }
}
/// Structural validation of a trusted host observation. Storage additionally
/// recomputes it from its own verified ledger; raw simulation events are not proof.
fn validate_evaluation(context: &GateContext, e: &GateEvaluation, now: u64) -> Result<()> {
    workflow_gates::validate(&e.request)?;
    let d = &e.decision;
    if e.request.policy != context.policy
        || e.request.target != context.target
        || d.schema_version != 1
        || d.evaluated_at_unix_ms != now
        || d.request_digest != workflow_gates::digest(&e.request)?
        || d.policy_digest != workflow_gates::digest(&context.policy)?
        || d.target_digest != workflow_gates::digest(&context.target)?
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
