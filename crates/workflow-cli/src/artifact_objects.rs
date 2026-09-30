use serde::Deserialize;
use std::path::PathBuf;
use workflow_artifact_s3::{S3ArtifactStore, S3Binding, S3Client};
use workflow_artifacts::*;
use workflow_service::{DatabaseBinding, SecretRef};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    database: DatabaseBinding,
    namespace: String,
    object: S3Binding,
    access_key: SecretRef,
    secret_key: SecretRef,
    #[serde(default)]
    session_token: Option<SecretRef>,
    #[serde(default)]
    ca_file: Option<PathBuf>,
}
fn host(error: workflow_service::Error) -> Error {
    Error::new(ErrorCode::Storage, error.to_string())
}
pub(crate) fn open(path: &str, create: bool) -> Result<S3ArtifactStore> {
    let binding: Binding = parse_message(
        &workflow_service::read_bounded(std::path::Path::new(path), MAX_JSON_BYTES)
            .map_err(host)?,
    )?;
    let ca = binding
        .ca_file
        .as_deref()
        .map(|p| workflow_service::read_bounded(p, 1_048_576))
        .transpose()
        .map_err(host)?;
    let objects = S3Client::new(
        binding.object,
        binding.access_key.resolve().map_err(host)?.expose().into(),
        binding.secret_key.resolve().map_err(host)?.expose().into(),
        binding
            .session_token
            .as_ref()
            .map(|s| s.resolve().map(|s| s.expose().to_owned()))
            .transpose()
            .map_err(host)?,
        ca.as_deref(),
    )?;
    let connection = binding.database.connect().map_err(host)?;
    if create {
        S3ArtifactStore::create(connection, objects, &binding.namespace)
    } else {
        S3ArtifactStore::open(connection, objects, &binding.namespace)
    }
}
pub(crate) fn execute(args: &[&str]) -> Option<Result<serde_json::Value>> {
    use workflow_artifact_local::{LocalArtifactStore, link_for_id};
    let result = match args {
        ["artifact", "s3-init", binding] => {
            open(binding, true).map(|_| serde_json::json!({"initialized":true}))
        }
        ["artifact", "s3-import", binding, local, id] => (|| {
            let source = LocalArtifactStore::open(local)?;
            let mut target = open(binding, false)?;
            let graph = source.lineage(&link_for_id(id)?)?;
            for reference in &graph {
                if target.publish(
                    &reference.manifest.spec,
                    &mut source.read(&reference.link())?.as_slice(),
                )? != *reference
                {
                    return Err(Error::new(
                        ErrorCode::CorruptStorage,
                        "object import changed identity",
                    ));
                }
            }
            Ok(
                serde_json::json!({"artifact":target.verify(&link_for_id(id)?)?,"imported":graph.len()}),
            )
        })(),
        ["artifact", "s3-export", binding, id, local] => (|| {
            let source = open(binding, false)?;
            let mut target = LocalArtifactStore::open(local)?;
            let graph = source.lineage(&link_for_id(id)?)?;
            for reference in &graph {
                if target.publish(
                    &reference.manifest.spec,
                    &mut source.read(&reference.link())?.as_slice(),
                )? != *reference
                {
                    return Err(Error::new(
                        ErrorCode::CorruptStorage,
                        "local import changed identity",
                    ));
                }
            }
            Ok(
                serde_json::json!({"artifact":target.verify(&link_for_id(id)?)?,"imported":graph.len()}),
            )
        })(),
        ["artifact", "s3-show", binding, id] => open(binding, false)
            .and_then(|s| s.verify(&link_for_id(id)?))
            .and_then(|r| {
                serde_json::to_value(r)
                    .map_err(|_| Error::new(ErrorCode::InvalidDocument, "serialization failed"))
            }),
        [
            "artifact",
            "s3-download-grant",
            binding,
            id,
            ttl,
            destination,
        ] => (|| {
            let ttl: u32 = ttl.parse().map_err(|_| {
                Error::new(
                    ErrorCode::InvalidContract,
                    "download lifetime must be 1..300 seconds",
                )
            })?;
            let grant = open(binding, false)?.grant_download(&link_for_id(id)?, ttl)?;
            workflow_service::write_private_output(
                std::path::Path::new(destination),
                &to_message(&grant)?,
            )
            .map_err(host)?;
            Ok(
                serde_json::json!({"artifact":grant.artifact,"expires_at_unix_ms":grant.expires_at_unix_ms}),
            )
        })(),
        ["artifact", "s3-cleanup-orphans", binding, after, limit] => (|| {
            let limit: u32 = limit
                .parse()
                .map_err(|_| Error::new(ErrorCode::InvalidContract, "limit must be 1..100"))?;
            serde_json::to_value(
                open(binding, false)?.cleanup_orphans((*after != "-").then_some(*after), limit)?,
            )
            .map_err(|_| Error::new(ErrorCode::InvalidDocument, "serialization failed"))
        })(),
        _ => return None,
    };
    Some(result)
}
