use crate::*;
use std::collections::BTreeSet;
use workflow_artifacts::{ArtifactLink, validate_link, validate_type};
use workflow_ir::VersionRef;
use workflow_validator::{identifier, pinned_version};
fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidContract, message)
}
pub(crate) fn valid_digest(s: &str) -> bool {
    s.len() == 71
        && s.starts_with("sha256:")
        && s[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn version(v: &VersionRef) -> bool {
    identifier(&v.id) && pinned_version(&v.version)
}
fn links(items: &[ArtifactLink]) -> Result<()> {
    if items.len() > 128 {
        return Err(invalid("at most 128 artifacts"));
    }
    let mut seen = BTreeSet::new();
    for item in items {
        validate_link(item)?;
        if !seen.insert(&item.artifact_id) {
            return Err(invalid("duplicate artifact"));
        }
    }
    Ok(())
}
pub fn validate_policy(p: &Policy) -> Result<()> {
    if p.schema_version != 1 || !version(&p.identity) || !(1..=64).contains(&p.requirements.len()) {
        return Err(invalid(
            "policy requires version 1, exact identity and 1..64 requirements",
        ));
    }
    let mut ids = BTreeSet::new();
    for q in &p.requirements {
        // Budget recursive shapes before serializing any in-process request.
        validate_type(&q.report_type)?;
        if !matches!(&q.report_type.content, workflow_artifacts::ContentSchema::Json { value_type: workflow_ir::ValueType::Object { fields } }
            if fields.get(&q.pass_field) == Some(&workflow_ir::ValueType::Boolean))
        {
            return Err(invalid(
                "report must be a closed JSON object with the required Boolean field",
            ));
        }
        if !identifier(&q.id)
            || !identifier(&q.node_id)
            || !version(&q.capability)
            || !valid_digest(&q.contract_digest)
            || q.pass_field.is_empty()
            || q.pass_field.len() > 128
            || q.max_age_ms == 0
            || q.max_age_ms > 2_592_000_000
            || !ids.insert(&q.id)
        {
            return Err(invalid(
                "invalid/duplicate requirement; maximum age must be 1 ms..30 days",
            ));
        }
    }
    to_message(p)?;
    Ok(())
}
pub fn validate(r: &Request) -> Result<()> {
    validate_policy(&r.policy)?;
    let ids: BTreeSet<_> = r.policy.requirements.iter().map(|q| &q.id).collect();
    let t = &r.target;
    if !identifier(&t.run_id)
        || !valid_digest(&t.run_digest)
        || !version(&t.action)
        || !valid_digest(&t.input_digest)
        || t.source_revision.repository.trim().is_empty()
        || t.source_revision.repository.len() > 1024
        || !matches!(t.source_revision.revision.len(), 40 | 64)
        || !t
            .source_revision
            .revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid(
            "target requires a run, action, repository, full Git revision and SHA-256 input digest",
        ));
    }
    links(&t.artifacts)?;
    if r.evidence.len() > 64 {
        return Err(invalid("at most 64 evidence bindings"));
    }
    let mut seen = BTreeSet::new();
    for e in &r.evidence {
        validate_link(&e.report)?;
        if !ids.contains(&e.requirement_id) || !seen.insert(&e.requirement_id) {
            return Err(invalid("unknown/duplicate evidence requirement"));
        }
    }
    to_message(r)?;
    Ok(())
}
pub(crate) fn same_links(a: &[ArtifactLink], b: &[ArtifactLink]) -> bool {
    fn normalized(xs: &[ArtifactLink]) -> BTreeSet<(&String, &String)> {
        xs.iter()
            .map(|x| (&x.artifact_id, &x.digest))
            .collect::<BTreeSet<_>>()
    }
    // Duplicates are never normalized away.
    a.len() == b.len() && normalized(a).len() == a.len() && normalized(a) == normalized(b)
}
