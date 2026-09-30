use crate::*;
use std::collections::{BTreeMap, BTreeSet};
use workflow_artifacts::{
    ArtifactLink, ArtifactStore, ArtifactType, ContentSchema, SourceRevision,
};
use workflow_ir::VersionRef;

pub fn proposal_type() -> ArtifactType {
    ArtifactType {
        identity: VersionRef {
            id: "workspace.merge-proposal".into(),
            version: "1.0.0".into(),
        },
        content: ContentSchema::Utf8,
    }
}
pub fn merge_type() -> ArtifactType {
    ArtifactType {
        identity: VersionRef {
            id: "workspace.merge-commit".into(),
            version: "1.0.0".into(),
        },
        content: ContentSchema::Utf8,
    }
}
pub fn validate_summary(summary: &str) -> Result<()> {
    if summary.trim().is_empty() || summary.len() > 4096 || summary.contains('\0') {
        return Err(Error::new(
            ErrorCode::InvalidContract,
            "decision summary requires 1..4096 UTF-8 bytes without NUL",
        ));
    }
    Ok(())
}
fn entries(files: &[SourceFile]) -> Vec<FileEntry> {
    files
        .iter()
        .map(|f| FileEntry {
            path: f.path.clone(),
            digest: content_digest(&f.bytes),
            bytes: f.bytes.len() as u64,
            executable: f.executable,
        })
        .collect()
}
fn load_proposal(
    link: &ArtifactLink,
    artifacts: &dyn ArtifactStore,
    source: &SourceRevision,
    baseline: &[FileEntry],
) -> Result<MergeProposal> {
    let reference = workflow_artifacts::verify_expected(artifacts, link, &proposal_type())?;
    let proposal: MergeProposal = parse_message(&artifacts.read(link)?)?;
    validate_ref(&proposal.workspace)?;
    validate_summary(&proposal.decision_summary)?;
    let spec = &proposal.workspace.manifest.spec;
    if spec.source_revision != *source
        || proposal.workspace.manifest.baseline != baseline
        || observation(&proposal.workspace, proposal.observation.files.clone())?
            != proposal.observation
        || proposal.observation.changes.len() > 64
        || proposal
            .observation
            .changes
            .iter()
            .any(|c| !c.declared_output)
    {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "proposal differs from its immutable base or declared outputs",
        ));
    }
    let mut expected_spec = publish_spec(spec, proposal_type());
    let mut inputs: BTreeMap<_, _> = expected_spec
        .inputs
        .iter()
        .map(|l| (l.artifact_id.clone(), l.clone()))
        .collect();
    let mut captured = BTreeSet::new();
    for file in &proposal.files {
        validate_path(&file.path)?;
        if !captured.insert(file.path.clone()) {
            return Err(corrupt("duplicate proposed file"));
        }
        let entry = proposal
            .observation
            .files
            .iter()
            .find(|f| f.path == file.path)
            .ok_or_else(|| corrupt("proposed file missing from observation"))?;
        if !proposal
            .observation
            .changes
            .iter()
            .any(|c| c.path == file.path && c.kind != ChangeKind::Deleted)
        {
            return Err(corrupt("proposal contains an unchanged file"));
        }
        let link = ArtifactLink {
            artifact_id: file.artifact_id.clone(),
            digest: file.digest.clone(),
        };
        let ty = &spec
            .outputs
            .iter()
            .find(|o| o.path == file.path)
            .ok_or_else(|| corrupt("undeclared proposed file"))?
            .artifact_type;
        let artifact = workflow_artifacts::verify_expected(artifacts, &link, ty)?;
        if artifact.manifest.spec != publish_spec(spec, ty.clone())
            || artifact.manifest.content_digest != entry.digest
            || artifact.manifest.bytes != entry.bytes
            || file.executable != entry.executable
        {
            return Err(corrupt("proposed file provenance/content changed"));
        }
        inputs.insert(link.artifact_id.clone(), link);
    }
    let wanted: BTreeSet<_> = proposal
        .observation
        .changes
        .iter()
        .filter(|c| c.kind != ChangeKind::Deleted)
        .map(|c| c.path.clone())
        .collect();
    expected_spec.inputs = inputs.into_values().collect();
    if wanted != captured || expected_spec != reference.manifest.spec {
        return Err(corrupt(
            "proposal does not bind its complete changed output set",
        ));
    }
    Ok(proposal)
}
fn corrupt(message: &str) -> Error {
    Error::new(ErrorCode::InvalidReference, message)
}

