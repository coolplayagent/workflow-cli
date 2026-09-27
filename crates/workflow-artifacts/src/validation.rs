use crate::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use workflow_ir::ValueType;
use workflow_validator::{identifier, pinned_version};
pub fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
pub fn validate_type(ty: &ArtifactType) -> Result<()> {
    if !identifier(&ty.identity.id) || !pinned_version(&ty.identity.version) {
        return Err(Error::new(
            ErrorCode::InvalidContract,
            "artifact type requires a stable ID and exact version",
        ));
    }
    if let ContentSchema::Json { value_type } = &ty.content {
        let mut pending = vec![(value_type, 0)];
        let mut count = 0;
        while let Some((t, depth)) = pending.pop() {
            count += 1;
            if depth > 16 || count > 4096 {
                return Err(Error::new(
                    ErrorCode::Budget,
                    "artifact type exceeds depth/member budget",
                ));
            }
            match t {
                ValueType::Array { items } => pending.push((items, depth + 1)),
                ValueType::Object { fields } => {
                    if fields.len() > 128 || fields.keys().any(|k| k.is_empty() || k.len() > 128) {
                        return Err(Error::new(
                            ErrorCode::InvalidContract,
                            "invalid artifact JSON members",
                        ));
                    }
                    pending.extend(fields.values().map(|v| (v, depth + 1)));
                }
                _ => {}
            }
        }
    }
    Ok(())
}
pub fn validate_spec(spec: &PublishSpec) -> Result<()> {
    if spec.schema_version != 1 {
        return Err(Error::new(
            ErrorCode::InvalidContract,
            "artifact spec requires version 1",
        ));
    }
    validate_type(&spec.artifact_type)?;
    for id in [
        &spec.producer.run_id,
        &spec.producer.node_instance_id,
        &spec.producer.attempt_id,
    ] {
        if !identifier(id) {
            return Err(Error::new(
                ErrorCode::InvalidContract,
                "producer identity required",
            ));
        }
    }
    if !codec::valid_digest(&spec.producer.request_digest)
        || !codec::valid_digest(&spec.producer.input_digest)
    {
        return Err(Error::new(
            ErrorCode::InvalidContract,
            "producer request/input sha256 required",
        ));
    }
    let AccessScope::Run { run_id } = &spec.access;
    if run_id != &spec.producer.run_id {
        return Err(Error::new(
            ErrorCode::ScopeMismatch,
            "artifact access must match its producing run",
        ));
    }
    let source = &spec.source_revision;
    if source.repository.trim().is_empty()
        || source.repository.len() > 1024
        || !matches!(source.revision.len(), 40 | 64)
        || !source
            .revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::new(
            ErrorCode::InvalidContract,
            "source requires a repository identity and full lowercase Git revision",
        ));
    }
    let mut ids = BTreeSet::new();
    if spec.inputs.len() > 128 {
        return Err(Error::new(
            ErrorCode::Budget,
            "at most 128 direct input artifacts",
        ));
    }
    for input in &spec.inputs {
        validate_link(input)?;
        if !ids.insert(&input.artifact_id) {
            return Err(Error::new(
                ErrorCode::InvalidReference,
                "duplicate input artifact",
            ));
        }
    }
    if to_message(spec)?.len() > MAX_MANIFEST_BYTES {
        return Err(Error::new(
            ErrorCode::Budget,
            "artifact spec exceeds 64 KiB",
        ));
    }
    Ok(())
}
pub fn validate_content(ty: &ArtifactType, bytes: &[u8]) -> Result<()> {
    validate_type(ty)?;
    if bytes.len() as u64 > MAX_CONTENT_BYTES {
        return Err(Error::new(ErrorCode::Budget, "artifact exceeds 64 MiB"));
    }
    match &ty.content {
        ContentSchema::Bytes => {}
        ContentSchema::Utf8 => {
            std::str::from_utf8(bytes)
                .map_err(|e| Error::new(ErrorCode::InvalidContent, e.to_string()))?;
        }
        ContentSchema::Json { value_type } => {
            let v: serde_json::Value = parse_message(bytes)?;
            if !value_type.accepts(&v) {
                return Err(Error::new(
                    ErrorCode::InvalidContent,
                    "artifact JSON differs from its exact type contract",
                ));
            }
        }
    }
    Ok(())
}
pub fn reference(spec: &PublishSpec, bytes: &[u8]) -> Result<ArtifactRef> {
    validate_spec(spec)?;
    validate_content(&spec.artifact_type, bytes)?;
    from_manifest(Manifest {
        spec: spec.clone(),
        content_digest: content_digest(bytes),
        bytes: bytes.len() as u64,
    })
}
pub fn from_manifest(manifest: Manifest) -> Result<ArtifactRef> {
    validate_spec(&manifest.spec)?;
    if !codec::valid_digest(&manifest.content_digest) || manifest.bytes > MAX_CONTENT_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "invalid content digest or size",
        ));
    }
    let digest = digest(&manifest)?;
    let artifact_id = format!("artifact-{}", &digest[7..]);
    let r = ArtifactRef {
        location: format!("artifact://{artifact_id}"),
        artifact_id,
        digest,
        manifest,
    };
    if to_message(&r)?.len() > MAX_MANIFEST_BYTES {
        return Err(Error::new(
            ErrorCode::Budget,
            "artifact reference exceeds 64 KiB",
        ));
    }
    Ok(r)
}
pub fn validate_link(r: &ArtifactLink) -> Result<()> {
    if !codec::valid_digest(&r.digest) || r.artifact_id != format!("artifact-{}", &r.digest[7..]) {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "artifact ID must bind its manifest digest",
        ));
    }
    Ok(())
}
pub fn validate_ref(r: &ArtifactRef) -> Result<()> {
    if from_manifest(r.manifest.clone())? != *r {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "artifact reference/manifest identity mismatch",
        ));
    }
    Ok(())
}
pub fn verify_expected(
    reader: &dyn ArtifactReader,
    link: &ArtifactLink,
    expected: &ArtifactType,
) -> Result<ArtifactRef> {
    validate_type(expected)?;
    let r = reader.verify(link)?;
    if &r.manifest.spec.artifact_type != expected {
        return Err(Error::new(
            ErrorCode::TypeConflict,
            "artifact does not match the consumer's expected type",
        ));
    }
    Ok(r)
}
