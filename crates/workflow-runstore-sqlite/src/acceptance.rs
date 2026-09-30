use crate::*;
use std::collections::BTreeMap;
use workflow_kernel::{EventKind, NodeState, SignalStatus};

impl SqliteRunStore {
    pub(crate) fn acceptance_manifest(&mut self, id: &str) -> Result<AcceptanceManifest> {
        let tx = self.connection.transaction().map_err(storage)?;
        let recovered = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        let (authority, records) =
            crate::execution::read(&tx, &recovered, self.artifacts.as_deref())?;
        let snapshot = recovered.engine.snapshot();
        let mut checks = vec![];
        let mut artifacts = BTreeMap::new();
        let mut links = vec![];
        for recorded in &recovered.events {
            if let EventKind::GateEvaluated {
                instance_id,
                evaluation,
                ..
            } = &recorded.event.kind
            {
                links.extend(evaluation.request.target.artifacts.clone());
                links.extend(evaluation.request.evidence.iter().map(|e| e.report.clone()));
                checks.push(AcceptanceCheck {
                    event_id: recorded.event.event_id.clone(),
                    revision: recorded.revision,
                    instance_id: *instance_id,
                    evaluation: *evaluation.clone(),
                });
            }
        }
        for attempt in authority.attempts.values() {
            if let Some(AttemptOutcome::Finished { result, .. }) = &attempt.outcome
                && let workflow_worker::AdapterOutcome::Succeeded { evidence, .. } = &result.outcome
            {
                links.extend(evidence.iter().map(|e| workflow_artifacts::ArtifactLink {
                    artifact_id: e.artifact_id.clone(),
                    digest: e.digest.clone(),
                }));
            }
        }
        for link in links {
            if artifacts.contains_key(&link.artifact_id) {
                continue;
            }
            let r = self
                .artifacts
                .as_ref()
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::ArtifactUnavailable,
                        "acceptance needs verified artifact bytes",
                    )
                })?
                .verify(&link)?;
            if r.link() != link {
                return Err(Error::new(
                    ErrorCode::ArtifactRejected,
                    "acceptance artifact identity differs",
                ));
            }
            artifacts.insert(link.artifact_id, r);
        }
        let root = &snapshot.frames[&1];
        let bundle = recovered.engine.bundle();
        let definition = bundle
            .spec()
            .workflows
            .iter()
            .find(|w| w.id == root.workflow.id && w.version == root.workflow.version)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::CorruptStorage,
                    "acceptance root definition missing",
                )
            })?;
        let has_terminal_gate = definition.nodes.iter().any(|n| {
            matches!(n.kind, workflow_ir::NodeKind::Terminal { .. })
                && root.nodes[&n.id].state == NodeState::Succeeded
                && root.nodes[&n.id].gate_decision.is_some()
        });
        let accepted = snapshot.status == RunStatus::Succeeded
            && has_terminal_gate
            && authority.recovery.is_none();
        let status = if !accepted {
            AcceptanceStatus::Incomplete
        } else if checks.iter().any(|c| c.evaluation.exception.is_some()) {
            AcceptanceStatus::AcceptedWithExceptions
        } else {
            AcceptanceStatus::Accepted
        };
        let mut manifest = AcceptanceManifest {
            schema_version: 1,
            run_id: id.into(),
            run_digest: snapshot.run_digest.clone(),
            bundle_digest: snapshot.bundle_digest.clone(),
            revision: snapshot.revision,
            completed_at_unix_ms: snapshot.now_unix_ms,
            status,
            requirements: recovered.engine.bundle().spec().postconditions.clone(),
            artifacts: artifacts.into_values().collect(),
            checks,
            approvals: snapshot
                .inbox
                .values()
                .filter(|e| matches!(e.status, SignalStatus::Applied { .. }))
                .cloned()
                .collect(),
            effects: authority.effects.into_values().collect(),
            snapshot_digest: workflow_worker::digest(snapshot)?,
            execution_digest: workflow_worker::digest(&records)?,
            digest: String::new(),
        };
        manifest.seal()?;
        tx.commit().map_err(storage)?;
        Ok(manifest)
    }
}
