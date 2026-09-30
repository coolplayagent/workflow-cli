use crate::write;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{io::Write, path::Path, time::Duration};
use workflow_runstore_postgres::access::{
    ArtifactUploadRequest, AuthenticatedService, CapabilityRule, Role,
};
use workflow_service::*;
pub const HELP: &str = "REMOTE SERVICE\n  workflow service serve <server-binding.json>\n  workflow service fence-restored <isolated-server-binding.json> <restore-request.json> <private-administrator-output>\n  workflow service init-artifacts <server-binding.json>\n  workflow service init-effects <server-binding.json>\n  workflow service migrate-access <server-binding.json>\n  workflow service bootstrap <server-binding.json> <tenant> <project> <actor> <private-token-output>\n  workflow service issue <server-binding.json> <admin-secret-ref.json> <provision.json> <private-token-output>\n  workflow remote call <client-binding.json> <request.json>\n  workflow remote validate <client-binding.json> <file.json|file.yaml>\n  workflow remote artifact-upload <client-binding.json> <assignment-id> <request-id> <artifact-type.json> <content-file>\n  workflow remote artifact-download <client-binding.json> <download-request.json> <private-output>\n  workflow remote work <client-binding.json> <iterations> <poll-ms>\n  workflow remote work-models <client-binding.json> <bundle.json> <model-bindings.json> <iterations> <poll-ms>\n  workflow remote work-effects <client-binding.json> <effect-bindings.json> <iterations> <poll-ms>\n  workflow remote schedule <client-binding.json> <scheduler.json> <iterations> <poll-ms>\n\nTLS and a scoped bearer are required. Bindings contain secret references, never literal tokens.\nCredential creation is a trusted local administrative operation with exclusive 0600 output.\nwork executes registered builtin read-only capabilities; work-models adds frozen model policies using host provider bindings; work-effects also invokes explicitly bound gateways. Scheduler effects require effects:true. Iterations are bounded; service managers may supervise commands.\n";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provision {
    actor: String,
    role: Role,
    capabilities: Vec<CapabilityRule>,
    ttl_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    #[serde(default)]
    effects: bool,
    instance_id: String,
    worker_ids: Vec<String>,
    lease_ms: u64,
    scan_limit: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Download {
    artifact: workflow_artifacts::ArtifactLink,
    assignment_id: Option<String>,
    ttl_ms: u64,
}
fn read<T: DeserializeOwned>(path: &str) -> Result<T> {
    serde_json::from_slice(&read_bounded(Path::new(path), MAX_REQUEST_BYTES)?)
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "invalid remote input file"))
}
fn bounds(iterations: &str, poll: &str) -> Result<(u32, Duration)> {
    let iterations: u32 = iterations
        .parse()
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "invalid iteration count"))?;
    let poll: u64 = poll
        .parse()
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "invalid polling period"))?;
    if !(1..=100000).contains(&iterations) || !(20..=60000).contains(&poll) {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "iterations 1..100000 and poll 20..60000 ms required",
        ));
    }
    Ok((iterations, Duration::from_millis(poll)))
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    if let ["remote", "validate", binding, file] = args {
        let (source, format) = match crate::load_source(file) {
            Ok(source) => source,
            Err(diagnostic) => {
                return crate::validation_output(
                    &workflow_validator::ValidationReport::rejected(*diagnostic),
                    stdout,
                    stderr,
                );
            }
        };
        let result = (|| -> Result<Response> {
            RemoteClient::new(read(binding)?)?.call(&Request {
                protocol_version: PROTOCOL_VERSION,
                request_id: "validate-definition".into(),
                operation: Operation::ValidateDefinition {
                    source,
                    format,
                    file: (*file).into(),
                },
            })
        })();
        return match result {
            Ok(Response::Validation(report)) => crate::validation_output(&report, stdout, stderr),
            _ => write(stderr, "remote validation could not be confirmed", 2),
        };
    }
    let result = (|| -> Result<serde_json::Value> {
        match args {
            ["service", "fence-restored", binding, request, output] => {
                if Path::new(output).symlink_metadata().is_ok() {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "private restore output must not exist",
                    ));
                }
                let binding: ServerBinding = read(binding)?;
                let request =
                    read::<workflow_runstore_postgres::access::DatabaseRestoreRequest>(request)?;
                let restored = AuthenticatedService::fence_restored_database(
                    &mut binding.database.connect()?,
                    &request,
                )?;
                let administrators: Vec<_> = restored.administrators.iter().map(|a| json!({
                    "tenant":a.tenant, "project":a.project, "credential_id":a.credential.id,
                    "expires_at_unix_ms":a.credential.expires_at_unix_ms, "secret":a.credential.expose_secret()
                })).collect();
                let private = serde_json::to_vec(
                    &json!({"report":restored.report,"administrators":administrators}),
                )
                .map_err(|_| {
                    Error::new(ErrorCode::Storage, "restore delivery serialization failed")
                })?;
                write_private_output(Path::new(output), &private)?;
                serde_json::to_value(restored.report).map_err(|_| {
                    Error::new(ErrorCode::Storage, "restore report serialization failed")
                })
            }
            [
                "remote",
                "work-models",
                binding,
                bundle,
                models,
                iterations,
                poll,
            ] => {
                let (iterations, poll) = bounds(iterations, poll)?;
                let client = RemoteClient::new(read(binding)?)?;
                let bundle: workflow_kernel::BundleSpec = read(bundle)?;
                workflow_kernel::CompiledBundle::compile(bundle.clone())?;
                let worker = crate::models::worker(&bundle, models)?;
                let mut completed = 0;
                let mut failed = 0;
                let mut fenced = 0;
                for i in 0..iterations {
                    let report = work_once(&client, &worker, 100)?;
                    completed += report.completed;
                    failed += report.failed;
                    fenced += report.fenced;
                    if i + 1 < iterations {
                        std::thread::sleep(poll);
                    }
                }
                Ok(json!({"settled_tasks":completed,"rejected_tasks":failed,"fenced":fenced}))
            }
            ["service", "migrate-access", path] => {
                let binding: ServerBinding = read(path)?;
                AuthenticatedService::migrate_access(&mut binding.database.connect()?)?;
                Ok(json!({"access_schema":2,"migrated":true}))
            }
            ["service", "init-effects", path] => {
                let binding: ServerBinding = read(path)?;
                AuthenticatedService::initialize_effects(&mut binding.database.connect()?)?;
                Ok(json!({"effect_dispatch_schema":1,"initialized":true}))
            }
            ["remote", "work-effects", binding, effects, iterations, poll] => {
                let (iterations, poll) = bounds(iterations, poll)?;
                let client = RemoteClient::new(read(binding)?)?;
                let effects = workflow_effect_http::HttpEffects::new(read(effects)?)?;
                let worker = workflow_builtin_capabilities::worker()?;
                let mut observed = 0;
                let mut failed = 0;
                let mut completed = 0;
                let mut fenced = 0;
                for i in 0..iterations {
                    let effects = work_effects_once(&client, &effects, 100)?;
                    let tasks = work_once(&client, &worker, 100)?;
                    observed += effects.completed;
                    completed += tasks.completed;
                    failed += tasks.failed;
                    fenced += effects.fenced + tasks.fenced;
                    if i + 1 < iterations {
                        std::thread::sleep(poll);
                    }
                }
                Ok(
                    json!({"observed_effect_calls":observed,"completed_tasks":completed,"failed_tasks":failed,"fenced":fenced}),
                )
            }
            ["service", "init-artifacts", path] => {
                let binding: ServerBinding = read(path)?;
                AuthenticatedService::initialize_artifacts(&mut binding.database.connect()?)?;
                Ok(json!({"artifact_schema":1,"initialized":true}))
            }
            [
                "remote",
                "artifact-upload",
                binding,
                assignment,
                request,
                artifact_type,
                file,
            ] => {
                let content = read_bounded(
                    Path::new(file),
                    workflow_artifacts::MAX_CONTENT_BYTES as usize,
                )?;
                let upload = ArtifactUploadRequest {
                    request_id: (*request).into(),
                    assignment_id: (*assignment).into(),
                    artifact_type: read(artifact_type)?,
                    bytes: content.len() as u64,
                    content_digest: workflow_artifacts::content_digest(&content),
                };
                let reference =
                    RemoteClient::new(read(binding)?)?.upload_artifact(&upload, &content)?;
                serde_json::to_value(reference)
                    .map_err(|_| Error::new(ErrorCode::InvalidRequest, "artifact reply invalid"))
            }
            ["remote", "artifact-download", binding, request, output] => {
                if Path::new(output).symlink_metadata().is_ok() {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "artifact output must not exist",
                    ));
                }
                let request: Download = read(request)?;
                let (reference, content) = RemoteClient::new(read(binding)?)?.download_artifact(
                    &request.artifact,
                    request.assignment_id.as_deref(),
                    request.ttl_ms,
                )?;
                write_private_output(Path::new(output), &content)?;
                serde_json::to_value(reference)
                    .map_err(|_| Error::new(ErrorCode::InvalidRequest, "artifact reply invalid"))
            }
            ["service", "serve", path] => {
                serve_foreground(read(path)?, |address| {
                    writeln!(stdout, "{}", json!({"bound_address":address.to_string()}))
                        .and_then(|_| stdout.flush())
                        .map_err(|_| Error::new(ErrorCode::Storage, "service output failed"))
                })?;
                Ok(json!({"stopped":true}))
            }
            ["service", "bootstrap", path, tenant, project, actor, output] => {
                if Path::new(output).symlink_metadata().is_ok() {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "credential output must not exist",
                    ));
                }
                let binding: ServerBinding = read(path)?;
                let credential = AuthenticatedService::bootstrap(
                    &mut binding.database.connect()?,
                    tenant,
                    project,
                    actor,
                    3600000,
                )?;
                write_credential(Path::new(output), &credential)?;
                Ok(
                    json!({"credential_id":credential.id,"expires_at_unix_ms":credential.expires_at_unix_ms}),
                )
            }
            ["service", "issue", path, admin, provision, output] => {
                if Path::new(output).symlink_metadata().is_ok() {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "credential output must not exist",
                    ));
                }
                let binding: ServerBinding = read(path)?;
                let admin: SecretRef = read(admin)?;
                let request: Provision = read(provision)?;
                let mut service = AuthenticatedService::open(binding.database.connect()?)?;
                let credential = service.issue(
                    admin.resolve()?.expose(),
                    &request.actor,
                    request.role,
                    &request.capabilities,
                    request.ttl_ms,
                )?;
                write_credential(Path::new(output), &credential)?;
                Ok(
                    json!({"credential_id":credential.id,"expires_at_unix_ms":credential.expires_at_unix_ms}),
                )
            }
            ["remote", "call", binding, request] => {
                serde_json::to_value(RemoteClient::new(read(binding)?)?.call(&read(request)?)?)
                    .map_err(|_| {
                        Error::new(
                            ErrorCode::InvalidRequest,
                            "remote reply serialization failed",
                        )
                    })
            }
            ["remote", "work", binding, iterations, poll] => {
                let (iterations, poll) = bounds(iterations, poll)?;
                let client = RemoteClient::new(read(binding)?)?;
                let worker = workflow_builtin_capabilities::worker()?;
                let mut completed = 0;
                let mut fenced = 0;
                let mut failed = 0;
                for i in 0..iterations {
                    let report = work_once(&client, &worker, 100)?;
                    completed += report.completed;
                    fenced += report.fenced;
                    failed += report.failed;
                    if i + 1 < iterations {
                        std::thread::sleep(poll);
                    }
                }
                Ok(json!({"completed":completed,"fenced":fenced,"failed":failed}))
            }
            ["remote", "schedule", binding, config, iterations, poll] => {
                let (iterations, poll) = bounds(iterations, poll)?;
                let client = RemoteClient::new(read(binding)?)?;
                let config: Schedule = read(config)?;
                let mut scheduler = Scheduler::new(&config.instance_id, config.worker_ids)?;
                if config.effects {
                    scheduler = scheduler.with_effects();
                }
                let mut dispatched = 0;
                let mut effect_waiting = 0;
                let mut effect_manual = 0;
                let mut busy = 0;
                let mut fenced = 0;
                for i in 0..iterations {
                    let report = scheduler.step(&client, config.lease_ms, config.scan_limit)?;
                    dispatched += report.dispatched;
                    effect_waiting += report.effect_waiting;
                    effect_manual += report.effect_manual;
                    busy += report.busy;
                    fenced += report.fenced;
                    if i + 1 < iterations {
                        std::thread::sleep(poll);
                    }
                }
                Ok(
                    json!({"dispatched":dispatched,"busy":busy,"fenced":fenced,"effect_waiting":effect_waiting,"effect_manual":effect_manual}),
                )
            }
            _ => Err(Error::new(ErrorCode::InvalidRequest, HELP)),
        }
    })();
    match result {
        Ok(value)=>write(stdout,&value.to_string(),0),
        Err(error)=>write(stderr,&json!({"code":error.code,"message":"remote command failed; no durable success confirmed"}).to_string(),2),
    }
}
