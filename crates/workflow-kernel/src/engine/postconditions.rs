use super::*;
use workflow_gates::Verdict;
fn rejected(message: &str) -> Error {
    Error::new(ErrorCode::InvalidGateResult, message)
}
impl Engine {
    /// Resolve only an applied, authenticated-by-ingress approval in this exact
    /// frame. A digest value or human annotation without that inbox fact is not
    /// permission. Ordinary and exception approvals retain distinct identities.
    pub fn approval_evidence(
        &self,
        frame_id: u64,
        requirement: &workflow_gates::ApprovalRequirement,
        policy: &workflow_gates::Policy,
        target: &workflow_gates::Target,
        now: u64,
    ) -> Result<Option<workflow_gates::ApprovalEvidence>> {
        let frame = self
            .state
            .frames
            .get(&frame_id)
            .ok_or_else(|| rejected("approval frame missing"))?;
        let node = frame
            .nodes
            .get(&requirement.node_id)
            .ok_or_else(|| rejected("approval node missing"))?;
        let wait = self
            .bundle
            .wait_policy(&frame.workflow, &requirement.node_id)
            .filter(|p| {
                p.kind == WaitKind::HumanApproval
                    && p.subjects.get(&requirement.subject_field) == Some(&SubjectKind::Digest)
            })
            .ok_or_else(|| rejected("declared human approval digest subject required"))?;
        if node.state != NodeState::Succeeded {
            return Ok(None);
        }
        let review = workflow_gates::review_digest(policy, target)?;
        if node
            .inputs
            .get(&requirement.subject_field)
            .and_then(serde_json::Value::as_str)
            != Some(&review)
        {
            return Err(rejected(
                "approval belongs to another policy, action or delivery subject",
            ));
        }
        let applied: Vec<_> = self
            .state
            .inbox
            .values()
            .filter(|e| {
                e.message.target.instance_id == node.instance_id
                    && e.message.decision == SignalDecision::Approve
                    && matches!(e.status, SignalStatus::Applied { .. })
            })
            .collect();
        if applied.len() != 1 {
            return Err(rejected("approval has no unique applied inbox fact"));
        }
        let entry = applied[0];
        let SignalStatus::Applied { at_unix_ms, .. } = entry.status else {
            unreachable!()
        };
        if now < at_unix_ms || now >= entry.message.expires_at_unix_ms {
            return Ok(None);
        }
        Ok(Some(workflow_gates::ApprovalEvidence {
            node_id: requirement.node_id.clone(),
            instance_id: node.instance_id,
            message_id: entry.message.message_id.clone(),
            correlation_id: entry.message.correlation_id.clone(),
            actor: entry.message.source.clone(),
            approval_policy: wait.identity.clone(),
            review_digest: review,
            reason: entry.message.reason.clone(),
            received_at_unix_ms: entry.received_at_unix_ms,
            applied_at_unix_ms: at_unix_ms,
            expires_at_unix_ms: entry.message.expires_at_unix_ms,
            exception: entry.message.exception.clone(),
        }))
    }
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
        let mut context = GateContext {
            exception: None,
            expected_instances: g
                .policy
                .requirements
                .iter()
                .map(|q| (q.id.clone(), f.nodes[&q.node_id].instance_id))
                .collect(),
            target: workflow_gates::Target {
                run_id: self.state.run_id.clone(),
                run_digest: self.state.run_digest.clone(),
                action: g.action.clone(),
                source_revision: source,
                input_digest: workflow_worker::digest(&f.nodes[&g.input_node].inputs)?,
                artifacts,
            },
            policy: g.policy.clone(),
        };
        if let Some(requirement) = &g.exception {
            context.exception = self
                .approval_evidence(
                    frame,
                    requirement,
                    &context.policy,
                    &context.target,
                    self.state.now_unix_ms,
                )?
                .filter(|approval| approval.exception.is_some());
        }
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
        workflow_gates::validate_evaluation(context, evaluation, self.state.now_unix_ms)
            .map_err(|e| rejected(&e.message))?;
        let context = context.clone();
        self.touch()?;
        self.record_mut(frame, &id).gate_decision = Some(Box::new(evaluation.decision.clone()));
        self.record_mut(frame, &id).gate_exception = evaluation.exception.clone().map(Box::new);
        if evaluation.exception.is_some() {
            return self.finish(
                frame,
                &id,
                NodeState::Succeeded,
                None,
                Some("postcondition_exception".into()),
            );
        }
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
