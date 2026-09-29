use super::*;
use std::collections::{BTreeMap, BTreeSet};
use workflow_artifacts::{ArtifactLink, ArtifactReader, ArtifactRef};

pub(super) const MAX_RUN_ARTIFACTS: usize = 512;
pub(super) const MAX_RUN_CONTENT_BYTES: u64 = 512 * 1024 * 1024;
const SCHEMA: &str = include_str!("artifact_schema.sql");

pub(super) fn exists(tx: &mut Transaction<'_>) -> Result<bool> {
    Ok(tx
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_artifacts')",
            &[],
        )
        .map_err(storage)?
        .get(0))
}
pub(super) fn check(tx: &mut Transaction<'_>) -> Result<()> {
    let version: i32 = tx
        .query_one(
            "SELECT version FROM workflow_artifacts.schema_version WHERE singleton=true",
            &[],
        )
        .map_err(storage)?
        .get(0);
    if version != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "unsupported artifact schema",
        ));
    }
    Ok(())
}
pub(super) fn initialize(tx: &mut Transaction<'_>) -> Result<()> {
    tx.query_one("SELECT pg_advisory_xact_lock(57465233)", &[])
        .map_err(storage)?;
    if !exists(tx)? {
        tx.batch_execute(SCHEMA).map_err(storage)?;
    }
    check(tx)
}

/// Owned verification snapshot: content has already been checked in the same
/// transaction while the run is locked. Only manifests are retained in memory.
/// No transaction reference or filesystem path escapes into the reducer.
pub(super) struct Catalog {
    records: BTreeMap<String, ArtifactRef>,
}
impl Catalog {
    pub(super) fn closure(
        &self,
        root: &ArtifactLink,
    ) -> workflow_artifacts::Result<Vec<ArtifactRef>> {
        workflow_artifacts::validate_link(root)?;
        let mut pending = vec![(root.clone(), false)];
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut result = vec![];
        while let Some((link, done)) = pending.pop() {
            let item = self.records.get(&link.artifact_id).ok_or_else(|| {
                workflow_artifacts::Error::new(
                    workflow_artifacts::ErrorCode::NotFound,
                    "artifact dependency missing",
                )
            })?;
            if item.link() != link {
                return Err(workflow_artifacts::Error::new(
                    workflow_artifacts::ErrorCode::CorruptStorage,
                    "artifact dependency digest mismatch",
                ));
            }
            if done {
                visiting.remove(&link.artifact_id);
                if visited.insert(link.artifact_id) {
                    result.push(item.clone());
                }
                continue;
            }
            if visited.contains(&link.artifact_id) {
                continue;
            }
            if !visiting.insert(link.artifact_id.clone())
                || visiting.len() + visited.len() > workflow_artifacts::MAX_LINEAGE
            {
                return Err(workflow_artifacts::Error::new(
                    workflow_artifacts::ErrorCode::CorruptStorage,
                    "artifact dependency graph invalid",
                ));
            }
            pending.push((link, true));
            for input in item.manifest.spec.inputs.iter().rev() {
                pending.push((input.clone(), false));
            }
        }
        Ok(result)
    }
}
impl ArtifactReader for Catalog {
    fn verify(&self, link: &ArtifactLink) -> workflow_artifacts::Result<ArtifactRef> {
        self.closure(link)?.pop().ok_or_else(|| {
            workflow_artifacts::Error::new(
                workflow_artifacts::ErrorCode::NotFound,
                "artifact dependency missing",
            )
        })
    }
}

pub(super) fn load(
    tx: &mut Transaction<'_>,
    who: &Identity,
    run: &str,
) -> Result<Option<Box<dyn ArtifactReader>>> {
    Ok(read(tx, who, run)?.map(|c| Box::new(c) as Box<dyn ArtifactReader>))
}

pub(super) fn read(tx: &mut Transaction<'_>, who: &Identity, run: &str) -> Result<Option<Catalog>> {
    // Existing deployments keep their read-only behavior until a trusted local
    // initialization. A run that needs evidence still fails without a reader.
    if !exists(tx)? {
        return Ok(None);
    }
    check(tx)?;
    let rows = tx.query(
        "SELECT id,CASE WHEN octet_length(reference)<=65536 THEN reference ELSE NULL END,octet_length(content) FROM workflow_artifacts.artifacts WHERE tenant=$1 AND project=$2 AND run_id=$3 ORDER BY id COLLATE \"C\" LIMIT 513",
        &[&who.tenant, &who.project, &run],
    ).map_err(storage)?;
    if rows.len() > MAX_RUN_ARTIFACTS {
        return Err(corrupt("artifact catalog exceeds supported capacity"));
    }
    let mut catalog = Catalog {
        records: BTreeMap::new(),
    };
    let mut total = 0u64;
    for row in rows {
        let id: String = row.get(0);
        let document: Option<String> = row.get(1);
        let reference: ArtifactRef = serde_json::from_str(
            document
                .as_deref()
                .ok_or_else(|| corrupt("artifact manifest exceeds capacity"))?,
        )
        .map_err(|_| corrupt("artifact manifest invalid"))?;
        workflow_artifacts::validate_ref(&reference)?;
        let bytes: i32 = row.get(2);
        if bytes < 0
            || bytes as u64 != reference.manifest.bytes
            || reference.artifact_id != id
            || reference.manifest.spec.producer.run_id != run
        {
            return Err(corrupt("artifact catalog identity mismatch"));
        }
        total = total
            .checked_add(bytes as u64)
            .ok_or_else(|| corrupt("artifact catalog exceeds supported capacity"))?;
        if total > MAX_RUN_CONTENT_BYTES {
            return Err(corrupt("artifact catalog exceeds supported capacity"));
        }
        // Fetch only one bounded object at a time, after checking its length.
        let content: Vec<u8> = tx.query_one(
            "SELECT content FROM workflow_artifacts.artifacts WHERE tenant=$1 AND project=$2 AND id=$3",
            &[&who.tenant, &who.project, &id],
        ).map_err(storage)?.get(0);
        if workflow_artifacts::reference(&reference.manifest.spec, &content)? != reference {
            return Err(corrupt("artifact content differs from manifest"));
        }
        catalog.records.insert(id, reference);
        live(tx, who)?;
    }
    for item in catalog.records.values() {
        catalog.verify(&item.link())?;
    }
    Ok(Some(catalog))
}
