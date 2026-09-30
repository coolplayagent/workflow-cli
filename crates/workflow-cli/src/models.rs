use crate::write;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    io::{Read, Write},
    sync::Arc,
};
use workflow_model_http::{HttpBinding, HttpModel};
use workflow_models::*;
use workflow_worker::{ExecutionGrant, WorkRequest, WorkResult, Worker};
pub const HELP: &str = "MODEL POLICY EXECUTION\n  workflow model check-policy <policy.json>\n  workflow model describe-binding <http-binding.json>\n  workflow model dispatch <policy.json> <http-binding.json> <request.json> <grant.json>\n  workflow model check-record <policy.json> <request.json> <result.json>\n  workflow run drive-models <db> <run-id> <owner> <max-commands> <bindings.json>\n  workflow schema <model-policy|model-proposal|model-record|model-http-binding>\n\nModel policies are frozen in the run bundle; provider/model/endpoints remain host bindings.\nCredentials are resolved on each call from a short-lived private lease; local execution also supports named environment references.\nOnly listed read-only tools can be invoked. Models cannot write run state or skip gates.\ncheck-record replays explicit records without model/network calls; it does not authenticate a provider or grant.\n";
fn read<T: DeserializeOwned>(path: &str) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(path)
        .and_then(|f| f.take(2_097_153).read_to_end(&mut bytes))
        .map_err(|_| Error::new(ErrorCode::InvalidMessage, "model input file I/O failed"))?;
    parse_message(&bytes)
}
#[derive(Clone, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    policy: workflow_worker::ModelPolicyBinding,
    http: HttpBinding,
}
pub fn worker(bundle: &workflow_kernel::BundleSpec, path: &str) -> Result<Worker> {
    bound_worker(bundle, &bindings(path)?, false)
}
pub(crate) fn shared_worker(
    bundle: &workflow_kernel::BundleSpec,
    path: &str,
) -> Result<(Worker, Option<workflow_credentials::Principal>)> {
    let bindings = bindings(path)?;
    let mut principal = None;
    for b in &bindings {
        if let Some(p) = b.http.shared_principal()? {
            if principal.as_ref().is_some_and(|old| old != p) {
                return Err(Error::new(
                    ErrorCode::InvalidBinding,
                    "shared provider identities differ",
                ));
            }
            principal = Some(p.clone());
        }
    }
    Ok((bound_worker(bundle, &bindings, false)?, principal))
}
pub(crate) fn bindings(path: &str) -> Result<Vec<Binding>> {
    let bindings: Vec<Binding> = read(path)?;
    if bindings.len() > 128 {
        return Err(Error::new(
            ErrorCode::InvalidBinding,
            "too many model bindings",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for b in &bindings {
        b.policy.validate()?;
        if !seen.insert(b.policy.digest.clone()) {
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "duplicate model binding",
            ));
        }
        HttpModel::new(b.http.clone())?;
    }
    Ok(bindings)
}
pub(crate) fn bound_worker(
    bundle: &workflow_kernel::BundleSpec,
    bindings: &[Binding],
    allow_unused: bool,
) -> Result<Worker> {
    let mut seen = std::collections::BTreeSet::new();
    let mut worker = workflow_builtin_capabilities::worker()?;
    for b in bindings {
        let p = bundle
            .model_policies
            .iter()
            .find(|p| p.policy == b.policy.policy);
        let Some(p) = p else {
            if allow_unused {
                continue;
            }
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "binding policy absent from run bundle",
            ));
        };
        let policy = Policy::new(p.clone())?;
        if policy.binding() != &b.policy {
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "host binding policy digest differs from frozen run",
            ));
        }
        seen.insert(b.policy.digest.clone());
        worker.register(ModelCapability::new(
            policy,
            Arc::new(HttpModel::new(b.http.clone())?),
            workflow_builtin_capabilities::worker()?,
        )?)?;
    }
    for p in &bundle.model_policies {
        if !seen.contains(&Policy::new(p.clone())?.binding().digest) {
            return Err(Error::new(
                ErrorCode::MissingCapability,
                "model policy has no host binding",
            ));
        }
    }
    Ok(worker)
}
fn execute(args: &[&str]) -> Result<(serde_json::Value, i32)> {
    let value = match args {
        ["schema", "model-http-binding"] => {
            serde_json::to_value(schemars::schema_for!(HttpBinding))
                .map_err(|_| Error::new(ErrorCode::InvalidMessage, "schema encoding failed"))?
        }
        [
            "schema",
            kind @ ("model-policy" | "model-proposal" | "model-record"),
        ] => parse_message(schema(&kind[6..])?.as_bytes())?,
        ["model", "check-policy", file] => {
            let p = Policy::new(read(file)?)?;
            let task = workflow_worker::Capability::new(p.spec().task.clone())?;
            json!({"ok":true,"result":{"protocol_versions":[2],"binding":p.binding(),"task_contract_digest":task.digest(),"policy":p.spec()}})
        }
        ["model", "describe-binding", file] => {
            json!({"ok":true,"result":HttpModel::new(read(file)?)?.identity()})
        }
        ["model", "dispatch", policy, binding, request, grant] => {
            let p = Policy::new(read(policy)?)?;
            let mut worker = Worker::default();
            worker.register(ModelCapability::new(
                p,
                Arc::new(HttpModel::new(read(binding)?)?),
                workflow_builtin_capabilities::worker()?,
            )?)?;
            let r: WorkRequest = read(request)?;
            let result = worker
                .execute(&r, &read::<ExecutionGrant>(grant)?)?
                .into_result();
            let code = if matches!(
                result.outcome,
                workflow_worker::AdapterOutcome::Succeeded { .. }
            ) {
                0
            } else {
                1
            };
            return Ok((
                serde_json::to_value(result).map_err(|_| {
                    Error::new(ErrorCode::InvalidMessage, "model result encoding failed")
                })?,
                code,
            ));
        }
        ["model", "check-record", policy, request, result] => {
            let p = Policy::new(read(policy)?)?;
            let request: WorkRequest = read(request)?;
            let result: WorkResult = read(result)?;
            verify_result(&p, &request, &result)?;
            json!({"ok":true,"result":{"verified":true,"request_digest":result.request_digest}})
        }
        _ => return Err(Error::new(ErrorCode::InvalidRequest, "usage")),
    };
    Ok((value, 0))
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    match execute(args) {
        Ok((value, code)) => match to_message(&value) {
            Ok(bytes) => write(stdout, std::str::from_utf8(&bytes).unwrap(), code),
            Err(e) => write(stdout, &json!({"ok":false,"error":e}).to_string(), 1),
        },
        Err(e) if e.message == "usage" => write(stderr, HELP, 2),
        Err(e) => write(stdout, &json!({"ok":false,"error":e}).to_string(), 1),
    }
}

#[cfg(test)]
mod tests;
