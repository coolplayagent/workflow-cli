use crate::write;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{io::Write, path::Path, time::Duration};
use workflow_runstore_postgres::access::{AuthenticatedService, CapabilityRule, Role};
use workflow_service::*;
pub const HELP: &str = "REMOTE SERVICE\n  workflow service serve <server-binding.json>\n  workflow service bootstrap <server-binding.json> <tenant> <project> <actor> <private-token-output>\n  workflow service issue <server-binding.json> <admin-secret-ref.json> <provision.json> <private-token-output>\n  workflow remote call <client-binding.json> <request.json>\n  workflow remote work <client-binding.json> <iterations> <poll-ms>\n  workflow remote schedule <client-binding.json> <scheduler.json> <iterations> <poll-ms>\n\nTLS and a scoped bearer are required. Bindings contain secret references, never literal tokens.\nCredential creation is a trusted local administrative operation with exclusive 0600 output.\nwork executes registered builtin read-only capabilities. Iterations are bounded; service managers may supervise commands.\n";
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
    instance_id: String,
    worker_ids: Vec<String>,
    lease_ms: u64,
    scan_limit: u32,
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
    let result = (|| -> Result<serde_json::Value> {
        match args {
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
                let mut dispatched = 0;
                let mut busy = 0;
                let mut fenced = 0;
                for i in 0..iterations {
                    let report = scheduler.step(&client, config.lease_ms, config.scan_limit)?;
                    dispatched += report.dispatched;
                    busy += report.busy;
                    fenced += report.fenced;
                    if i + 1 < iterations {
                        std::thread::sleep(poll);
                    }
                }
                Ok(json!({"dispatched":dispatched,"busy":busy,"fenced":fenced}))
            }
            _ => Err(Error::new(ErrorCode::InvalidRequest, HELP)),
        }
    })();
    match result {
        Ok(value)=>write(stdout,&value.to_string(),0),
        Err(error)=>write(stderr,&json!({"code":error.code,"message":"remote command failed; no durable success confirmed"}).to_string(),2),
    }
}
