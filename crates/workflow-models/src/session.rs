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
    retries: u32,
    retry_at: u64,
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
            retries: 0,
            retry_at: 0,
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
        if at < self.retry_at {
            return Err(invalid("model retry preceded durable eligibility"));
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
                let hint = match &failure {
                    ModelFailure::Uncertain | ModelFailure::Unavailable => Some(None),
                    ModelFailure::Temporary { retry_after_ms }
                    | ModelFailure::RateLimited { retry_after_ms } => Some(*retry_after_ms),
                    _ => None,
                };
                if let (Some(hint), Some(policy)) = (hint, &self.policy.spec().retry)
                    && self.retries < policy.max_retries
                    && self.calls < self.policy.spec().budget.model_calls
                {
                    let backoff = policy
                        .initial_backoff_ms
                        .saturating_mul(1u64 << self.retries)
                        .min(policy.max_backoff_ms)
                        .max(hint.unwrap_or(0));
                    let retry_at = at.saturating_add(backoff);
                    if backoff <= policy.max_backoff_ms && retry_at < self.deadline {
                        self.retries += 1;
                        self.retry_at = retry_at;
                        return Ok(());
                    }
                }
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
    pub fn retry_at(&self) -> u64 {
        self.retry_at
    }
    pub fn checkpoint(&self, admitted: Option<Admission>) -> ModelCheckpoint {
        ModelCheckpoint {
            schema_version: 1,
            request: self.request.clone(),
            policy_digest: self.policy.binding().digest.clone(),
            identity: self.identity.clone(),
            deadline_unix_ms: self.deadline,
            events: self.events.clone(),
            admitted,
        }
    }
    fn apply_event(&mut self, event: &ModelEvent) -> Result<()> {
        match event {
            ModelEvent::Replied {
                call_digest,
                at_unix_ms,
                response,
            } => self.replied(call_digest.clone(), *at_unix_ms, response.clone()),
            ModelEvent::ToolFinished {
                request,
                at_unix_ms,
                response,
            } => self.tool_finished((**request).clone(), *at_unix_ms, response.clone()),
        }
    }
    pub fn restore(policy: &Policy, checkpoint: &ModelCheckpoint) -> Result<Self> {
        to_message(checkpoint)?;
        if checkpoint.schema_version != 1
            || checkpoint.policy_digest != policy.binding().digest
            || checkpoint.events.len() > 32
        {
            return Err(invalid("model checkpoint binding/version/budget mismatch"));
        }
        let mut session = Self::new(
            policy.clone(),
            checkpoint.request.clone(),
            checkpoint.identity.clone(),
            checkpoint.deadline_unix_ms,
        )?;
        for event in &checkpoint.events {
            session.apply_event(event)?;
        }
        if let Some(admission) = &checkpoint.admitted {
            let at = match (admission, session.next_action()?) {
                (
                    Admission::Model {
                        call_digest,
                        at_unix_ms,
                    },
                    Next::Model(call),
                ) if *call_digest == digest(&call)? && *at_unix_ms >= session.retry_at => {
                    *at_unix_ms
                }
                (
                    Admission::Tool {
                        request_digest,
                        at_unix_ms,
                    },
                    Next::Tool(request),
                ) if *request_digest == digest(&request)? => *at_unix_ms,
                _ => {
                    return Err(invalid(
                        "checkpoint admission differs from the next authorized call",
                    ));
                }
            };
            session.time(at)?;
        }
        Ok(session)
    }
    pub fn record(mut self) -> Result<ModelRecord> {
        if !matches!(self.next_action()?, Next::Complete) {
            return Err(invalid("model session is incomplete"));
        }
        let record = ModelRecord {
            schema_version: 1,
            source_request: None,
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
    if let Some(source) = &record.source_request {
        if record.schema_version != 2 || record.request_digest != digest(request)? {
            return Err(invalid("resumed record version or request differs"));
        }
        verify_resume(source, request)?;
        let mut original = record.clone();
        original.schema_version = 1;
        original.source_request = None;
        original.request_digest = digest(source)?;
        return verify_record(policy, source, &original);
    }
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
        session.apply_event(event)?;
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
    execute_journaled(policy, request, model, tools, clock, deadline, None)
}
#[allow(clippy::too_many_arguments)]
pub fn execute_journaled(
    policy: &Policy,
    request: &WorkRequest,
    model: &dyn ModelAdapter,
    tools: &Worker,
    clock: &impl Clock,
    deadline: u64,
    journal: Option<&dyn SessionJournal>,
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
    let mut saved = journal.map(|j| j.load(request)).transpose()?.flatten();
    let mut session = if let Some(checkpoint) = &saved {
        verify_resume(&checkpoint.request, request)?;
        if checkpoint.identity != model.identity() || checkpoint.deadline_unix_ms > deadline {
            return Err(invalid("model checkpoint provider or deadline changed"));
        }
        Session::restore(policy, checkpoint)?
    } else {
        Session::new(policy.clone(), request.clone(), model.identity(), deadline)?
    };
    let persist = |session: &Session,
                   admission: Option<Admission>,
                   saved: &mut Option<ModelCheckpoint>|
     -> Result<()> {
        if let Some(journal) = journal {
            let next = session.checkpoint(admission);
            journal.save(request, saved.as_ref(), &next)?;
            *saved = Some(next);
        }
        Ok(())
    };
    // A process death leaves an admission without an acknowledged observation.
    // Consume that call's budget and record uncertainty before choosing more work.
    if let Some(admission) = saved.as_ref().and_then(|s| s.admitted.clone()) {
        let at = clock.now_unix_ms()?;
        match admission {
            Admission::Model { call_digest, .. } => session.replied(
                call_digest,
                at,
                Reply::Failed {
                    failure: ModelFailure::Uncertain,
                },
            )?,
            Admission::Tool { .. } => {
                let Next::Tool(tool) = session.next_action()? else {
                    return Err(invalid("lost tool admission"));
                };
                session.tool_finished(*tool, at, ToolReply::Uncertain)?;
            }
        }
        persist(&session, None, &mut saved)?;
    }
    let deadline = session.deadline;
    loop {
        let now = clock.now_unix_ms()?;
        if now >= deadline || now < session.last_time {
            return Err(Error::new(
                ErrorCode::DeadlineExceeded,
                "model session clock/deadline rejected",
            ));
        }
        match session.next_action()? {
            Next::Complete => {
                let source = session.request.clone();
                let mut record = session.record()?;
                if source != *request {
                    record.schema_version = 2;
                    record.source_request = Some(Box::new(source));
                    record.request_digest = digest(request)?;
                }
                return Ok(record);
            }
            Next::Model(call) => {
                if now < session.retry_at {
                    std::thread::sleep(std::time::Duration::from_millis(
                        (session.retry_at - now).min(100),
                    ));
                    continue;
                }
                persist(
                    &session,
                    Some(Admission::Model {
                        call_digest: digest(&call)?,
                        at_unix_ms: now,
                    }),
                    &mut saved,
                )?;
                let response = model.complete(&call);
                session.replied(digest(&call)?, clock.now_unix_ms()?, response)?;
                persist(&session, None, &mut saved)?;
            }
            Next::Tool(request) => {
                persist(
                    &session,
                    Some(Admission::Tool {
                        request_digest: digest(&request)?,
                        at_unix_ms: now,
                    }),
                    &mut saved,
                )?;
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
                persist(&session, None, &mut saved)?;
            }
        }
    }
}
pub struct ModelCapability {
    policy: Policy,
    model: Arc<dyn ModelAdapter>,
    tools: Worker,
    journal: Option<Arc<dyn SessionJournal>>,
}
impl ModelCapability {
    pub fn with_journal(mut self, journal: Arc<dyn SessionJournal>) -> Self {
        self.journal = Some(journal);
        self
    }
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
            journal: None,
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
        match execute_journaled(
            &self.policy,
            invocation.request,
            self.model.as_ref(),
            &self.tools,
            &SystemClock,
            invocation.effective_deadline_unix_ms,
            self.journal.as_deref(),
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

/// Immutable business scope and absolute original deadline survive host takeover.
pub fn verify_resume(source: &WorkRequest, current: &WorkRequest) -> Result<()> {
    if source == current {
        return Ok(());
    }
    let scope_matches = match (&source.scope, &current.scope) {
        (
            workflow_worker::InvocationScope::Workflow {
                definition_digest: a,
                run_id: b,
                node_id: c,
                node_instance_id: d,
                lease_epoch: e,
                ..
            },
            workflow_worker::InvocationScope::Workflow {
                definition_digest: x,
                run_id: y,
                node_id: z,
                node_instance_id: w,
                lease_epoch: f,
                ..
            },
        ) => (a, b, c, d) == (x, y, z, w) && e <= f,
        _ => false,
    };
    if !scope_matches
        || source.inputs != current.inputs
        || source.input_digest != current.input_digest
        || source.model_policy != current.model_policy
        || source.capability != current.capability
        || source.contract_digest != current.contract_digest
        || source.protocol_version != current.protocol_version
        || source.trace_id != current.trace_id
        || source.issued_at_unix_ms > current.issued_at_unix_ms
        || source.deadline_unix_ms > current.deadline_unix_ms
    {
        return Err(invalid(
            "checkpoint belongs to a different task, input, policy or time boundary",
        ));
    }
    Ok(())
}

/// Enforce append-only observations and one durable admission per external call.
pub fn verify_checkpoint_update(
    previous: Option<&ModelCheckpoint>,
    next: &ModelCheckpoint,
    at: u64,
) -> Result<()> {
    if next.deadline_unix_ms <= at
        || next.admitted.as_ref().is_some_and(|a| match a {
            Admission::Model { at_unix_ms, .. } | Admission::Tool { at_unix_ms, .. } => {
                *at_unix_ms > at
            }
        })
    {
        return Err(invalid("checkpoint admission is future-dated or expired"));
    }
    let Some(previous) = previous else {
        if !next.events.is_empty() {
            return Err(invalid(
                "initial checkpoint contains unacknowledged history",
            ));
        }
        return Ok(());
    };
    if previous.request != next.request
        || previous.policy_digest != next.policy_digest
        || previous.identity != next.identity
        || previous.deadline_unix_ms != next.deadline_unix_ms
        || previous.schema_version != next.schema_version
        || !next.events.starts_with(&previous.events)
    {
        return Err(invalid(
            "checkpoint immutable binding or acknowledged history changed",
        ));
    }
    match (
        &previous.admitted,
        &next.admitted,
        &next.events[previous.events.len()..],
    ) {
        (None, Some(_), []) => (),
        (
            Some(Admission::Model {
                call_digest,
                at_unix_ms,
            }),
            None,
            [
                ModelEvent::Replied {
                    call_digest: observed,
                    at_unix_ms: completed,
                    ..
                },
            ],
        ) if call_digest == observed && completed >= at_unix_ms && *completed <= at => (),
        (
            Some(Admission::Tool {
                request_digest,
                at_unix_ms,
            }),
            None,
            [
                ModelEvent::ToolFinished {
                    request,
                    at_unix_ms: completed,
                    ..
                },
            ],
        ) if *request_digest == digest(request)? && completed >= at_unix_ms && *completed <= at => {
        }
        _ => {
            return Err(invalid(
                "checkpoint must admit one call or acknowledge its exact observation",
            ));
        }
    }
    Ok(())
}
