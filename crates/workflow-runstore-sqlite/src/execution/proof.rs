use super::*;
use workflow_kernel::{EventKind, TaskResult};
use workflow_worker::{
    AdapterOutcome, Capability, EffectContract, NodeAttempt, RequestContext, WorkRequest,
};

pub(super) fn capability(r: &Recovered, entry: &OutboxEntry) -> Result<Capability> {
    let Command::ExecuteTask { capability, .. } = &entry.command else {
        return Err(corrupt("task command required"));
    };
    let descriptor = r
        .engine
        .bundle()
        .spec()
        .capabilities
        .iter()
        .find(|d| &d.capability == capability)
        .ok_or_else(|| corrupt("missing task contract"))?;
    let c = Capability::new(descriptor.clone())?;
    if c.descriptor().effects != EffectContract::ReadOnly {
        return Err(Error::new(
            ErrorCode::UnsupportedEffect,
            "local execution only supports read-only capabilities; writes require the effect executor",
        ));
    }
    Ok(c)
}
pub(super) fn prepare(
    r: &Recovered,
    e: &OutboxEntry,
    l: &Lease,
    number: u32,
    now: u64,
) -> Result<PreparedTask> {
    let c = capability(r, e)?;
    let Command::ExecuteTask {
        instance_id,
        frame_id,
        node_id,
        inputs,
        ..
    } = &e.command
    else {
        unreachable!()
    };
    let reference = &r.engine.snapshot().frames[frame_id].workflow;
    let w = r
        .engine
        .bundle()
        .spec()
        .workflows
        .iter()
        .find(|w| w.id == reference.id && w.version == reference.version)
        .ok_or_else(|| corrupt("missing frame definition"))?;
    let id = format!("attempt-{}-{}-{number}", e.sequence, l.epoch);
    let deadline = now
        .checked_add(c.descriptor().timeout_ms)
        .ok_or_else(|| Error::new(ErrorCode::InvalidRequest, "task deadline overflow"))?
        .min(l.expires_at_unix_ms);
    let policy = match &w
        .nodes
        .iter()
        .find(|n| n.id == *node_id)
        .ok_or_else(|| corrupt("task node missing"))?
        .kind
    {
        workflow_ir::NodeKind::Task {
            policy: Some(reference),
            ..
        } => {
            let p = r
                .engine
                .bundle()
                .spec()
                .model_policies
                .iter()
                .find(|p| &p.policy == reference)
                .ok_or_else(|| corrupt("model policy missing"))?;
            Some(workflow_models::Policy::new(p.clone())?.binding().clone())
        }
        _ => None,
    };
    let request = WorkRequest::for_node_with_policy(
        w,
        node_id,
        &c,
        inputs.clone(),
        RequestContext {
            request_id: id.clone(),
            trace_id: l.run_id.clone(),
            issued_at_unix_ms: now,
            deadline_unix_ms: deadline,
        },
        NodeAttempt {
            run_id: l.run_id.clone(),
            node_instance_id: format!("instance-{instance_id}"),
            attempt_id: id.clone(),
            lease_epoch: l.epoch,
        },
        policy,
    )?;
    let grant = workflow_worker::ExecutionGrant::bind(&request)?;
    Ok(PreparedTask {
        attempt_id: id,
        command_id: e.command_id.clone(),
        command_sequence: e.sequence,
        epoch: l.epoch,
        number,
        prepared_revision: r.engine.snapshot().revision,
        request,
        grant,
    })
}
pub(super) fn model_record(
    r: &Recovered,
    request: &WorkRequest,
    result: &workflow_worker::WorkResult,
) -> Result<()> {
    if let Some(binding) = &request.model_policy {
        let p = r
            .engine
            .bundle()
            .spec()
            .model_policies
            .iter()
            .find(|p| p.policy == binding.policy)
            .ok_or_else(|| corrupt("model policy missing"))?;
        workflow_models::verify_result(&workflow_models::Policy::new(p.clone())?, request, result)?;
    }
    Ok(())
}
pub(super) fn task_result(result: &workflow_worker::WorkResult) -> Result<TaskResult> {
    Ok(match &result.outcome {
        AdapterOutcome::Succeeded { outputs, .. } => TaskResult::Succeeded {
            outputs: outputs.clone(),
        },
        AdapterOutcome::Failed { code, class, .. } => {
            if *class == workflow_worker::FailureClass::Cancelled {
                TaskResult::Cancelled
            } else {
                TaskResult::Failed { code: code.clone() }
            }
        }
    })
}
pub(super) fn verify_artifacts(
    result: &workflow_worker::WorkResult,
    request: &WorkRequest,
    reader: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<()> {
    let evidence = match &result.outcome {
        AdapterOutcome::Succeeded { evidence, .. } | AdapterOutcome::Failed { evidence, .. } => {
            evidence
        }
    };
    if evidence.is_empty() {
        return Ok(());
    }
    let reader = reader.ok_or_else(|| {
        Error::new(
            ErrorCode::ArtifactUnavailable,
            "run evidence requires a configured artifact store",
        )
    })?;
    let producer = artifact_producer(request)?;
    for e in evidence {
        let artifact = reader.verify(&workflow_artifacts::ArtifactLink {
            artifact_id: e.artifact_id.clone(),
            digest: e.digest.clone(),
        })?;
        workflow_artifacts::validate_ref(&artifact)?;
        if artifact.artifact_id != e.artifact_id
            || artifact.digest != e.digest
            || artifact.manifest.spec.producer != producer
        {
            return Err(Error::new(
                ErrorCode::ArtifactRejected,
                "artifact evidence belongs to another producer/request/input",
            ));
        }
    }
    Ok(())
}
pub(super) fn entry<'a>(r: &'a Recovered, id: &str) -> Result<&'a OutboxEntry> {
    r.outbox
        .iter()
        .find(|e| e.command_id == id)
        .ok_or_else(|| corrupt("execution command is missing"))
}
pub(super) fn verify_receipt(r: &Recovered, id: &str, epoch: u64) -> Result<()> {
    let e = entry(r, id)?;
    let receipt = e
        .receipt
        .as_ref()
        .ok_or_else(|| corrupt("execution acknowledgement missing"))?;
    if receipt.delivery_id != format!("runtime-{epoch}-{}", e.sequence) {
        return Err(corrupt("execution receipt identity mismatch"));
    }
    Ok(())
}
pub(super) fn stored_event<'a>(
    r: &'a Recovered,
    id: &str,
    revision: u64,
    now: u64,
) -> Result<&'a Event> {
    let record = r
        .events
        .iter()
        .find(|e| e.revision == revision)
        .ok_or_else(|| corrupt("execution event missing"))?;
    if record.event.event_id != id || record.event.at_unix_ms != now {
        return Err(corrupt("execution event identity/time mismatch"));
    }
    Ok(&record.event)
}
pub(super) fn verify(
    r: &Recovered,
    a: &Authority,
    action: &ExecutionAction,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<()> {
    match action {
        ExecutionAction::Effect { record, transition } => {
            super::effects::verify(r, a, record, transition)?
        }
        ExecutionAction::Prepared { attempt } => {
            let paused = r
                .events
                .iter()
                .take_while(|e| e.revision <= attempt.prepared_revision)
                .filter_map(|e| match e.event.kind {
                    EventKind::Pause { .. } => Some(true),
                    EventKind::Resume { .. } | EventKind::Cancel => Some(false),
                    _ => None,
                })
                .last()
                .unwrap_or(false);
            if paused {
                return Err(corrupt("task preparation was admitted while paused"));
            }
            let e = entry(r, &attempt.command_id)?;
            let l = a
                .lease
                .as_ref()
                .ok_or_else(|| corrupt("prepared task has no lease"))?;
            let mut expected = prepare(r, e, l, attempt.number, attempt.request.issued_at_unix_ms)?;
            if attempt.prepared_revision < e.revision
                || attempt.prepared_revision > r.engine.snapshot().revision
            {
                return Err(corrupt("prepared state revision mismatch"));
            }
            expected.prepared_revision = attempt.prepared_revision;
            if &expected != attempt.as_ref() {
                return Err(corrupt("prepared request differs from immutable command"));
            }
        }
        ExecutionAction::Finished {
            attempt_id,
            result,
            event_id,
            event_revision,
            at_unix_ms,
        } => {
            let p = &a.attempts[attempt_id].prepared;
            verify_artifacts(result, &p.request, artifacts)?;
            let e = entry(r, &p.command_id)?;
            let c = capability(r, e)?;
            workflow_worker::accept_result(&p.request, &p.grant, &c, result.clone(), *at_unix_ms)?;
            model_record(r, &p.request, result)?;
            let Command::ExecuteTask { instance_id, .. } = e.command else {
                unreachable!()
            };
            let event = stored_event(r, event_id, *event_revision, *at_unix_ms)?;
            if event.kind
                != (EventKind::TaskCompleted {
                    instance_id,
                    result: task_result(result)?,
                })
            {
                return Err(corrupt(
                    "stored worker result differs from committed transition",
                ));
            }
            verify_receipt(r, &p.command_id, p.epoch)?;
        }
        ExecutionAction::GateChecked {
            epoch,
            command_id,
            event_id,
            event_revision,
            at_unix_ms,
        } => {
            let command = entry(r, command_id)?;
            let Command::CheckGate {
                instance_id,
                context,
            } = &command.command
            else {
                return Err(corrupt("gate proof has no gate command"));
            };
            verify_receipt(r, command_id, *epoch)?;
            let expected = super::gates::evaluate(
                r,
                a,
                context,
                artifacts,
                *at_unix_ms,
                event_revision.saturating_sub(1),
            )?;
            if stored_event(r, event_id, *event_revision, *at_unix_ms)?.kind
                != (EventKind::GateEvaluated {
                    instance_id: *instance_id,
                    context_digest: digest(context)?,
                    evaluation: Box::new(expected),
                })
            {
                return Err(corrupt(
                    "gate observation differs from verified execution evidence",
                ));
            }
        }
        ExecutionAction::Handled {
            epoch,
            command_id,
            event_id,
            event_revision,
            at_unix_ms,
        } => {
            verify_receipt(r, command_id, *epoch)?;
            let handled = entry(r, command_id)?;
            if super::effects::managed(r, handled)? {
                super::effects::verify_handled(r, a, handled)?;
            } else {
                match &handled.command {
                    Command::ExecuteTask { .. } => {
                        capability(r, entry(r, command_id)?)?;
                    }
                    Command::CancelTask { instance_id } => {
                        super::controls::read_only_instance(r, *instance_id)?
                    }
                    Command::ReconcileTask { .. } => {
                        return Err(corrupt("local executor cannot acknowledge reconciliation"));
                    }
                    _ => {}
                }
            }
            if let (Some(id), Some(revision)) = (event_id, event_revision) {
                let instance = match entry(r, command_id)?.command {
                    Command::ExecuteTask { instance_id, .. }
                    | Command::CancelTask { instance_id } => instance_id,
                    _ => return Err(corrupt("invalid cancellation command")),
                };
                if stored_event(r, id, *revision, *at_unix_ms)?.kind
                    != (EventKind::TaskCompleted {
                        instance_id: instance,
                        result: TaskResult::Cancelled,
                    })
                {
                    return Err(corrupt("cancel acknowledgement event mismatch"));
                }
            }
        }
        ExecutionAction::TimerAdvanced {
            event_id,
            event_revision,
            at_unix_ms,
            ..
        } if stored_event(r, event_id, *event_revision, *at_unix_ms)?.kind
            != EventKind::AdvanceTime =>
        {
            return Err(corrupt("timer event mismatch"));
        }
        _ => {}
    }
    Ok(())
}
