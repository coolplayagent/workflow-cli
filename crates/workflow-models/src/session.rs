use crate::*;
use std::sync::Arc;
use workflow_worker::{
    AdapterExecution, AdapterOutcome, Capability, CapabilityAdapter, Clock, ExecutionGrant,
    Invocation, RequestContext, SystemClock, WorkRequest, Worker,
};

fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidResult, message)
}
fn bounded(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max && !s.contains('\0')
}

/// Deterministic record reducer. Replaying never invokes a model or capability.
pub struct Session {
    policy: Policy,
    request: WorkRequest,
    identity: ModelIdentity,
    deadline: u64,
    last_time: u64,
    events: Vec<ModelEvent>,
    calls: u32,
    tools: u32,
    pending: Option<WorkRequest>,
    outcome: Option<AdapterOutcome>,
}
pub enum Next {
    Model(Box<ModelCall>),
    Tool(Box<WorkRequest>),
    Complete,
}
impl Session {
    pub fn new(
        policy: Policy,
        request: WorkRequest,
        identity: ModelIdentity,
        deadline: u64,
    ) -> Result<Self> {
        let c = Capability::new(policy.spec().task.clone())?;
        workflow_worker::validate_request(
            &request,
            &ExecutionGrant::bind(&request)?,
            &c,
            request.issued_at_unix_ms,
        )?;
        if request.model_policy.as_ref() != Some(policy.binding())
            || deadline <= request.issued_at_unix_ms
            || deadline > request.deadline_unix_ms
        {
            return Err(invalid(
                "model session policy/deadline differs from prepared request",
            ));
        }
        workflow_worker::ModelPolicyBinding {
            policy: identity.adapter.clone(),
            digest: identity.binding_digest.clone(),
        }
        .validate()?;
        if !bounded(&identity.model, 256) {
            return Err(invalid("bounded configured model identity required"));
        }
        let last_time = request.issued_at_unix_ms;
        Ok(Self {
            policy,
            request,
            identity,
            deadline,
            last_time,
            events: vec![],
            calls: 0,
            tools: 0,
            pending: None,
            outcome: None,
        })
    }
    fn fail(&mut self, failure: ModelFailure) {
        self.outcome = Some(failure_outcome(&failure));
    }
    pub fn next_action(&mut self) -> Result<Next> {
        if self.outcome.is_some() {
            return Ok(Next::Complete);
        }
        if let Some(request) = &self.pending {
            return Ok(Next::Tool(Box::new(request.clone())));
        }
        let b = &self.policy.spec().budget;
        if self.calls >= b.model_calls {
            self.fail(ModelFailure::Budget);
            return Ok(Next::Complete);
        }
        let call = ModelCall {
            protocol_version: 1,
            request_digest: digest(&self.request)?,
            policy: self.policy.spec().clone(),
            inputs: self.request.inputs.clone(),
            events: self.events.clone(),
            remaining_model_calls: b.model_calls - self.calls,
            remaining_tool_calls: b.tool_calls - self.tools,
            deadline_unix_ms: self.deadline,
        };
        if to_message(&call)?.len() > b.context_bytes as usize {
            self.fail(ModelFailure::Budget);
            return Ok(Next::Complete);
        }
        Ok(Next::Model(Box::new(call)))
    }
    fn time(&mut self, at: u64) -> Result<()> {
        if at < self.last_time || at >= self.deadline {
            return Err(invalid("model record time is nonmonotonic or expired"));
        }
        self.last_time = at;
        Ok(())
    }
    pub fn replied(&mut self, call_digest: String, at: u64, response: Reply) -> Result<()> {
        let Next::Model(call) = self.next_action()? else {
            return Err(invalid("model reply has no pending model call"));
        };
        if digest(&call)? != call_digest {
            return Err(invalid("model reply belongs to another context"));
        }
        self.time(at)?;
        let response_size = to_message(&response)?.len();
        self.calls += 1;
        self.events.push(ModelEvent::Replied {
            call_digest,
            at_unix_ms: at,
            response: response.clone(),
        });
        if response_size > self.policy.spec().budget.response_bytes as usize {
            self.fail(ModelFailure::Budget);
            return Ok(());
        }
        let reply = match response {
            Reply::Failed { failure } => {
                self.fail(failure);
                return Ok(());
            }
            Reply::Received { reply } => reply,
        };
        if reply.proposal.protocol_version != 1
            || !bounded(&reply.resolved_model, 256)
            || !bounded(&reply.response_id, 256)
            || reply
                .usage
                .output_tokens
                .is_some_and(|n| n > u64::from(self.policy.spec().budget.output_tokens_per_call))
        {
            self.fail(ModelFailure::InvalidResponse);
            return Ok(());
        }
        match reply.proposal.action {
            Action::Complete { outputs, summary } => {
                if !bounded(&summary, 8192)
                    || workflow_validator::validate_values(
                        &self.policy.spec().task.outputs,
                        &outputs,
                    )
                    .is_err()
                {
                    self.fail(ModelFailure::InvalidResponse);
                } else {
                    self.outcome = Some(AdapterOutcome::Succeeded {
                        outputs,
                        evidence: vec![],
                    });
                }
            }
            Action::Call {
                capability,
                inputs,
                summary,
            } => {
                if !bounded(&summary, 8192) {
                    self.fail(ModelFailure::InvalidResponse);
                    return Ok(());
                }
                if self.tools >= self.policy.spec().budget.tool_calls {
                    self.fail(ModelFailure::Budget);
                    return Ok(());
                }
                let Some(d) = self
                    .policy
                    .spec()
                    .tools
                    .iter()
                    .find(|d| d.capability == capability)
                else {
                    self.fail(ModelFailure::InvalidResponse);
                    return Ok(());
                };
                let c = Capability::new(d.clone())?;
                let id = format!(
                    "model-tool-{}",
                    &digest(&(digest(&self.request)?, self.calls))?[7..]
                );
                let context = RequestContext {
                    request_id: id,
                    trace_id: self.request.trace_id.clone(),
                    issued_at_unix_ms: at,
                    deadline_unix_ms: self.deadline.min(at.saturating_add(d.timeout_ms)),
                };
                match WorkRequest::standalone(&c, inputs, context) {
                    Ok(request) => {
                        self.pending = Some(request);
                        self.tools += 1;
                    }
                    Err(_) => self.fail(ModelFailure::InvalidResponse),
                }
            }
        }
        Ok(())
    }
    pub fn tool_finished(
        &mut self,
        request: WorkRequest,
        at: u64,
        response: ToolReply,
    ) -> Result<()> {
        if self.outcome.is_some() || self.pending.as_ref() != Some(&request) {
            return Err(invalid("tool result is not the exact authorized call"));
        }
        self.time(at)?;
        if let ToolReply::Received { result } = &response {
            let d = self
                .policy
                .spec()
                .tools
                .iter()
                .find(|d| d.capability == request.capability)
                .ok_or_else(|| invalid("tool contract missing"))?;
            workflow_worker::accept_result(
                &request,
                &ExecutionGrant::bind(&request)?,
                &Capability::new(d.clone())?,
                result.clone(),
                at,
            )?;
        }
        self.events.push(ModelEvent::ToolFinished {
            request: Box::new(request),
            at_unix_ms: at,
            response,
        });
        self.pending = None;
        Ok(())
    }
    pub fn record(mut self) -> Result<ModelRecord> {
        if !matches!(self.next_action()?, Next::Complete) {
            return Err(invalid("model session is incomplete"));
        }
        let record = ModelRecord {
            schema_version: 1,
            request_digest: digest(&self.request)?,
            policy_digest: self.policy.binding().digest.clone(),
            identity: self.identity,
            deadline_unix_ms: self.deadline,
            events: self.events,
            outcome: self.outcome.unwrap(),
        };
        to_message(&record)?;
        Ok(record)
    }
}
pub fn verify_record(policy: &Policy, request: &WorkRequest, record: &ModelRecord) -> Result<()> {
    to_message(record)?;
    if record.schema_version != 1
        || record.events.len() > 32
        || record.request_digest != digest(request)?
        || record.policy_digest != policy.binding().digest
    {
        return Err(invalid("model record binding/version/budget mismatch"));
    }
    let mut session = Session::new(
        policy.clone(),
        request.clone(),
        record.identity.clone(),
        record.deadline_unix_ms,
    )?;
    for event in &record.events {
        match event {
            ModelEvent::Replied {
                call_digest,
                at_unix_ms,
                response,
            } => session.replied(call_digest.clone(), *at_unix_ms, response.clone())?,
            ModelEvent::ToolFinished {
                request,
                at_unix_ms,
                response,
            } => session.tool_finished((**request).clone(), *at_unix_ms, response.clone())?,
        }
    }
    if session.record()? != *record {
        return Err(invalid(
            "model outcome differs from explicit execution history",
        ));
    }
    Ok(())
}
/// Offline proof validation, not live lease authorization or provider authentication.
pub fn verify_result(
    policy: &Policy,
    request: &WorkRequest,
    result: &workflow_worker::WorkResult,
) -> Result<ModelRecord> {
    workflow_worker::accept_result(
        request,
        &ExecutionGrant::bind(request)?,
        &Capability::new(policy.spec().task.clone())?,
        result.clone(),
        result.completed_at_unix_ms,
    )?;
    let record: ModelRecord = parse_message(&to_message(
        result
            .model_record
            .as_ref()
            .ok_or_else(|| invalid("model record missing"))?,
    )?)?;
    verify_record(policy, request, &record)?;
    if record.outcome != result.outcome
        || record.events.iter().any(|e| {
            let at = match e {
                ModelEvent::Replied { at_unix_ms, .. }
                | ModelEvent::ToolFinished { at_unix_ms, .. } => *at_unix_ms,
            };
            at > result.completed_at_unix_ms
        })
    {
        return Err(invalid(
            "model record outcome/time differs from worker completion",
        ));
    }
    Ok(record)
}
pub fn execute(
    policy: &Policy,
    request: &WorkRequest,
    model: &dyn ModelAdapter,
    tools: &Worker,
    clock: &impl Clock,
    deadline: u64,
) -> Result<ModelRecord> {
    for d in &policy.spec().tools {
        if tools
            .capability(&d.capability.id, &d.capability.version)?
            .digest()
            != Capability::new(d.clone())?.digest()
        {
            return Err(invalid("registered tool differs from frozen model policy"));
        }
    }
    let mut session = Session::new(policy.clone(), request.clone(), model.identity(), deadline)?;
    loop {
        let now = clock.now_unix_ms()?;
        if now >= deadline || now < session.last_time {
            return Err(Error::new(
                ErrorCode::DeadlineExceeded,
                "model session clock/deadline rejected",
            ));
        }
        match session.next_action()? {
            Next::Complete => return session.record(),
            Next::Model(call) => {
                let response = model.complete(&call);
                session.replied(digest(&call)?, clock.now_unix_ms()?, response)?;
            }
            Next::Tool(request) => {
                let response = match tools.execute_with_clock(
                    &request,
                    &ExecutionGrant::bind(&request)?,
                    clock,
                ) {
                    Ok(r) => ToolReply::Received {
                        result: r.into_result(),
                    },
                    Err(e) => ToolReply::Rejected { error: e.code },
                };
                session.tool_finished(*request, clock.now_unix_ms()?, response)?;
            }
        }
    }
}
pub struct ModelCapability {
    policy: Policy,
    model: Arc<dyn ModelAdapter>,
    tools: Worker,
}
impl ModelCapability {
    pub fn new(policy: Policy, model: Arc<dyn ModelAdapter>, tools: Worker) -> Result<Self> {
        for d in &policy.spec().tools {
            if tools
                .capability(&d.capability.id, &d.capability.version)?
                .descriptor()
                != d
            {
                return Err(invalid("model tool is missing or differs from policy"));
            }
        }
        Ok(Self {
            policy,
            model,
            tools,
        })
    }
}
impl CapabilityAdapter for ModelCapability {
    fn descriptor(&self) -> workflow_worker::CapabilityDescriptor {
        self.policy.spec().task.clone()
    }
    fn model_policy(&self) -> Option<workflow_worker::ModelPolicyBinding> {
        Some(self.policy.binding().clone())
    }
    fn invoke(&self, invocation: Invocation<'_>) -> AdapterOutcome {
        self.invoke_recorded(invocation).outcome
    }
    fn invoke_recorded(&self, invocation: Invocation<'_>) -> AdapterExecution {
        match execute(
            &self.policy,
            invocation.request,
            self.model.as_ref(),
            &self.tools,
            &SystemClock,
            invocation.effective_deadline_unix_ms,
        ) {
            Ok(record) => AdapterExecution {
                outcome: record.outcome.clone(),
                model_record: serde_json::to_value(record).ok(),
            },
            // Missing records are rejected by the worker; they cannot become successful model results.
            Err(_) => AdapterExecution {
                outcome: failure_outcome(&ModelFailure::InvalidResponse),
                model_record: None,
            },
        }
    }
}
