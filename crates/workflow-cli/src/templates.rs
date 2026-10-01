use crate::write;
use serde_json::{Value, json};
use std::io::Write;
use workflow_registry_sqlite::SqliteTemplateCatalog;
use workflow_templates::*;
#[cfg(test)]
mod tests;

pub const HELP: &str = "REVIEWED WORKFLOW TEMPLATES\n  workflow template validate <template.json>\n  workflow template plan <template.json> <instance.json>\n  workflow template diff <before.json> <after.json>\n  workflow template init <catalog-db> <owners.json>\n  workflow template propose <catalog-db> <candidate.json> <actor>\n  workflow template candidate <catalog-db> <candidate-digest>\n  workflow template review <catalog-db> <candidate-digest> <actor> <approve|reject> <reason>\n  workflow template publish <catalog-db> <candidate-digest>\n  workflow template get <catalog-db> <template-id> <version>\n  workflow template instantiate <catalog-db> <template-id> <version> <instance.json>\n  workflow schema <template|template-instance|template-candidate|template-owners>\n\nplan is a pure dry-run: it validates parameters and declared bindings and lists possible writes without calling any adapter.\nOnly init creates a catalog. Local actor names are trusted-host attestations; the TLS API authenticates shared reviewers.\nReview is independent from proposal and binds regression references plus change/compatibility reasons. Published definitions and child versions are immutable.\nInstantiate emits publication provenance and plan.request, which can be passed to the existing local or shared runtime. Host planning declarations do not issue execution credentials.\n";
fn invalid(path: &str, message: &str) -> Error {
    Error {
        path: path.into(),
        message: message.into(),
    }
}
fn read<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    let (source, format) =
        crate::load_source(path).map_err(|_| invalid("io", "template input could not be read"))?;
    if !matches!(format, workflow_ir::Format::Json) {
        return Err(invalid("io", "template inputs require JSON"));
    }
    workflow_worker::parse_message(source.as_bytes())
        .map_err(|_| invalid("$", "invalid template JSON or unknown schema field"))
}
fn value<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| invalid("$", "output encoding failed"))
}
fn identity(id: &str, version: &str) -> workflow_ir::VersionRef {
    workflow_ir::VersionRef {
        id: id.into(),
        version: version.into(),
    }
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let result = (|| -> Result<Value> {
        match args {
            ["schema", "template"] => value(schemars::schema_for!(Template)),
            ["schema", "template-instance"] => value(schemars::schema_for!(InstanceRequest)),
            ["schema", "template-candidate"] => value(schemars::schema_for!(Candidate)),
            ["schema", "template-owners"] => value(schemars::schema_for!(OwnerPolicy)),
            ["template", "validate", path] => {
                let t: Template = read(path)?;
                let compiled = t.validate()?;
                Ok(
                    json!({"valid":true,"template_digest":t.content_digest()?,"bundle_digest":compiled.digest(),"owner_role":t.owner_role,"deliverables":t.deliverables}),
                )
            }
            ["template", "plan", path, instance] => value(plan(&read(path)?, &read(instance)?)?),
            ["template", "diff", before, after] => value(diff(&read(before)?, &read(after)?)?),
            ["template", "init", db, owners] => {
                SqliteTemplateCatalog::create(db, &read(owners)?)?;
                Ok(json!({"initialized":true}))
            }
            ["template", "propose", db, path, actor] => {
                let digest =
                    SqliteTemplateCatalog::open(db, false)?.propose(&read(path)?, actor)?;
                Ok(json!({"candidate_digest":digest}))
            }
            ["template", "candidate", db, digest] => {
                value(SqliteTemplateCatalog::open(db, true)?.candidate(digest)?)
            }
            ["template", "review", db, digest, actor, decision, reason] => {
                let decision = match *decision {
                    "approve" => ReviewDecision::Approve,
                    "reject" => ReviewDecision::Reject,
                    _ => {
                        return Err(invalid(
                            "usage",
                            "review decision must be approve or reject",
                        ));
                    }
                };
                let at = workflow_worker::Clock::now_unix_ms(&workflow_worker::SystemClock)
                    .map_err(|_| invalid("io", "host time unavailable"))?;
                value(
                    SqliteTemplateCatalog::open(db, false)?
                        .review(digest, actor, decision, reason, at)?,
                )
            }
            ["template", "publish", db, digest] => {
                value(SqliteTemplateCatalog::open(db, false)?.publish(digest)?)
            }
            ["template", "get", db, id, version] => {
                value(SqliteTemplateCatalog::open(db, true)?.get(&identity(id, version))?)
            }
            ["template", "instantiate", db, id, version, path] => {
                let published =
                    SqliteTemplateCatalog::open(db, true)?.get(&identity(id, version))?;
                let planned = plan(&published.candidate.template, &read(path)?)?;
                Ok(json!({"publication_digest":published.digest,"plan":planned}))
            }
            _ => Err(invalid("usage", "unknown template command")),
        }
    })();
    match result {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(text) => write(stdout, &text, 0),
            Err(_) => write(stderr, "template output failed", 2),
        },
        Err(error) => {
            let code = if error.path == "io" || error.path == "usage" {
                2
            } else {
                1
            };
            if error.path == "usage" {
                write(stderr, HELP, code)
            } else {
                write(stderr, &json!({"error":error}).to_string(), code)
            }
        }
    }
}
