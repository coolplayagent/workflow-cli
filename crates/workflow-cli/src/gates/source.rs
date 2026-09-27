use std::collections::BTreeMap;
use workflow_artifact_local::LocalArtifactStore;
use workflow_artifacts::{
    ArtifactLink, ArtifactReader, ArtifactRef, Error, ErrorCode, Producer, Result,
};
use workflow_gates::{EvidenceSource, ExecutedCheck};
use workflow_runstore::{ExecutionAction, ExecutionStore, PreparedTask, RunStore, executed_check};
use workflow_runstore_sqlite::SqliteRunStore;

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
                        checks.insert(
                            attempt_id,
                            executed_check(&before.run_digest, p, &result, at_unix_ms)
                                .map_err(invalid)?,
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
