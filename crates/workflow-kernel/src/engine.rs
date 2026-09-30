mod inbox;
mod lifecycle;
mod migration;
mod postconditions;
use crate::bundle::{key, workflow_key};
use crate::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use workflow_ir::*;

#[derive(Clone, Debug)]
pub struct Engine {
    bundle: Arc<CompiledBundle>,
    initial_bundle: Arc<CompiledBundle>,
    state: Snapshot,
    limits: Limits,
    initial_inputs: Values,
    started_at: u64,
    events: Vec<Event>,
    seen: BTreeMap<String, String>,
}
fn checkpoint_checksum(c: &Checkpoint) -> Result<String> {
    Ok(workflow_worker::digest(&(
        c.schema_version,
        &c.bundle_digest,
        &c.run_id,
        &c.inputs,
        c.started_at_unix_ms,
        &c.limits,
        &c.events,
        &c.state_digest,
    ))?)
}
impl Engine {
    pub fn start(
        bundle: CompiledBundle,
        run_id: &str,
        inputs: Values,
        at: u64,
        limits: Limits,
    ) -> Result<(Self, Transition)> {
        if !workflow_validator::identifier(run_id) || at == 0 {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "run ID and positive start time required",
            ));
        }
        if !(1..=4096).contains(&limits.max_frames)
            || !(1..=100000).contains(&limits.max_instances)
            || !(1..=1000000).contains(&limits.max_transitions)
            || !(1..=10000).contains(&limits.max_events)
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "limits exceed supported positive bounds",
            ));
        }
        workflow_validator::validate_values(&bundle.root().inputs, &inputs)
            .map_err(|e| Error::new(ErrorCode::InvalidRequest, e.message))?;
        workflow_worker::to_message(&inputs)?;
        let root = bundle.spec().root.clone();
        let state = Snapshot {
            schema_version: 1,
            run_id: run_id.into(),
            run_digest: workflow_worker::digest(&(bundle.digest(), run_id, &inputs, at, &limits))?,
            bundle_digest: bundle.digest().into(),
            revision: 1,
            now_unix_ms: at,
            status: RunStatus::Running,
            pause: None,
            inbox: BTreeMap::new(),
            frames: BTreeMap::new(),
            transition_count: 0,
            next_frame_id: 1,
            next_instance_id: 1,
            next_token_sequence: 1,
        };
        let mut engine = Self {
            initial_bundle: Arc::new(bundle.clone()),
            bundle: Arc::new(bundle),
            state,
            limits,
            initial_inputs: inputs.clone(),
            started_at: at,
            events: vec![],
            seen: BTreeMap::new(),
        };
        engine.add_frame(&root, inputs)?;
        let mut commands = vec![];
        engine.drive(&mut commands)?;
        engine.bound()?;
        Ok((
            engine,
            Transition {
                revision: 1,
                duplicate: false,
                commands,
            },
        ))
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.state
    }
    pub fn bundle(&self) -> &CompiledBundle {
        &self.bundle
    }
    pub fn initial_bundle(&self) -> &CompiledBundle {
        &self.initial_bundle
    }
    pub fn bundle_spec_at(&self, revision: u64) -> Result<&BundleSpec> {
        if revision == 0 || revision > self.state.revision {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "revision is outside retained bundle history",
            ));
        }
        for event in self.events.iter().rev() {
            if event.expected_revision < revision
                && let EventKind::MigrateDefinition { plan } = &event.kind
            {
                return Ok(&plan.request.target_bundle);
            }
        }
        Ok(self.initial_bundle.spec())
    }
    /// Replay only recorded transitions under their frozen definitions. This
    /// performs no model, task, clock or provider invocation.
    pub fn at_revision(&self, revision: u64) -> Result<Self> {
        if revision == 0 || revision > self.state.revision {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "revision is outside the retained history",
            ));
        }
        if revision == self.state.revision {
            return Ok(self.clone());
        }
        let (mut engine, _) = Self::start(
            self.initial_bundle.as_ref().clone(),
            &self.state.run_id,
            self.initial_inputs.clone(),
            self.started_at,
            self.limits.clone(),
        )?;
        for event in self.events.iter().take((revision - 1) as usize) {
            engine.apply(event.clone())?;
        }
        Ok(engine)
    }
    pub fn apply(&mut self, event: Event) -> Result<Transition> {
        workflow_worker::to_message(&event)?;
        if event.run_id != self.state.run_id || event.run_digest != self.state.run_digest {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "event belongs to a different run identity",
            ));
        }
        if !workflow_validator::identifier(&event.event_id) {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "event ID must be a stable identifier",
            ));
        }
        let hash = workflow_worker::digest(&event)?;
        if let Some(previous) = self.seen.get(&event.event_id) {
            if previous != &hash {
                return Err(Error::new(
                    ErrorCode::EventConflict,
                    "event ID already names different content",
                ));
            }
            return Ok(Transition {
                revision: self.state.revision,
                duplicate: true,
                commands: vec![],
            });
        }
        if event.expected_revision != self.state.revision {
            return Err(Error::new(
                ErrorCode::RevisionConflict,
                format!(
                    "expected {}, actual {}",
                    event.expected_revision, self.state.revision
                ),
            ));
        }
        if !matches!(
            self.state.status,
            RunStatus::Running | RunStatus::Cancelling
        ) && !matches!(event.kind, EventKind::ReceiveSignal { .. })
        {
            return Err(Error::new(ErrorCode::TerminalRun, "run is terminal"));
        }
        if event.at_unix_ms < self.state.now_unix_ms {
            return Err(Error::new(
                ErrorCode::ClockReversal,
                "event time moved backwards",
            ));
        }
        if self.events.len() >= self.limits.max_events as usize
            && self.state.status != RunStatus::Cancelling
            && !matches!(event.kind, EventKind::Cancel)
        {
            return Err(Error::new(
                ErrorCode::BudgetExceeded,
                "event budget exhausted; cancellation remains available",
            ));
        }
        if let EventKind::MigrateDefinition { plan } = &event.kind {
            let expected = self.plan_migration(&plan.request)?;
            if &expected != plan.as_ref() {
                return Err(Error::new(
                    ErrorCode::InvalidRequest,
                    "migration plan differs from the current source state or recomputed impact",
                ));
            }
        }
        let mut next = self.clone();
        let mut commands = vec![];
        next.state.now_unix_ms = event.at_unix_ms;
        // Cancellation is available even if an ordinary transition budget is exhausted.
        if let EventKind::MigrateDefinition { plan } = &event.kind {
            next.migrate_definition(plan, &mut commands)?;
        } else if matches!(event.kind, EventKind::Cancel) {
            next.state.pause = None;
            next.state.status = RunStatus::Cancelling;
            next.cancel_frame(1, &mut commands)?;
        } else if next.lifecycle(&event.kind)? {
            if next.state.pause.is_none() {
                next.expire(&mut commands)?;
            }
        } else {
            if next.state.pause.is_some() {
                if !matches!(
                    event.kind,
                    EventKind::TaskCompleted { .. }
                        | EventKind::TaskReconciled { .. }
                        | EventKind::ReceiveSignal { .. }
                ) {
                    return Err(Error::new(
                        ErrorCode::RunPaused,
                        "run is paused; resume before advancing timers, signals or gates",
                    ));
                }
            } else {
                next.expire(&mut commands)?;
            }
            next.handle(&event.kind, &mut commands)?;
        }
        if next.state.pause.is_none() {
            next.drive(&mut commands)?;
        } else {
            next.settle_inbox(&mut commands)?;
        }
        next.state.revision = next
            .state
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::new(ErrorCode::BudgetExceeded, "revision exhausted"))?;
        next.events.push(event.clone());
        next.seen.insert(event.event_id, hash);
        next.bound()?;
        let transition = Transition {
            revision: next.state.revision,
            duplicate: false,
            commands,
        };
        *self = next;
        Ok(transition)
    }
    pub fn checkpoint(&self) -> Result<Checkpoint> {
        let mut checkpoint = Checkpoint {
            schema_version: 1,
            bundle_digest: self.initial_bundle.digest().into(),
            run_id: self.state.run_id.clone(),
            inputs: self.initial_inputs.clone(),
            started_at_unix_ms: self.started_at,
            limits: self.limits.clone(),
            events: self.events.clone(),
            state_digest: workflow_worker::digest(&self.state)?,
            checksum: String::new(),
        };
        checkpoint.checksum = checkpoint_checksum(&checkpoint)?;
        Ok(checkpoint)
    }
    pub fn restore(bundle: CompiledBundle, checkpoint: Checkpoint) -> Result<Self> {
        let invalid = |m| Error::new(ErrorCode::CorruptCheckpoint, m);
        workflow_worker::to_message(&checkpoint).map_err(|e| invalid(e.message))?;
        if checkpoint.checksum != checkpoint_checksum(&checkpoint)? {
            return Err(invalid("checkpoint checksum mismatch".into()));
        }
        if checkpoint.schema_version != 1 || checkpoint.bundle_digest != bundle.digest() {
            return Err(invalid("checkpoint bundle/schema mismatch".into()));
        }
        let (mut engine, _) = Self::start(
            bundle,
            &checkpoint.run_id,
            checkpoint.inputs,
            checkpoint.started_at_unix_ms,
            checkpoint.limits,
        )
        .map_err(|e| invalid(e.message))?;
        for event in checkpoint.events {
            if engine
                .apply(event)
                .map_err(|e| invalid(e.message))?
                .duplicate
            {
                return Err(invalid("duplicate event in checkpoint journal".into()));
            }
        }
        if workflow_worker::digest(engine.snapshot())? != checkpoint.state_digest {
            return Err(invalid(
                "checkpoint state digest disagrees with replay".into(),
            ));
        }
        Ok(engine)
    }
    fn bound(&self) -> Result<()> {
        workflow_worker::to_message(&self.state)
            .map_err(|e| Error::new(ErrorCode::BudgetExceeded, e.message))?;
        workflow_worker::to_message(&self.checkpoint()?)
            .map_err(|e| Error::new(ErrorCode::BudgetExceeded, e.message))?;
        Ok(())
    }
    fn touch(&mut self) -> Result<()> {
        self.state.transition_count =
            self.state.transition_count.checked_add(1).ok_or_else(|| {
                Error::new(ErrorCode::BudgetExceeded, "transition counter exhausted")
            })?;
        if self.state.transition_count > self.limits.max_transitions
            && self.state.status != RunStatus::Cancelling
        {
            return Err(Error::new(
                ErrorCode::BudgetExceeded,
                "transition budget exhausted; cancellation remains available",
            ));
        }
        Ok(())
    }
    fn add_frame(&mut self, reference: &VersionRef, inputs: Values) -> Result<u64> {
        let w = self.bundle.workflows[&key(reference)].clone();
        if self.state.frames.len() >= self.limits.max_frames as usize
            || self.state.next_instance_id - 1 + w.nodes.len() as u64
                > u64::from(self.limits.max_instances)
        {
            return Err(Error::new(
                ErrorCode::BudgetExceeded,
                "frame or node-instance budget exhausted",
            ));
        }
        let id = self.state.next_frame_id;
        self.state.next_frame_id += 1;
        let mut nodes = BTreeMap::new();
        for n in &w.nodes {
            let instance_id = self.state.next_instance_id;
            self.state.next_instance_id += 1;
            nodes.insert(
                n.id.clone(),
                NodeInstance {
                    instance_id,
                    state: NodeState::Pending,
                    inputs: Values::new(),
                    outputs: Values::new(),
                    cancel_requested: false,
                    winner_edge: None,
                    reason: None,
                    gate_decision: None,
                },
            );
        }
        let edges = w
            .edges
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    EdgeToken {
                        status: TokenStatus::Pending,
                        sequence: 0,
                    },
                )
            })
            .collect();
        self.state.frames.insert(
            id,
            Frame {
                workflow: reference.clone(),
                definition_digest: w
                    .digest()
                    .map_err(|e| Error::new(ErrorCode::InvalidBundle, e.to_string()))?,
                inputs,
                status: FrameStatus::Active,
                nodes,
                edges,
                outputs: Values::new(),
                reason: None,
            },
        );
        Ok(id)
    }
    fn workflow(&self, frame: u64) -> &Workflow {
        &self.bundle.workflows[&key(&self.state.frames[&frame].workflow)]
    }
    fn node(&self, frame: u64, id: &str) -> &Node {
        self.workflow(frame)
            .nodes
            .iter()
            .find(|n| n.id == id)
            .expect("compiled node")
    }
    fn record(&self, frame: u64, id: &str) -> &NodeInstance {
        &self.state.frames[&frame].nodes[id]
    }
    fn record_mut(&mut self, frame: u64, id: &str) -> &mut NodeInstance {
        self.state
            .frames
            .get_mut(&frame)
            .unwrap()
            .nodes
            .get_mut(id)
            .unwrap()
    }
    fn locate(&self, instance: u64) -> Result<(u64, String)> {
        self.state
            .frames
            .iter()
            .find_map(|(frame, f)| {
                f.nodes
                    .iter()
                    .find(|(_, n)| n.instance_id == instance)
                    .map(|(id, _)| (*frame, id.clone()))
            })
            .ok_or_else(|| Error::new(ErrorCode::UnknownInstance, "unknown node instance"))
    }
    fn finish(
        &mut self,
        frame: u64,
        id: &str,
        state: NodeState,
        selected: Option<BTreeSet<String>>,
        reason: Option<String>,
    ) -> Result<()> {
        if self.record(frame, id).state.terminal() {
            return Ok(());
        }
        self.touch()?;
        let edges: Vec<_> = self
            .workflow(frame)
            .edges
            .iter()
            .filter(|e| e.from == id)
            .map(|e| e.id.clone())
            .collect();
        let token = match state {
            NodeState::Succeeded => TokenStatus::Selected,
            NodeState::Failed => TokenStatus::Failed,
            NodeState::Cancelled => TokenStatus::Cancelled,
            _ => TokenStatus::Skipped,
        };
        let record = self.record_mut(frame, id);
        record.state = state;
        record.reason = reason;
        for edge in edges {
            let status = if token == TokenStatus::Selected
                && selected.as_ref().is_some_and(|s| !s.contains(&edge))
            {
                TokenStatus::Skipped
            } else {
                token.clone()
            };
            let sequence = self.state.next_token_sequence;
            self.state.next_token_sequence += 1;
            let t = self
                .state
                .frames
                .get_mut(&frame)
                .unwrap()
                .edges
                .get_mut(&edge)
                .unwrap();
            if t.status != TokenStatus::Pending {
                return Err(Error::new(
                    ErrorCode::InvalidRequest,
                    "edge was already settled",
                ));
            }
            *t = EdgeToken { status, sequence };
        }
        Ok(())
    }
    fn route(&self, frame: u64, id: &str, route: &Route) -> BTreeSet<String> {
        self.workflow(frame)
            .edges
            .iter()
            .filter(|e| e.from == id && e.route == *route)
            .map(|e| e.id.clone())
            .collect()
    }
    fn inputs(&self, frame: u64, node: &Node) -> Result<Values> {
        let f = &self.state.frames[&frame];
        let mut values = Values::new();
        for (name, binding) in &node.bindings {
            let value = match binding {
                Binding::Literal { value } => Some(value),
                Binding::WorkflowInput { field } => f.inputs.get(field),
                Binding::NodeOutput { node, field } => {
                    let source = &f.nodes[node];
                    if source.state == NodeState::Succeeded {
                        source.outputs.get(field)
                    } else {
                        None
                    }
                }
            };
            if let Some(v) = value {
                values.insert(name.clone(), v.clone());
            }
        }
        workflow_validator::validate_values(&node.inputs, &values)
            .map_err(|e| Error::new(ErrorCode::InvalidRequest, e.message))?;
        Ok(values)
    }
    fn handle(&mut self, event: &EventKind, commands: &mut Vec<Command>) -> Result<()> {
        match event {
            EventKind::ReceiveSignal { message } => self.receive_signal(message),
            EventKind::TaskCompleted {
                instance_id,
                result,
            } => self.task(*instance_id, result, false, commands),
            EventKind::TaskReconciled {
                instance_id,
                result,
            } => self.task(*instance_id, result, true, commands),
            EventKind::Signal {
                instance_id,
                event,
                accepted,
                outputs,
            } => {
                let (frame, id) = self.locate(*instance_id)?;
                if self
                    .bundle
                    .wait_policy(&self.state.frames[&frame].workflow, &id)
                    .is_some()
                {
                    return Err(Error::new(
                        ErrorCode::InvalidSignal,
                        "protected wait requires an attributed durable Inbox decision",
                    ));
                }
                self.signal(*instance_id, event, *accepted, outputs, commands)
            }
            EventKind::GateEvaluated {
                instance_id,
                context_digest,
                evaluation,
            } => self.gate_evaluated(*instance_id, context_digest, evaluation),
            EventKind::RetryGate {
                instance_id,
                context_digest,
            } => self.retry_gate(*instance_id, context_digest, commands),
            EventKind::AdvanceTime => Ok(()),
            EventKind::Cancel
            | EventKind::Pause { .. }
            | EventKind::Resume { .. }
            | EventKind::MigrateDefinition { .. } => {
                unreachable!("handled before timers")
            }
        }
    }
    fn signal(
        &mut self,
        instance_id: u64,
        event: &str,
        accepted: bool,
        outputs: &Values,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        let (frame, id) = self.locate(instance_id)?;
        let node = self.node(frame, &id).clone();
        let NodeKind::Wait {
            event: expected, ..
        } = &node.kind
        else {
            return Err(Error::new(
                ErrorCode::InvalidSignal,
                "instance is not a wait",
            ));
        };
        if expected != event || !matches!(self.record(frame, &id).state, NodeState::Waiting { .. })
        {
            return Err(Error::new(
                ErrorCode::InvalidSignal,
                "wait is not active for this event; deliver time advancement separately if it expired",
            ));
        }
        if accepted {
            workflow_validator::validate_values(&node.outputs, outputs)
                .map_err(|e| Error::new(ErrorCode::InvalidSignal, e.message))?;
        } else if !outputs.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidSignal,
                "rejection must not publish accepted outputs",
            ));
        }
        self.record_mut(frame, &id).outputs = outputs.clone();
        commands.push(Command::CancelTimer { instance_id });
        let route = if accepted {
            Route::Accepted
        } else {
            Route::Rejected
        };
        self.finish(
            frame,
            &id,
            NodeState::Succeeded,
            Some(self.route(frame, &id, &route)),
            Some(if accepted { "accepted" } else { "rejected" }.into()),
        )
    }
    fn task(
        &mut self,
        instance: u64,
        result: &TaskResult,
        reconciled: bool,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        let (frame, id) = self.locate(instance)?;
        let record = self.record(frame, &id).clone();
        let valid = if reconciled {
            record.state == NodeState::Reconciling
        } else {
            matches!(
                record.state,
                NodeState::TaskReady | NodeState::CancelRequested
            )
        };
        if !valid {
            return Err(Error::new(
                ErrorCode::InvalidTaskResult,
                "task is not awaiting this kind of result",
            ));
        }
        let node = self.node(frame, &id).clone();
        let NodeKind::Task { capability, .. } = &node.kind else {
            return Err(Error::new(
                ErrorCode::InvalidTaskResult,
                "instance is not a task",
            ));
        };
        let state = match result {
            TaskResult::Succeeded { outputs } => {
                workflow_validator::validate_values(&node.outputs, outputs)
                    .map_err(|e| Error::new(ErrorCode::InvalidTaskResult, e.message))?;
                self.record_mut(frame, &id).outputs = outputs.clone();
                NodeState::Succeeded
            }
            TaskResult::Failed { code } => {
                let Some(class) = self.bundle.capabilities[&key(capability)]
                    .descriptor()
                    .error_codes
                    .get(code)
                else {
                    return Err(Error::new(
                        ErrorCode::InvalidTaskResult,
                        "undeclared capability error code",
                    ));
                };
                if *class == workflow_worker::FailureClass::UnknownEffect {
                    if reconciled {
                        return Err(Error::new(
                            ErrorCode::InvalidTaskResult,
                            "unknown-effect failure cannot settle reconciliation",
                        ));
                    }
                    self.touch()?;
                    let r = self.record_mut(frame, &id);
                    r.state = NodeState::Reconciling;
                    r.reason = Some(code.clone());
                    commands.push(Command::ReconcileTask {
                        instance_id: instance,
                    });
                    return Ok(());
                }
                NodeState::Failed
            }
            TaskResult::Cancelled => NodeState::Cancelled,
            TaskResult::Uncertain { reason } => {
                if reconciled || reason.is_empty() || reason.len() > 1024 {
                    return Err(Error::new(
                        ErrorCode::InvalidTaskResult,
                        "reconciliation needs a known outcome; uncertainty needs a bounded reason",
                    ));
                }
                self.touch()?;
                let r = self.record_mut(frame, &id);
                r.state = NodeState::Reconciling;
                r.reason = Some(reason.clone());
                commands.push(Command::ReconcileTask {
                    instance_id: instance,
                });
                return Ok(());
            }
        };
        let reason = match result {
            TaskResult::Failed { code } => Some(code.clone()),
            _ => None,
        };
        if !record.cancel_requested
            && state == NodeState::Succeeded
            && self.begin_gate(frame, &id, commands)?
        {
            return Ok(());
        }
        self.finish(
            frame,
            &id,
            if record.cancel_requested {
                NodeState::Cancelled
            } else {
                state
            },
            None,
            reason,
        )
    }
    fn expire(&mut self, commands: &mut Vec<Command>) -> Result<()> {
        let due: Vec<_> = self
            .state
            .frames
            .iter()
            .flat_map(|(fid, f)| {
                f.nodes.iter().filter_map(move |(id, n)| match n.state {
                    NodeState::Waiting { deadline_unix_ms } => {
                        Some((*fid, id.clone(), deadline_unix_ms, false))
                    }
                    NodeState::Child {
                        deadline_unix_ms: Some(deadline),
                        exhausting: false,
                        ..
                    } => Some((*fid, id.clone(), deadline, true)),
                    _ => None,
                })
            })
            .filter(|(_, _, deadline, _)| *deadline <= self.state.now_unix_ms)
            .collect();
        for (frame, id, _, child) in due {
            if child {
                if let NodeState::Child {
                    frame_id,
                    iteration,
                    deadline_unix_ms,
                    ..
                } = self.record(frame, &id).state.clone()
                {
                    self.record_mut(frame, &id).state = NodeState::Child {
                        frame_id,
                        iteration,
                        deadline_unix_ms,
                        exhausting: true,
                    };
                    self.cancel_frame(frame_id, commands)?;
                }
            } else {
                commands.push(Command::CancelTimer {
                    instance_id: self.record(frame, &id).instance_id,
                });
                self.finish(
                    frame,
                    &id,
                    NodeState::Succeeded,
                    Some(self.route(frame, &id, &Route::TimedOut)),
                    Some("timed_out".into()),
                )?;
            }
        }
        Ok(())
    }
    fn cancel_frame(&mut self, frame: u64, commands: &mut Vec<Command>) -> Result<()> {
        if self.state.frames[&frame].status.terminal() {
            return Ok(());
        }
        self.state.frames.get_mut(&frame).unwrap().status = FrameStatus::Cancelling;
        let ids: Vec<_> = self.state.frames[&frame].nodes.keys().cloned().collect();
        for id in ids {
            self.cancel_node(frame, &id, commands)?;
        }
        Ok(())
    }
    fn cancel_node(&mut self, frame: u64, id: &str, commands: &mut Vec<Command>) -> Result<()> {
        let record = self.record(frame, id).clone();
        if record.state.terminal() {
            return Ok(());
        }
        self.record_mut(frame, id).cancel_requested = true;
        match record.state {
            NodeState::TaskReady => {
                self.touch()?;
                self.record_mut(frame, id).state = NodeState::CancelRequested;
                commands.push(Command::CancelTask {
                    instance_id: record.instance_id,
                });
            }
            NodeState::Reconciling | NodeState::CancelRequested => {}
            NodeState::Child {
                frame_id,
                deadline_unix_ms,
                ..
            } => {
                if deadline_unix_ms.is_some() {
                    commands.push(Command::CancelTimer {
                        instance_id: record.instance_id,
                    });
                }
                self.cancel_frame(frame_id, commands)?;
            }
            NodeState::Waiting { .. } => {
                commands.push(Command::CancelTimer {
                    instance_id: record.instance_id,
                });
                self.finish(
                    frame,
                    id,
                    NodeState::Cancelled,
                    None,
                    Some("cancelled".into()),
                )?;
            }
            NodeState::CheckingGate { .. } | NodeState::Pending => self.finish(
                frame,
                id,
                NodeState::Cancelled,
                None,
                Some("cancelled_before_activation".into()),
            )?,
            _ => {}
        }
        Ok(())
    }
    fn readiness(&self, frame: u64, node: &Node) -> Option<(TokenStatus, Option<String>)> {
        if node.id == self.workflow(frame).entry {
            return Some((TokenStatus::Selected, None));
        }
        let incoming: Vec<_> = self
            .workflow(frame)
            .edges
            .iter()
            .filter(|e| e.to == node.id)
            .map(|e| (e, &self.state.frames[&frame].edges[&e.id]))
            .collect();
        if matches!(
            node.kind,
            NodeKind::Join {
                mode: JoinMode::Any,
                ..
            }
        ) && let Some((edge, _)) = incoming
            .iter()
            .filter(|(_, t)| t.status == TokenStatus::Selected)
            .min_by_key(|(_, t)| t.sequence)
        {
            return Some((TokenStatus::Selected, Some(edge.id.clone())));
        }
        if incoming
            .iter()
            .any(|(_, t)| t.status == TokenStatus::Pending)
        {
            return None;
        }
        let status = if incoming
            .iter()
            .any(|(_, t)| t.status == TokenStatus::Failed)
        {
            TokenStatus::Failed
        } else if incoming
            .iter()
            .any(|(_, t)| t.status == TokenStatus::Cancelled)
        {
            TokenStatus::Cancelled
        } else if incoming
            .iter()
            .any(|(_, t)| t.status == TokenStatus::Selected)
        {
            TokenStatus::Selected
        } else {
            TokenStatus::Skipped
        };
        Some((status, None))
    }
    fn drive(&mut self, commands: &mut Vec<Command>) -> Result<()> {
        loop {
            let mut progress = self.settle_inbox(commands)?;
            let frames: Vec<_> = self.state.frames.keys().copied().collect();
            for frame in frames {
                if self.state.frames[&frame].status.terminal() {
                    continue;
                }
                let nodes = self.workflow(frame).nodes.clone();
                for node in nodes {
                    match self.record(frame, &node.id).state.clone() {
                        child @ NodeState::Child { frame_id, .. }
                            if self.state.frames[&frame_id].status.terminal() =>
                        {
                            self.child_done(frame, &node, child, commands)?;
                            progress = true;
                        }
                        NodeState::Pending => {
                            if let Some((token, winner)) = self.readiness(frame, &node) {
                                progress = true;
                                if token == TokenStatus::Selected {
                                    self.activate(frame, &node, winner, commands)?;
                                } else {
                                    let state = match token {
                                        TokenStatus::Failed => NodeState::Failed,
                                        TokenStatus::Cancelled => NodeState::Cancelled,
                                        _ => NodeState::Skipped,
                                    };
                                    self.finish(
                                        frame,
                                        &node.id,
                                        state,
                                        None,
                                        Some("upstream_outcome".into()),
                                    )?;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if self.state.frames[&frame]
                    .nodes
                    .values()
                    .all(|n| n.state.terminal())
                {
                    self.frame_done(frame)?;
                    progress = true;
                }
            }
            let root = &self.state.frames[&1].status;
            self.state.status = match root {
                FrameStatus::Succeeded => RunStatus::Succeeded,
                FrameStatus::Failed => RunStatus::Failed,
                FrameStatus::Cancelled => RunStatus::Cancelled,
                FrameStatus::Cancelling => RunStatus::Cancelling,
                FrameStatus::Active => RunStatus::Running,
            };
            if !progress {
                break;
            }
        }
        Ok(())
    }
    fn activate(
        &mut self,
        frame: u64,
        node: &Node,
        winner: Option<String>,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        let inputs = match self.inputs(frame, node) {
            Ok(v) => v,
            Err(e) => {
                return self.finish(
                    frame,
                    &node.id,
                    NodeState::Failed,
                    None,
                    Some(format!("input: {}", e.message)),
                );
            }
        };
        self.record_mut(frame, &node.id).inputs = inputs.clone();
        for condition in &node.preconditions {
            match workflow_validator::evaluate(condition, &inputs) {
                Ok(true) => {}
                Ok(false) => {
                    return self.finish(
                        frame,
                        &node.id,
                        NodeState::Skipped,
                        None,
                        Some("precondition_false".into()),
                    );
                }
                Err(e) => {
                    return self.finish(
                        frame,
                        &node.id,
                        NodeState::Failed,
                        None,
                        Some(format!("condition: {}", e.message)),
                    );
                }
            }
        }
        let instance = self.record(frame, &node.id).instance_id;
        match &node.kind {
            NodeKind::Task { capability, .. } => {
                self.touch()?;
                self.record_mut(frame, &node.id).state = NodeState::TaskReady;
                commands.push(Command::ExecuteTask {
                    instance_id: instance,
                    frame_id: frame,
                    node_id: node.id.clone(),
                    definition_digest: self.state.frames[&frame].definition_digest.clone(),
                    capability: capability.clone(),
                    contract_digest: self.bundle.capabilities[&key(capability)].digest().into(),
                    inputs,
                });
                Ok(())
            }
            NodeKind::Wait { event, timeout_ms } => {
                let deadline = self.deadline(*timeout_ms)?;
                self.touch()?;
                self.record_mut(frame, &node.id).state = NodeState::Waiting {
                    deadline_unix_ms: deadline,
                };
                commands.push(Command::AwaitSignal {
                    instance_id: instance,
                    event: event.clone(),
                    deadline_unix_ms: deadline,
                });
                Ok(())
            }
            NodeKind::Subworkflow { workflow } => {
                let child = self.add_frame(workflow, inputs)?;
                self.touch()?;
                self.record_mut(frame, &node.id).state = NodeState::Child {
                    frame_id: child,
                    iteration: 1,
                    deadline_unix_ms: None,
                    exhausting: false,
                };
                Ok(())
            }
            NodeKind::Loop {
                body, deadline_ms, ..
            } => {
                let deadline = self.deadline(*deadline_ms)?;
                let child = self.add_frame(body, inputs)?;
                self.touch()?;
                self.record_mut(frame, &node.id).state = NodeState::Child {
                    frame_id: child,
                    iteration: 1,
                    deadline_unix_ms: Some(deadline),
                    exhausting: false,
                };
                commands.push(Command::ScheduleLoopDeadline {
                    instance_id: instance,
                    deadline_unix_ms: deadline,
                });
                Ok(())
            }
            NodeKind::Decision { .. } => {
                let outgoing: Vec<_> = self
                    .workflow(frame)
                    .edges
                    .iter()
                    .filter(|e| e.from == node.id)
                    .collect();
                match workflow_validator::select_branch(node, &outgoing, &inputs) {
                    Ok(edge) => {
                        let selected = BTreeSet::from([edge.id.clone()]);
                        self.finish(frame, &node.id, NodeState::Succeeded, Some(selected), None)
                    }
                    Err(e) => self.finish(
                        frame,
                        &node.id,
                        NodeState::Failed,
                        None,
                        Some(format!("decision: {}", e.message)),
                    ),
                }
            }
            NodeKind::Terminal { outcome } => {
                let state = match outcome {
                    TerminalOutcome::Succeeded => NodeState::Succeeded,
                    TerminalOutcome::Failed => NodeState::Failed,
                    TerminalOutcome::Cancelled => NodeState::Cancelled,
                };
                if state == NodeState::Succeeded {
                    self.record_mut(frame, &node.id).outputs = inputs;
                    if self.begin_gate(frame, &node.id, commands)? {
                        return Ok(());
                    }
                }
                self.finish(
                    frame,
                    &node.id,
                    state,
                    None,
                    Some("declared_terminal".into()),
                )
            }
            NodeKind::Join {
                remaining: RemainingPolicy::CancelAndReconcile,
                ..
            } => {
                let winner = winner.expect("any join winner");
                let source = self
                    .workflow(frame)
                    .edges
                    .iter()
                    .find(|e| e.id == winner)
                    .unwrap()
                    .from
                    .clone();
                let groups = self.bundle.cancellations
                    [&(workflow_key(self.workflow(frame)), node.id.clone())]
                    .clone();
                self.record_mut(frame, &node.id).winner_edge = Some(winner);
                self.finish(frame, &node.id, NodeState::Succeeded, None, None)?;
                for group in groups {
                    if !group.contains(&source) {
                        for id in group {
                            self.cancel_node(frame, &id, commands)?;
                        }
                    }
                }
                Ok(())
            }
            NodeKind::Join { .. } | NodeKind::Fork => {
                self.record_mut(frame, &node.id).winner_edge = winner;
                self.finish(frame, &node.id, NodeState::Succeeded, None, None)
            }
        }
    }
    fn deadline(&self, duration: u64) -> Result<u64> {
        self.state
            .now_unix_ms
            .checked_add(duration)
            .ok_or_else(|| Error::new(ErrorCode::InvalidRequest, "deadline timestamp overflow"))
    }
    fn child_done(
        &mut self,
        frame: u64,
        node: &Node,
        child_state: NodeState,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        let NodeState::Child {
            frame_id: child,
            iteration,
            deadline_unix_ms: deadline,
            exhausting,
        } = child_state
        else {
            unreachable!("child state selected by reducer")
        };
        let completed = self.state.frames[&child].clone();
        let record = self.record(frame, &node.id).clone();
        if record.cancel_requested {
            return self.finish(
                frame,
                &node.id,
                NodeState::Cancelled,
                None,
                Some("child_cancelled".into()),
            );
        }
        if let NodeKind::Loop {
            body,
            max_iterations,
            ..
        } = &node.kind
        {
            if exhausting
                || (completed.status == FrameStatus::Failed && iteration >= *max_iterations)
            {
                commands.push(Command::CancelTimer {
                    instance_id: record.instance_id,
                });
                return self.finish(
                    frame,
                    &node.id,
                    NodeState::Succeeded,
                    Some(self.route(frame, &node.id, &Route::Exhausted)),
                    Some("loop_exhausted".into()),
                );
            }
            if completed.status == FrameStatus::Failed {
                let next = self.add_frame(body, record.inputs)?;
                self.touch()?;
                self.record_mut(frame, &node.id).state = NodeState::Child {
                    frame_id: next,
                    iteration: iteration + 1,
                    deadline_unix_ms: deadline,
                    exhausting: false,
                };
                return Ok(());
            }
            commands.push(Command::CancelTimer {
                instance_id: record.instance_id,
            });
        }
        let state = match completed.status {
            FrameStatus::Succeeded => NodeState::Succeeded,
            FrameStatus::Cancelled => NodeState::Cancelled,
            _ => NodeState::Failed,
        };
        if state == NodeState::Succeeded {
            self.record_mut(frame, &node.id).outputs = completed.outputs;
        }
        let selected =
            if matches!(node.kind, NodeKind::Loop { .. }) && state == NodeState::Succeeded {
                Some(self.route(frame, &node.id, &Route::Completed))
            } else {
                None
            };
        self.finish(frame, &node.id, state, selected, None)
    }
    fn frame_done(&mut self, frame: u64) -> Result<()> {
        let f = &self.state.frames[&frame];
        let terminals: Vec<_> = self
            .workflow(frame)
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Terminal { .. }))
            .map(|n| &f.nodes[&n.id])
            .collect();
        let successes: Vec<_> = terminals
            .iter()
            .filter(|n| n.state == NodeState::Succeeded)
            .collect();
        let conflict = successes
            .windows(2)
            .any(|pair| pair[0].outputs != pair[1].outputs);
        let status = if f.status == FrameStatus::Cancelling {
            FrameStatus::Cancelled
        } else if conflict || terminals.iter().any(|n| n.state == NodeState::Failed) {
            FrameStatus::Failed
        } else if terminals.iter().any(|n| n.state == NodeState::Cancelled) {
            FrameStatus::Cancelled
        } else if !successes.is_empty() {
            FrameStatus::Succeeded
        } else {
            FrameStatus::Failed
        };
        let outputs = if status == FrameStatus::Succeeded {
            successes[0].outputs.clone()
        } else {
            Values::new()
        };
        let f = self.state.frames.get_mut(&frame).unwrap();
        f.status = status;
        f.outputs = outputs;
        if conflict {
            f.reason = Some("conflicting_terminal_outputs".into());
        }
        Ok(())
    }
}
