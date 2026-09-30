use crate::*;
use std::collections::{BTreeMap, BTreeSet};
use workflow_artifacts::{AccessScope, ArtifactType, ContentSchema, PublishSpec, Retention};
use workflow_ir::{ValueType, VersionRef};
fn invalid(m: &str) -> Error {
    Error::new(ErrorCode::InvalidContract, m)
}
pub fn valid_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub fn valid_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn validate_path(p: &str) -> Result<()> {
    if p.is_empty()
        || p.len() > 1024
        || p.contains('\\')
        || p.bytes().any(|b| b < 32 || b == 127)
        || p.split('/').count() > 32
        || p.split('/').any(|c| {
            c.is_empty()
                || c == "."
                || c == ".."
                || c.eq_ignore_ascii_case(".git")
                || c.contains(':')
                || c.len() > 255
                || c.ends_with(['.', ' '])
                || matches!(
                    c.split('.').next().unwrap().to_ascii_uppercase().as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
    {
        return Err(invalid(
            "portable relative file path required, without .git, traversal or control characters",
        ));
    }
    Ok(())
}
pub fn file_type() -> ArtifactType {
    ArtifactType {
        identity: VersionRef {
            id: "workspace.file".into(),
            version: "1.0.0".into(),
        },
        content: ContentSchema::Bytes,
    }
}
pub fn output_type() -> ArtifactType {
    let string = ValueType::String;
    let fields = BTreeMap::from([
        ("workspace_id".into(), string.clone()),
        ("workspace_digest".into(), string.clone()),
        ("baseline_tree_digest".into(), string.clone()),
        ("observed_tree_digest".into(), string.clone()),
        ("clean".into(), ValueType::Boolean),
        (
            "files".into(),
            ValueType::Array {
                items: Box::new(ValueType::Object {
                    fields: BTreeMap::from([
                        ("path".into(), string.clone()),
                        ("executable".into(), ValueType::Boolean),
                        ("artifact_id".into(), string.clone()),
                        ("digest".into(), string),
                    ]),
                }),
            },
        ),
    ]);
    ArtifactType {
        identity: VersionRef {
            id: "workspace.output-manifest".into(),
            version: "1.0.0".into(),
        },
        content: ContentSchema::Json {
            value_type: ValueType::Object { fields },
        },
    }
}
pub fn publish_spec(spec: &CheckoutSpec, ty: ArtifactType) -> PublishSpec {
    PublishSpec {
        schema_version: 1,
        artifact_type: ty,
        producer: spec.producer.clone(),
        source_revision: spec.source_revision.clone(),
        inputs: spec.inputs.clone(),
        access: AccessScope::Run {
            run_id: spec.producer.run_id.clone(),
        },
        retention: Retention::RunDependency,
    }
}
pub fn validate_spec(s: &CheckoutSpec) -> Result<()> {
    if s.schema_version != 1 || s.outputs.len() > 64 || s.inputs.len() > 64 {
        return Err(invalid(
            "workspace schema 1, at most 64 input artifacts and 64 output paths required",
        ));
    }
    workflow_artifacts::validate_spec(&publish_spec(s, file_type()))?;
    let mut paths = BTreeSet::new();
    for output in &s.outputs {
        validate_path(&output.path)?;
        workflow_artifacts::validate_type(&output.artifact_type)?;
        if !paths.insert(output.path.to_ascii_lowercase()) {
            return Err(invalid("duplicate or case-colliding output path"));
        }
    }
    for path in &paths {
        for (i, _) in path.match_indices('/') {
            if paths.contains(&path[..i]) {
                return Err(invalid("output file/directory collision"));
            }
        }
    }
    to_message(s)?;
    Ok(())
}
pub fn workspace_id(s: &CheckoutSpec) -> Result<String> {
    validate_spec(s)?;
    Ok(format!(
        "workspace-{}",
        &digest(&(
            &s.producer.run_id,
            &s.producer.node_instance_id,
            &s.producer.attempt_id
        ))?[7..]
    ))
}
pub fn validate_files(files: &[FileEntry]) -> Result<()> {
    if files.len() > MAX_FILES {
        return Err(Error::new(
            ErrorCode::Budget,
            "workspace exceeds 4096 files",
        ));
    }
    let mut total = 0u64;
    let mut previous = None;
    let mut folded = BTreeSet::new();
    let mut directories = BTreeSet::new();
    for f in files {
        validate_path(&f.path)?;
        if !valid_digest(&f.digest) || f.bytes > MAX_FILE_BYTES {
            return Err(invalid("file digest/size invalid"));
        }
        if previous.is_some_and(|p: &str| p >= f.path.as_str())
            || !folded.insert(f.path.to_ascii_lowercase())
        {
            return Err(invalid(
                "files must have unique sorted paths without case collisions",
            ));
        }
        for (i, _) in f.path.match_indices('/') {
            directories.insert(f.path[..i].to_ascii_lowercase());
        }
        previous = Some(&f.path);
        total = total
            .checked_add(f.bytes)
            .ok_or_else(|| invalid("tree size overflow"))?;
    }
    if !folded.is_disjoint(&directories) {
        return Err(invalid("file/directory collision"));
    }
    if directories.len() >= MAX_FILES {
        return Err(Error::new(
            ErrorCode::Budget,
            "workspace exceeds directory budget",
        ));
    }
    if total > MAX_TREE_BYTES {
        return Err(Error::new(ErrorCode::Budget, "workspace exceeds 256 MiB"));
    }
    to_message(&files)?;
    Ok(())
}
pub fn reference(manifest: WorkspaceManifest) -> Result<WorkspaceRef> {
    validate_spec(&manifest.spec)?;
    validate_files(&manifest.baseline)?;
    if !valid_oid(&manifest.git_tree) || manifest.tree_digest != digest(&manifest.baseline)? {
        return Err(invalid("invalid baseline tree identity"));
    }
    for s in [
        &manifest.environment.os,
        &manifest.environment.architecture,
        &manifest.environment.git_version,
    ] {
        if s.is_empty() || s.len() > 256 || s.chars().any(char::is_control) {
            return Err(invalid("bounded observed environment required"));
        }
    }
    if manifest.environment.tools.len() > 32
        || manifest.environment.tools.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 128
                || value.is_empty()
                || value.len() > 256
                || key.chars().any(char::is_control)
                || value.chars().any(char::is_control)
        })
    {
        return Err(invalid("observed tool versions exceed supported bounds"));
    }
    let id = workspace_id(&manifest.spec)?;
    let r = WorkspaceRef {
        workspace_id: id.clone(),
        digest: digest(&manifest)?,
        location: format!("workspace://{id}"),
        manifest,
    };
    to_message(&r)?;
    Ok(r)
}
pub fn validate_ref(r: &WorkspaceRef) -> Result<()> {
    if reference(r.manifest.clone())? != *r {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "workspace reference differs from manifest",
        ));
    }
    Ok(())
}
pub fn validate_link(l: &WorkspaceLink) -> Result<()> {
    if !l
        .workspace_id
        .strip_prefix("workspace-")
        .is_some_and(|s| valid_digest(&format!("sha256:{s}")))
        || !valid_digest(&l.digest)
    {
        return Err(Error::new(
            ErrorCode::InvalidReference,
            "exact workspace ID and digest required",
        ));
    }
    Ok(())
}
pub fn observation(r: &WorkspaceRef, files: Vec<FileEntry>) -> Result<Observation> {
    validate_ref(r)?;
    validate_files(&files)?;
    let before: BTreeMap<_, _> = r.manifest.baseline.iter().map(|f| (&f.path, f)).collect();
    let after: BTreeMap<_, _> = files.iter().map(|f| (&f.path, f)).collect();
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).copied().collect();
    let mut changes = vec![];
    for path in paths {
        let kind = match (before.get(path), after.get(path)) {
            (None, Some(_)) => Some(ChangeKind::Added),
            (Some(_), None) => Some(ChangeKind::Deleted),
            (Some(a), Some(b)) if a != b => Some(ChangeKind::Modified),
            _ => None,
        };
        if let Some(kind) = kind {
            changes.push(Change {
                path: path.clone(),
                kind,
                declared_output: r.manifest.spec.outputs.iter().any(|o| &o.path == path),
            });
        }
    }
    Ok(Observation {
        workspace: r.link(),
        source_revision: r.manifest.spec.source_revision.clone(),
        tree_digest: digest(&files)?,
        files,
        clean: changes.is_empty(),
        changes,
    })
}
