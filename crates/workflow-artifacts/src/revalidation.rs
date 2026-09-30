use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub old: ArtifactLink,
    pub new: ArtifactLink,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevalidationPolicy {
    RequireFreshEvidence,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffectedEvidence {
    pub artifact: ArtifactLink,
    pub producer: Producer,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevalidationPlan {
    pub schema_version: u32,
    pub policy: RevalidationPolicy,
    pub inventory_digest: String,
    pub replacements: Vec<Replacement>,
    pub affected: Vec<AffectedEvidence>,
    pub decision_summary: String,
}
pub fn revalidation_type() -> ArtifactType {
    ArtifactType {
        identity: workflow_ir::VersionRef {
            id: "artifact.revalidation-plan".into(),
            version: "1.0.0".into(),
        },
        content: ContentSchema::Utf8,
    }
}
fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidContract, message)
}
fn replacements(
    reader: &dyn ArtifactReader,
    replacements: &[Replacement],
) -> Result<BTreeSet<String>> {
    if replacements.is_empty() || replacements.len() > 64 {
        return Err(invalid("revalidation requires 1..64 replacements"));
    }
    let mut old = BTreeSet::new();
    let mut new = BTreeSet::new();
    for replacement in replacements {
        validate_link(&replacement.old)?;
        validate_link(&replacement.new)?;
        if !old.insert(replacement.old.artifact_id.clone())
            || !new.insert(replacement.new.artifact_id.clone())
        {
            return Err(invalid("duplicate replacement"));
        }
        let before = reader.verify(&replacement.old)?;
        let after = reader.verify(&replacement.new)?;
        if before.manifest.spec.artifact_type != after.manifest.spec.artifact_type
            || before.manifest.spec.access != after.manifest.spec.access
        {
            return Err(invalid(
                "replacement must retain the exact type and run scope",
            ));
        }
    }
    if !old.is_disjoint(&new) {
        return Err(invalid(
            "replacement cannot retain or chain through superseded artifacts",
        ));
    }
    for replacement in replacements {
        if affected(reader, &replacement.new, &old)? {
            return Err(invalid("replacement still depends on superseded input"));
        }
    }
    Ok(old)
}
fn affected(
    reader: &dyn ArtifactReader,
    link: &ArtifactLink,
    superseded: &BTreeSet<String>,
) -> Result<bool> {
    let mut pending = vec![link.clone()];
    let mut seen = BTreeSet::new();
    while let Some(link) = pending.pop() {
        if !seen.insert(link.artifact_id.clone()) {
            continue;
        }
        if seen.len() > MAX_LINEAGE {
            return Err(Error::new(
                ErrorCode::Budget,
                "revalidation dependency budget exceeded",
            ));
        }
        if superseded.contains(&link.artifact_id) {
            return Ok(true);
        }
        let artifact = reader.verify(&link)?;
        pending.extend(artifact.manifest.spec.inputs);
    }
    Ok(false)
}
/// Exact affected evidence/producer projection for this retained inventory.
/// New publications are checked transitively by InvalidatedReader too.
pub fn plan_revalidation(
    store: &dyn ArtifactInventory,
    changes: &[Replacement],
    summary: &str,
) -> Result<RevalidationPlan> {
    if summary.trim().is_empty() || summary.len() > 4096 || summary.contains('\0') {
        return Err(invalid("bounded decision summary required"));
    }
    let old = replacements(store, changes)?;
    let inventory = store.retained_manifests()?;
    if inventory.len() > MAX_CATALOG {
        return Err(Error::new(
            ErrorCode::Budget,
            "inventory exceeds supported capacity",
        ));
    }
    let mut records = BTreeMap::new();
    for artifact in inventory {
        validate_ref(&artifact)?;
        let link = artifact.link();
        if store.verify(&link)? != artifact || records.insert(link.artifact_id, artifact).is_some()
        {
            return Err(invalid("artifact inventory mismatch"));
        }
    }
    let mut impacted = old.clone();
    loop {
        let count = impacted.len();
        for artifact in records.values() {
            if artifact
                .manifest
                .spec
                .inputs
                .iter()
                .any(|l| impacted.contains(&l.artifact_id))
            {
                impacted.insert(artifact.artifact_id.clone());
            }
        }
        if impacted.len() == count {
            break;
        }
    }
    let affected = records
        .values()
        .filter(|r| impacted.contains(&r.artifact_id))
        .map(|r| AffectedEvidence {
            artifact: r.link(),
            producer: r.manifest.spec.producer.clone(),
        })
        .collect();
    let inventory_digest = digest(&records.values().map(|r| r.link()).collect::<Vec<_>>())?;
    let plan = RevalidationPlan {
        schema_version: 1,
        policy: RevalidationPolicy::RequireFreshEvidence,
        inventory_digest,
        replacements: changes.to_vec(),
        affected,
        decision_summary: summary.into(),
    };
    to_message(&plan)?;
    Ok(plan)
}

/// A host-selected current evidence view. Original immutable artifacts/history
/// remain available through the historical reader; old successes are never rewritten.
/// New reports which still depend on an old input are rejected as well.
pub struct InvalidatedReader {
    reader: Box<dyn ArtifactReader>,
    superseded: BTreeSet<String>,
}
impl InvalidatedReader {
    pub fn new(reader: Box<dyn ArtifactReader>, plan: &RevalidationPlan) -> Result<Self> {
        if plan.schema_version != 1
            || !codec::valid_digest(&plan.inventory_digest)
            || plan.decision_summary.trim().is_empty()
            || plan.decision_summary.len() > 4096
            || plan.decision_summary.contains('\0')
        {
            return Err(invalid("invalid revalidation plan"));
        }
        let superseded = replacements(reader.as_ref(), &plan.replacements)?;
        Ok(Self { reader, superseded })
    }
}
impl ArtifactReader for InvalidatedReader {
    fn verify(&self, link: &ArtifactLink) -> Result<ArtifactRef> {
        validate_link(link)?;
        if affected(self.reader.as_ref(), link, &self.superseded)? {
            return Err(Error::new(
                ErrorCode::InvalidReference,
                "evidence depends on superseded input; recomputation under current inputs required",
            ));
        }
        self.reader.verify(link)
    }
}