/// Deterministic, reviewed three-way file merge. Nonidentical concurrent edits
/// are conflicts; no proposal ordering can silently choose a winner.
pub fn plan_merge(
    source: &SourceTree,
    links: &[ArtifactLink],
    resolutions: &BTreeMap<String, String>,
    artifacts: &dyn ArtifactStore,
) -> Result<MergePlan> {
    if !(1..=16).contains(&links.len())
        || links
            .windows(2)
            .any(|w| w[0].artifact_id >= w[1].artifact_id)
    {
        return Err(corrupt(
            "merge requires 1..16 unique proposals sorted by artifact ID",
        ));
    }
    let baseline = entries(&source.files);
    validate_files(&baseline)?;
    let mut files: BTreeMap<_, _> = baseline
        .iter()
        .map(|f| (f.path.clone(), f.clone()))
        .collect();
    let mut changes: BTreeMap<String, Vec<(String, Option<FileEntry>)>> = BTreeMap::new();
    let mut run = None;
    for link in links {
        let proposal = load_proposal(link, artifacts, &source.source_revision, &baseline)?;
        let owner = proposal.workspace.manifest.spec.producer.run_id;
        if run.as_ref().is_some_and(|old| *old != owner) {
            return Err(corrupt("merge proposals belong to different runs"));
        }
        run = Some(owner);
        for change in &proposal.observation.changes {
            let entry = proposal
                .observation
                .files
                .iter()
                .find(|f| f.path == change.path)
                .cloned();
            changes
                .entry(change.path.clone())
                .or_default()
                .push((link.artifact_id.clone(), entry));
        }
    }
    let mut conflicts = vec![];
    let mut used = BTreeSet::new();
    for (path, candidates) in changes {
        let all_equal = candidates
            .iter()
            .all(|(_, entry)| *entry == candidates[0].1);
        let selected = if all_equal {
            Some(&candidates[0].1)
        } else if let Some(choice) = resolutions.get(&path) {
            used.insert(path.clone());
            Some(
                &candidates
                    .iter()
                    .find(|(id, _)| id == choice)
                    .ok_or_else(|| corrupt("resolution must select one of this path's proposals"))?
                    .1,
            )
        } else {
            conflicts.push(MergeConflict {
                path: path.clone(),
                proposals: candidates.iter().map(|(id, _)| id.clone()).collect(),
            });
            None
        };
        if let Some(entry) = selected {
            match entry {
                Some(value) => {
                    files.insert(path, value.clone());
                }
                None => {
                    files.remove(&path);
                }
            }
        }
    }
    if used != resolutions.keys().cloned().collect() {
        return Err(corrupt("unused or unnecessary merge resolution"));
    }
    let files: Vec<_> = files.into_values().collect();
    validate_files(&files)?;
    let plan = MergePlan {
        schema_version: 1,
        source_revision: source.source_revision.clone(),
        baseline_tree_digest: digest(&baseline)?,
        proposals: links.to_vec(),
        resolutions: resolutions.clone(),
        files,
        conflicts,
        requires_revalidation: true,
    };
    to_message(&plan)?;
    Ok(plan)
}
/// Rebuild exact bytes from verified immutable proposals, never from a later
/// mutable workspace. Every apply rechecks the complete plan and source tree.
pub fn merged_files(
    source: &SourceTree,
    plan: &MergePlan,
    artifacts: &dyn ArtifactStore,
) -> Result<Vec<SourceFile>> {
    if plan_merge(source, &plan.proposals, &plan.resolutions, artifacts)? != *plan
        || !plan.conflicts.is_empty()
    {
        return Err(Error::new(
            ErrorCode::Conflict,
            "merge plan changed or has unresolved conflicts",
        ));
    }
    let wanted: BTreeSet<_> = plan.files.iter().map(|f| f.digest.clone()).collect();
    let mut content: BTreeMap<_, _> = source
        .files
        .iter()
        .filter_map(|f| {
            let digest = content_digest(&f.bytes);
            wanted.contains(&digest).then(|| (digest, f.bytes.clone()))
        })
        .collect();
    let baseline = entries(&source.files);
    for link in &plan.proposals {
        let proposal = load_proposal(link, artifacts, &source.source_revision, &baseline)?;
        for file in proposal.files {
            let entry = proposal
                .observation
                .files
                .iter()
                .find(|entry| entry.path == file.path)
                .expect("validated proposal");
            if content.contains_key(&entry.digest) || !wanted.contains(&entry.digest) {
                continue;
            }
            let bytes = artifacts.read(&ArtifactLink {
                artifact_id: file.artifact_id,
                digest: file.digest,
            })?;
            content.insert(content_digest(&bytes), bytes);
        }
    }
    plan.files
        .iter()
        .map(|file| {
            let bytes = content
                .get(&file.digest)
                .ok_or_else(|| corrupt("merged payload unavailable"))?
                .clone();
            if bytes.len() as u64 != file.bytes {
                return Err(corrupt("merged payload length mismatch"));
            }
            Ok(SourceFile {
                path: file.path.clone(),
                bytes,
                executable: file.executable,
            })
        })
        .collect()
}
