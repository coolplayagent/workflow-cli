use crate::write;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::io::{Read, Write};
use workflow_workspace_local::{GitSource, LocalWorkspaceStore};
use workflow_workspaces::*;
pub const HELP: &str = "ATTEMPT WORKSPACES\n  workflow workspace init <store>\n  workflow workspace prepare <request.json> <source.json> <input-refs.json> <outputs.json>\n  workflow workspace checkout <store> <repository-id> <repository-path> <spec.json>\n  workflow workspace show <store> <workspace-id>\n  workflow workspace path <store> <workspace-id>\n  workflow workspace observe <store> <workspace-id>\n  workflow workspace verify-clean <store> <workspace-id>\n  workflow workspace capture <store> <workspace-id> <artifact-store>\n  workflow workspace cleanup-orphans <store>\n  workflow schema <workspace-checkout|workspace-ref|workspace-observation|workspace-output>\n\nOnly init creates a store. Linux adapter requires Git and /proc; source commits are read without checkout hooks/filters.\nOne physical file copy per attempt; an exact retry never resets edited work. No process sandbox or automatic merge.\nprepare binds a supplied workflow request; the host separately verifies its actual lease/attempt authority.\nOutputs are exact paths and artifact types. capture retains output artifacts plus a manifest for the observed tree.\nobserve exits 0 when inspected (read clean/changes); verify-clean exits 1 for a dirty tree. Neither advances a run or grants an effect.\n";
fn invalid(m: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidContract, m)
}
fn read<T: DeserializeOwned>(file: &str) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(file)
        .and_then(|f| f.take(2_097_153).read_to_end(&mut bytes))
        .map_err(|e| invalid(format!("I/O: {e}")))?;
    parse_message(&bytes).map_err(Into::into)
}
fn report(v: impl Serialize) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| invalid(e.to_string()))
}
fn execute(args: &[&str]) -> Result<Value> {
    match args {
        [
            "schema",
            kind @ ("workspace-checkout"
            | "workspace-ref"
            | "workspace-observation"
            | "workspace-output"),
        ] => parse_message(schema(&kind[10..])?.as_bytes()).map_err(Into::into),
        ["workspace", "init", root] => {
            LocalWorkspaceStore::create(root)?;
            Ok(json!({"initialized":true}))
        }
        ["workspace", "prepare", request, source, inputs, outputs] => {
            let request: workflow_worker::WorkRequest = read(request)?;
            let spec = CheckoutSpec {
                schema_version: 1,
                producer: workflow_runstore::artifact_producer(&request)
                    .map_err(|e| invalid(e.message))?,
                source_revision: read(source)?,
                merge_policy: MergePolicy::Explicit,
                inputs: read(inputs)?,
                outputs: read(outputs)?,
            };
            validate_spec(&spec)?;
            report(spec)
        }
        ["workspace", "checkout", root, repository, path, file] => report(
            LocalWorkspaceStore::open(root)?
                .checkout(&read(file)?, &GitSource::open(repository, path)?)?,
        ),
        [
            "workspace",
            op @ ("show" | "path" | "observe" | "verify-clean"),
            root,
            id,
        ] => {
            let store = LocalWorkspaceStore::open(root)?;
            let r = store.find(id)?;
            match *op {
                "show" => report(r),
                "path" => Ok(json!({"workspace":r.link(),"path":store.path(&r.link())?})),
                _ => report(store.observe(&r.link())?),
            }
        }
        ["workspace", "capture", root, id, artifacts] => {
            let mut store = LocalWorkspaceStore::open(root)?;
            let r = store.find(id)?;
            report(store.capture(
                &r.link(),
                &mut workflow_artifact_local::LocalArtifactStore::open(artifacts)?,
            )?)
        }
        ["workspace", "cleanup-orphans", root] => {
            report(LocalWorkspaceStore::open(root)?.cleanup_orphans()?)
        }
        _ => Err(invalid("usage")),
    }
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    match execute(args) {
        Ok(v) => {
            let code = if matches!(args, ["workspace", "verify-clean", ..]) && v["clean"] != true {
                1
            } else {
                0
            };
            match to_message(&json!({"ok":true,"result":v})) {
                Ok(bytes) => write(stdout, std::str::from_utf8(&bytes).unwrap(), code),
                Err(e) => write(
                    stdout,
                    &json!({"ok":false,"error":e,"commit_may_have_succeeded":true}).to_string(),
                    1,
                ),
            }
        }
        Err(e) if e.message == "usage" => write(stderr, HELP, 2),
        Err(e) => write(
            stdout,
            &json!({"ok":false,"error":e}).to_string(),
            if e.message.starts_with("I/O:") { 2 } else { 1 },
        ),
    }
}
#[cfg(test)]
mod tests;
