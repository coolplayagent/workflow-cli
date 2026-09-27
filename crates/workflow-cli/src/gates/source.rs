use std::collections::BTreeMap;
use workflow_artifact_local::LocalArtifactStore;
use workflow_artifacts::{
    ArtifactLink, ArtifactReader, ArtifactRef, Error, ErrorCode, Producer, Result,
};
use workflow_gates::{CheckOutcome, EvidenceSource, ExecutedCheck};
use workflow_runstore::{
    ExecutionAction, ExecutionStore, PreparedTask, RunStore, artifact_producer,
};
use workflow_runstore_sqlite::SqliteRunStore;
use workflow_worker::{AdapterOutcome, FailureClass, InvocationScope};

pub(super) struct LocalEvidence {
    artifacts: LocalArtifactStore,
    checks: BTreeMap<String, ExecutedCheck>,
}
fn invalid(e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::InvalidReference, e.to_string())
}
impl LocalEvidence {
    pub(super) fn open(db: &str, root: &str, run_id: &str) -> Result<Self> {
        let artifacts = LocalArtifactStore::open(root)?;
        let mut store = SqliteRunStore::open(db)
            .map_err(invalid)?
            .with_artifacts(Box::new(LocalArtifactStore::open(root)?));
        let before = store.get(run_id).map_err(invalid)?;
        let mut prepared: BTreeMap<String, Box<PreparedTask>> = BTreeMap::new();
        let mut checks = BTreeMap::new();
        let mut cursor = 0;
        let mut count = 0;
        loop {
            // Every page recovers and verifies the durable authority chain and worker results.
            let page = store
                .execution_history(run_id, cursor, 100)
                .map_err(invalid)?;
            count += page.items.len();
            if count as u64 > workflow_runstore::MAX_EXECUTION_RECORDS {
                return Err(invalid("execution evidence budget exceeded"));
            }
            for record in page.items {
                match record.action {
                    ExecutionAction::Prepared { attempt } => {
                        prepared.insert(attempt.attempt_id.clone(), attempt);
                    }
                    ExecutionAction::Finished {
                        attempt_id,
                        result,
                        at_unix_ms,
                        ..
                    } => {
                        let p = prepared
                            .get(&attempt_id)
                            .ok_or_else(|| invalid("missing prepared check"))?;
                        let producer = artifact_producer(&p.request).map_err(invalid)?;
                        let InvocationScope::Workflow { node_id, .. } = &p.request.scope else {
                            return Err(invalid("workflow check required"));
                        };
                        let (outcome, evidence) = match result.outcome {
                            AdapterOutcome::Succeeded { outputs, evidence } => {
                                (CheckOutcome::Succeeded { outputs }, evidence)
                            }
                            AdapterOutcome::Failed {
                                code,
                                class,
                                evidence,
                                ..
                            } => {
                                let outcome = if class == FailureClass::Permanent {
                                    CheckOutcome::Failed { code }
                                } else {
                                    CheckOutcome::Inconclusive { code }
                                };
                                (outcome, evidence)
                            }
                        };
                        checks.insert(
                            attempt_id,
                            ExecutedCheck {
                                producer,
                                run_digest: before.run_digest.clone(),
                                node_id: node_id.clone(),
                                capability: p.request.capability.clone(),
                                contract_digest: p.request.contract_digest.clone(),
                                completed_at_unix_ms: result.completed_at_unix_ms,
                                settled_at_unix_ms: at_unix_ms,
                                outcome,
                                evidence: evidence
                                    .into_iter()
                                    .map(|e| ArtifactLink {
                                        artifact_id: e.artifact_id,
                                        digest: e.digest,
                                    })
                                    .collect(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            match page.next_cursor {
                Some(next) if next > cursor => cursor = next,
                Some(_) => return Err(invalid("non-advancing evidence cursor")),
                None => break,
            }
        }
        let after = store.get(run_id).map_err(invalid)?;
        if before.revision != after.revision || before.run_digest != after.run_digest {
            return Err(invalid(
                "run changed during evidence read; retry with the current target",
            ));
        }
        Ok(Self { artifacts, checks })
    }
}
impl ArtifactReader for LocalEvidence {
    fn verify(&self, link: &ArtifactLink) -> Result<ArtifactRef> {
        self.artifacts.verify(link)
    }
}
impl EvidenceSource for LocalEvidence {
    fn executed_check(&self, producer: &Producer) -> Result<Option<ExecutedCheck>> {
        Ok(self.checks.get(&producer.attempt_id).cloned())
    }
}
