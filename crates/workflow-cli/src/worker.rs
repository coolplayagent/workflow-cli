use crate::write;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    sync::atomic::{AtomicU64, Ordering},
};
use workflow_worker::*;

pub const HELP: &str = "CAPABILITIES AND WORKER PROTOCOL\n  workflow capability list\n  workflow capability describe <id> <version>\n  workflow capability invoke <id> <version> <inputs.json>\n  workflow worker prepare <id> <version> <job.json>\n  workflow worker prepare-node <definition.json|yaml> <node-id> <job.json>\n  workflow worker grant <request.json>\n  workflow worker dispatch <request.json> <grant.json>\n  workflow worker check-result <request.json> <grant.json> <result.json>\n  workflow schema <capability|request|grant|result>\n\nOnly built-in read-only capabilities are dispatched. grant is explicit local approval;\nprotect it as host authority. No command creates a run, acquires a lease or commits state.\nExit 0: valid successful result; 1: rejected request or capability failure; 2: usage/I/O.\n";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    request_id: String,
    trace_id: String,
    timeout_ms: u64,
    inputs: Values,
    attempt: Option<Attempt>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    run_id: String,
    node_instance_id: String,
    attempt_id: String,
    lease_epoch: u64,
}

fn read_bytes(path: &str) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    std::fs::File::open(path)
        .and_then(|f| {
            f.take((MAX_MESSAGE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| Error::new(ErrorCode::InvalidMessage, format!("I/O {path}: {e}")))?;
    Ok(bytes)
}
fn read<T: DeserializeOwned>(path: &str) -> Result<T> {
    parse_message(&read_bytes(path)?)
}
fn context(job: &Job, max_timeout_ms: u64) -> Result<RequestContext> {
    if job.timeout_ms == 0 || job.timeout_ms > max_timeout_ms {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "job timeout must be positive and no greater than the capability timeout",
        ));
    }
    let now = SystemClock.now_unix_ms()?;
    Ok(RequestContext {
        request_id: job.request_id.clone(),
        trace_id: job.trace_id.clone(),
        issued_at_unix_ms: now,
        deadline_unix_ms: now.saturating_add(job.timeout_ms),
    })
}
fn json_value<T: serde::Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| Error::new(ErrorCode::InvalidMessage, e.to_string()))
}
fn capability_for<'a>(worker: &'a Worker, request: &WorkRequest) -> Result<&'a Capability> {
    worker.capability(&request.capability.id, &request.capability.version)
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let result = execute(args);
    match result {
        Ok((value, code)) => match to_message(&value) {
            Ok(bytes) => write(
                stdout,
                std::str::from_utf8(&bytes).expect("JSON is UTF-8"),
                code,
            ),
            Err(e) => write(stdout, &json!({"ok":false,"error":e}).to_string(), 1),
        },
        Err(e) if e.message == "usage" => write(stderr, HELP, 2),
        Err(e) => {
            let code = if e.message.starts_with("I/O ") { 2 } else { 1 };
            write(stdout, &json!({"ok":false,"error":e}).to_string(), code)
        }
    }
}
fn execute(args: &[&str]) -> Result<(Value, i32)> {
    if let ["worker", "execute-file", path] = args {
        crate::activity::execute_file(path)?;
        return Ok((json!({"ok":true}), 0));
    }
    let worker = workflow_builtin_capabilities::worker()?;
    match args {
        [
            "schema",
            kind @ ("capability" | "request" | "grant" | "result"),
        ] => Ok((parse_message(schema(kind)?.as_bytes())?, 0)),
        ["capability", "list"] => Ok((
            json!({"protocol_versions":[PROTOCOL_VERSION],"capabilities":worker.capabilities().iter().map(|c| json!({"descriptor":c.descriptor(),"digest":c.digest()})).collect::<Vec<_>>()}),
            0,
        )),
        ["capability", "describe", id, version] => {
            let c = worker.capability(id, version)?;
            Ok((json!({"descriptor":c.descriptor(),"digest":c.digest()}), 0))
        }
        ["capability", "invoke", id, version, file] => {
            let capability = worker.capability(id, version)?;
            let inputs = read(file)?;
            let now = SystemClock.now_unix_ms()?;
            let id = format!(
                "local-{}-{now}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            let request = WorkRequest::standalone(
                capability,
                inputs,
                RequestContext {
                    request_id: id.clone(),
                    trace_id: id,
                    issued_at_unix_ms: now,
                    deadline_unix_ms: now.saturating_add(capability.descriptor().timeout_ms),
                },
            )?;
            let result = worker.execute(&request, &ExecutionGrant::bind(&request)?)?;
            response(result.into_result())
        }
        ["worker", "prepare", id, version, file] => {
            let job: Job = read(file)?;
            if job.attempt.is_some() {
                return Err(Error::new(
                    ErrorCode::InvalidRequest,
                    "standalone job must not claim a workflow attempt",
                ));
            }
            let c = worker.capability(id, version)?;
            let context = context(&job, c.descriptor().timeout_ms)?;
            Ok((
                json_value(&WorkRequest::standalone(c, job.inputs, context)?)?,
                0,
            ))
        }
        ["worker", "prepare-node", file, node_id, job] => {
            let workflow = crate::load(file).map_err(|e| {
                let message = if e.code == "io_error" {
                    format!("I/O {file}: {}", e.message)
                } else {
                    e.message
                };
                Error::new(ErrorCode::InvalidBinding, message)
            })?;
            let job: Job = read(job)?;
            let node = workflow
                .nodes
                .iter()
                .find(|n| n.id == *node_id)
                .ok_or_else(|| Error::new(ErrorCode::InvalidBinding, "unknown node"))?;
            let workflow_ir::NodeKind::Task { capability, .. } = &node.kind else {
                return Err(Error::new(ErrorCode::InvalidBinding, "node must be a task"));
            };
            let c = worker.capability(&capability.id, &capability.version)?;
            let context = context(&job, c.descriptor().timeout_ms)?;
            let attempt = job.attempt.ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidRequest,
                    "node job requires an explicit host-owned attempt",
                )
            })?;
            let request = WorkRequest::for_node(
                &workflow,
                node_id,
                c,
                job.inputs,
                context,
                NodeAttempt {
                    run_id: attempt.run_id,
                    node_instance_id: attempt.node_instance_id,
                    attempt_id: attempt.attempt_id,
                    lease_epoch: attempt.lease_epoch,
                },
            )?;
            Ok((json_value(&request)?, 0))
        }
        ["worker", "grant", file] => {
            let request: WorkRequest = read(file)?;
            let c = capability_for(&worker, &request)?;
            let grant = ExecutionGrant::bind(&request)?;
            validate_request(&request, &grant, c, SystemClock.now_unix_ms()?)?;
            Ok((json_value(&grant)?, 0))
        }
        ["worker", "dispatch", request, grant] => {
            let request = read_bytes(request)?;
            let grant = read(grant)?;
            response(parse_message(&worker.dispatch_json(&request, &grant)?)?)
        }
        ["worker", "check-result", request, grant, result] => {
            let request: WorkRequest = read(request)?;
            let grant = read(grant)?;
            let result = read(result)?;
            response(
                accept_result(
                    &request,
                    &grant,
                    capability_for(&worker, &request)?,
                    result,
                    SystemClock.now_unix_ms()?,
                )?
                .into_result(),
            )
        }
        _ => Err(Error::new(ErrorCode::InvalidRequest, "usage")),
    }
}
fn response(result: WorkResult) -> Result<(Value, i32)> {
    let code = if matches!(result.outcome, AdapterOutcome::Succeeded { .. }) {
        0
    } else {
        1
    };
    Ok((json_value(&result)?, code))
}

#[cfg(test)]
mod tests;
