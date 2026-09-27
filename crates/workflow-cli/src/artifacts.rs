use crate::write;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
};
use workflow_artifact_local::{LocalArtifactStore, link_for_id};
use workflow_artifacts::*;
pub const HELP: &str = "TYPED ARTIFACTS\n  workflow artifact init <store>\n  workflow artifact prepare <request.json> <type.json> <source.json> <input-refs.json>\n  workflow artifact put <store> <publish.json> <payload>\n  workflow artifact show <store> <artifact-id>\n  workflow artifact verify <store> <artifact-id> <expected-type.json>\n  workflow artifact lineage <store> <artifact-id> <after-id|-> <limit>\n  workflow artifact impact <store> <artifact-id> <after-id|-> <limit>\n  workflow artifact export <store> <artifact-id> <new-payload-path>\n  workflow artifact import <store> <reference.json> <payload>\n  workflow artifact cleanup-orphans <store>\n  workflow schema <artifact-publish|artifact-ref|artifact-type>\n\nOnly init creates storage. put/import acknowledge after durable content and manifest commit.\nReferences use portable artifact:// identities; provenance and scopes are trusted host claims.\nprepare binds the supplied workflow request; finish checks the actual durable attempt.\nCommitted artifacts are retained; cleanup removes only uncommitted objects/uploads.\nStructured JSON is limited to 2 MiB; text/binary to 64 MiB. No workspace isolation or remote authorization is implied.\n";
fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidDocument, message)
}
fn io(e: std::io::Error) -> Error {
    Error::new(ErrorCode::Storage, format!("I/O: {e}"))
}
fn read<T: DeserializeOwned>(path: &str) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(path)
        .map_err(io)?
        .take(MAX_JSON_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    parse_message(&bytes)
}
fn report(v: impl Serialize) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| invalid(e.to_string()))
}
fn execute(args: &[&str]) -> Result<Value> {
    match args {
        [
            "schema",
            kind @ ("artifact-publish" | "artifact-ref" | "artifact-type"),
        ] => parse_message(schema(&kind[9..])?.as_bytes()),
        ["artifact", "init", root] => {
            LocalArtifactStore::create(root)?;
            Ok(json!({"initialized":true}))
        }
        ["artifact", "prepare", request, ty, source, inputs] => {
            let request: workflow_worker::WorkRequest = read(request)?;
            let producer =
                workflow_runstore::artifact_producer(&request).map_err(|e| invalid(e.message))?;
            let spec = PublishSpec {
                schema_version: 1,
                artifact_type: read(ty)?,
                source_revision: read(source)?,
                inputs: read(inputs)?,
                access: AccessScope::Run {
                    run_id: producer.run_id.clone(),
                },
                producer,
                retention: Retention::RunDependency,
            };
            validate_spec(&spec)?;
            report(spec)
        }
        ["artifact", "put", root, spec, file] => report(
            LocalArtifactStore::open(root)?
                .publish(&read(spec)?, &mut std::fs::File::open(file).map_err(io)?)?,
        ),
        ["artifact", "show", root, id] => report(LocalArtifactStore::open(root)?.resolve(id)?),
        ["artifact", "verify", root, id, expected] => report(verify_expected(
            &LocalArtifactStore::open(root)?,
            &link_for_id(id)?,
            &read(expected)?,
        )?),
        [
            "artifact",
            mode @ ("lineage" | "impact"),
            root,
            id,
            after,
            limit,
        ] => {
            let limit: usize = limit.parse().map_err(|_| invalid("limit must be 1..100"))?;
            if !(1..=100).contains(&limit) {
                return Err(invalid("limit must be 1..100"));
            }
            let store = LocalArtifactStore::open(root)?;
            let all = if *mode == "lineage" {
                store.lineage(&link_for_id(id)?)?
            } else {
                store.impact(&link_for_id(id)?)?
            };
            let start = if *after == "-" {
                0
            } else {
                all.iter()
                    .position(|r| r.artifact_id == *after)
                    .ok_or_else(|| invalid("cursor is not in this lineage"))?
                    + 1
            };
            let items: Vec<_> = all.iter().skip(start).take(limit).collect();
            let next = if start + items.len() < all.len() {
                items.last().map(|r| r.artifact_id.clone())
            } else {
                None
            };
            Ok(json!({"items":items,"next_cursor":next}))
        }
        ["artifact", "export", root, id, target] => {
            let store = LocalArtifactStore::open(root)?;
            let r = store.resolve(id)?;
            let bytes = store.read(&r.link())?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(target).map_err(io)?;
            let saved = (|| {
                f.write_all(&bytes).map_err(io)?;
                f.sync_all().map_err(io)?;
                let parent = Path::new(target)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                std::fs::File::open(parent)
                    .map_err(io)?
                    .sync_all()
                    .map_err(io)
            })();
            if saved.is_err() {
                let _ = std::fs::remove_file(target);
            }
            saved?;
            report(r)
        }
        ["artifact", "import", root, reference, file] => {
            let expected: ArtifactRef = read(reference)?;
            validate_ref(&expected)?;
            let mut bytes = vec![];
            std::fs::File::open(file)
                .map_err(io)?
                .take(MAX_CONTENT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if workflow_artifacts::reference(&expected.manifest.spec, &bytes)? != expected {
                return Err(Error::new(
                    ErrorCode::InvalidReference,
                    "import bytes differ from the exact reference",
                ));
            }
            report(
                LocalArtifactStore::open(root)?
                    .publish(&expected.manifest.spec, &mut bytes.as_slice())?,
            )
        }
        ["artifact", "cleanup-orphans", root] => {
            report(LocalArtifactStore::open(root)?.cleanup_orphans()?)
        }
        _ => Err(invalid("usage")),
    }
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    match execute(args) {
        Ok(v) => match to_message(&json!({"ok":true,"result":v})) {
            Ok(bytes) => write(stdout, std::str::from_utf8(&bytes).expect("JSON"), 0),
            Err(e) => write(
                stdout,
                &json!({"ok":false,"error":e,"commit_may_have_succeeded":true}).to_string(),
                1,
            ),
        },
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
