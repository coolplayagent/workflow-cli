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
        .ok_or_else(|| Error::new(ErrorCode::InvalidRequest, "task deadline overflow"))?;
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
        renewable: true,
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
    authority: &Authority,
    command_id: &str,
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
        let record = workflow_models::verify_result(
            &workflow_models::Policy::new(p.clone())?,
            request,
            result,
        )?;
        if let Some(checkpoint) = authority.model_checkpoints.get(command_id) {
            let source = record.source_request.as_deref().unwrap_or(request);
            if checkpoint.request != *source
                || checkpoint.events != record.events
                || checkpoint.identity != record.identity
                || checkpoint.admitted.is_some()
            {
                return Err(corrupt(
                    "model result differs from durably acknowledged session",
                ));
            }
        } else if record.source_request.is_some() {
            return Err(corrupt(
                "resumed result lacks a durable predecessor checkpoint",
            ));
        }
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
    let revision = match action {
        ExecutionAction::Prepared { attempt } => Some(attempt.prepared_revision),
        ExecutionAction::Finished { event_revision, .. }
        | ExecutionAction::GateChecked { event_revision, .. }
        | ExecutionAction::TimerAdvanced { event_revision, .. } => {
            Some(event_revision.saturating_sub(1))
        }
        ExecutionAction::Handled {
            command_id,
            event_revision,
            ..
        } => Some(event_revision.map_or(entry(r, command_id)?.revision, |v| v.saturating_sub(1))),
        ExecutionAction::Migrated { migration } => Some(migration.source_revision),
        _ => None,
    };
    if let Some(revision) = revision
        && r.events.iter().any(|e| {
            e.revision > revision && matches!(e.event.kind, EventKind::MigrateDefinition { .. })
        })
    {
        let mut historical = r.clone();
        historical.engine = r.engine.at_revision(revision)?;
        return verify_at(&historical, a, action, artifacts);
    }
    verify_at(r, a, action, artifacts)
}
fn verify_at(
    r: &Recovered,
    a: &Authority,
    action: &ExecutionAction,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<()> {
    match action {
        ExecutionAction::Migrated { migration } => {
            migration.validate(&a.run_id)?;
            let event = stored_event(
                r,
                &migration.event_id,
                migration.event_revision,
                migration.at_unix_ms,
            )?;
            let EventKind::MigrateDefinition { plan } = &event.kind else {
                return Err(corrupt("migration proof has no definition migration event"));
            };
            if digest(plan)? != migration.plan_digest
                || plan.source_revision != migration.source_revision
                || migration_event_id(&plan.request.migration_id)? != migration.event_id
                || event.expected_revision != migration.source_revision
                || r.engine.snapshot().revision != migration.source_revision
                || &r.engine.plan_migration(&plan.request)? != plan.as_ref()
            {
                return Err(corrupt(
                    "definition migration proof differs from the reviewed source and plan",
                ));
            }
            let previous: Vec<_> = r
                .outbox
                .iter()
                .filter(|e| e.revision <= migration.source_revision)
                .collect();
            if migration.previous_delivery_sequence > previous.len() as u64
                || previous.iter().any(|e| e.receipt.is_none())
            {
                return Err(corrupt(
                    "migration did not retire every prior pending command",
                ));
            }
            let retired: Vec<_> = previous
                .into_iter()
                .filter(|e| e.sequence > migration.previous_delivery_sequence)
                .collect();
            if retired.iter().map(|e| &e.command_id).collect::<Vec<_>>()
                != migration.retired_commands.iter().collect::<Vec<_>>()
            {
                return Err(corrupt(
                    "migration command retirement differs from source outbox",
                ));
            }
            for entry in retired {
                if entry.receipt.as_ref().unwrap().delivery_id
                    != migration_delivery_id(&migration.plan_digest, entry.sequence)?
                {
                    return Err(corrupt("retired command is missing its migration receipt"));
                }
            }
        }
        ExecutionAction::Effect { record, transition } => {
            super::effects::verify(r, a, record, transition, artifacts)?
        }
        ExecutionAction::Continued { plan, .. } => {
            super::continuation::verify(r, a, plan, artifacts)?;
        }
        ExecutionAction::ModelCheckpoint {
            attempt_id,
            checkpoint,
            ..
        } => {
            let attempt = a
                .attempts
                .get(attempt_id)
                .ok_or_else(|| corrupt("checkpoint attempt missing"))?;
            let binding = attempt
                .prepared
                .request
                .model_policy
                .as_ref()
                .ok_or_else(|| corrupt("checkpoint task has no model policy"))?;
            let policy = r
                .engine
                .bundle()
                .spec()
                .model_policies
                .iter()
                .find(|p| p.policy == binding.policy)
                .ok_or_else(|| corrupt("checkpoint policy missing"))?;
            workflow_models::Session::restore(
                &workflow_models::Policy::new(policy.clone())?,
                checkpoint,
            )?;
        }
        ExecutionAction::Prepared { attempt } => {
            let paused = r
                .events
                .iter()
                .take_while(|e| e.revision <= attempt.prepared_revision)
                .filter_map(|e| match e.event.kind {
                    EventKind::Pause { .. } | EventKind::MigrateDefinition { .. } => Some(true),
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
            if !attempt.renewable {
                expected.renewable = false;
                expected.request.deadline_unix_ms =
                    expected.request.deadline_unix_ms.min(l.expires_at_unix_ms);
                expected.grant = workflow_worker::ExecutionGrant::bind(&expected.request)?;
            }
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
            model_record(r, a, &p.command_id, &p.request, result)?;
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
        ExecutionAction::Restored {
            recovery,
            at_unix_ms,
        } => {
            let snapshot = super::effects::snapshot_at(r, recovery.source_revision)?;
            if digest(&snapshot)? != recovery.source_state_digest
                || *at_unix_ms < snapshot.now_unix_ms
            {
                return Err(corrupt(
                    "restore ownership proof differs from its source snapshot",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}
