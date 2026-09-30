use super::*;
use workflow_effects::{
    CallKind, EffectBinding, EffectIntent, ReleaseAuthorization, ReleaseIntent, ReleaseSubject,
};
use workflow_kernel::NodeState;

fn denied(message: &str) -> Error {
    Error::new(ErrorCode::TransitionRejected, message)
}

pub(super) fn freeze(
    r: &Recovered,
    entry: &OutboxEntry,
    binding: &EffectBinding,
) -> Result<Option<ReleaseIntent>> {
    let Some(policy) = &binding.release else {
        return Ok(None);
    };
    let Command::ExecuteTask {
        frame_id, inputs, ..
    } = &entry.command
    else {
        return Err(corrupt("release requires original task"));
    };
    let engine = r.engine.at_revision(entry.revision)?;
    let snapshot = engine.snapshot();
    let frame = &snapshot.frames[frame_id];
    let subject: ReleaseSubject = serde_json::from_value(
        inputs
            .get(&policy.subject_field)
            .cloned()
            .ok_or_else(|| denied("release subject input missing"))?,
    )
    .map_err(|_| denied("invalid release subject input"))?;
    let mut gates = vec![];
    for node_id in &policy.gate_nodes {
        let node = &frame.nodes[node_id];
        if node.state != NodeState::Succeeded || node.gate_decision.is_none() {
            return Err(denied("release prerequisite has no admitted gate"));
        }
        let context = r
            .outbox
            .iter()
            .rev()
            .filter(|e| e.revision <= entry.revision)
            .find_map(|e| match &e.command {
                Command::CheckGate {
                    instance_id,
                    context,
                } if *instance_id == node.instance_id => Some(*context.clone()),
                _ => None,
            })
            .ok_or_else(|| corrupt("release gate command is missing"))?;
        gates.push(context);
    }
    let mut approvals = vec![];
    for required in &policy.approvals {
        let index = policy
            .gate_nodes
            .iter()
            .position(|n| n == &required.gate_node)
            .ok_or_else(|| corrupt("release approval gate missing"))?;
        let c = &gates[index];
        let approval = engine
            .approval_evidence(
                *frame_id,
                &required.approval,
                &c.policy,
                &c.target,
                snapshot.now_unix_ms,
            )?
            .filter(|a| a.exception.is_none())
            .ok_or_else(|| denied("release requires a current applied ordinary approval"))?;
        approvals.push(approval);
    }
    Ok(Some(ReleaseIntent {
        policy: policy.clone(),
        subject,
        gates,
        approvals,
    }))
}

pub(super) fn authorize(
    r: &Recovered,
    authority: &Authority,
    intent: &EffectIntent,
    kind: CallKind,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
    now: u64,
    revision: u64,
) -> Result<Option<ReleaseAuthorization>> {
    let Some(release) = &intent.release else {
        return Ok(None);
    };
    if kind == CallKind::Query {
        return Ok(None);
    }
    let gates = release
        .gates
        .iter()
        .map(|context| super::gates::evaluate(r, authority, context, artifacts, now, revision))
        .collect::<Result<Vec<_>>>()?;
    let expires_at_unix_ms = gates
        .iter()
        .filter_map(|g| g.admission_deadline())
        .chain(release.approvals.iter().map(|a| a.expires_at_unix_ms))
        .min()
        .unwrap_or(now);
    let grant = ReleaseAuthorization {
        intent_digest: digest(release)?,
        evaluated_at_unix_ms: now,
        expires_at_unix_ms,
        gates,
    };
    grant.validate(release, now)?;
    Ok(Some(grant))
}

impl SqliteRunStore {
    /// Recheck immutable artifact bytes and current evidence before handing the
    /// already prepared call to a worker. The gateway still compares its actual
    /// target, atomically where its frozen contract promises that behavior.
    pub fn validate_effect_delivery(
        &mut self,
        attempt: &workflow_effects::EffectAttempt,
        now: u64,
    ) -> Result<()> {
        let tx = self.connection.transaction().map_err(storage)?;
        let r = crate::recovery::recover(&tx, &attempt.intent.run_id, self.artifacts.as_deref())?;
        let (a, _) = read(&tx, &r, self.artifacts.as_deref())?;
        let lease = a
            .lease
            .as_ref()
            .ok_or_else(|| denied("release lease missing"))?;
        a.check_live(lease, now)?;
        let snapshot = r.engine.snapshot();
        let node = snapshot
            .frames
            .values()
            .flat_map(|f| f.nodes.values())
            .find(|n| n.instance_id == attempt.intent.instance_id)
            .ok_or_else(|| denied("release node missing"))?;
        if lease.epoch != attempt.epoch
            || snapshot.pause.is_some()
            || (attempt.kind == CallKind::Write
                && (node.state != NodeState::TaskReady
                    || node.cancel_requested
                    || a.recovery.is_some()))
        {
            return Err(denied(
                "release dispatch is fenced by lease, pause, cancellation or recovery",
            ));
        }
        let call = a
            .effects
            .get(&attempt.intent.operation_key)
            .and_then(|s| s.calls.last())
            .ok_or_else(|| denied("release call missing"))?;
        if &call.attempt != attempt
            || call.observation.is_some()
            || now < attempt.issued_at_unix_ms
            || now >= attempt.deadline_unix_ms
        {
            return Err(denied("release call is stale or expired"));
        }
        if attempt.kind == CallKind::Write {
            authorize(
                &r,
                &a,
                &attempt.intent,
                attempt.kind,
                self.artifacts.as_deref(),
                now,
                r.engine.snapshot().revision,
            )?;
        }
        tx.commit().map_err(storage)?;
        Ok(())
    }
}
