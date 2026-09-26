use crate::{load, write};
use serde_json::{Value, json};
use std::io::{Read, Write};
use workflow_definitions::{
    DefinitionRegistry, Error, ErrorCode, PageRequest, Patch, Result, semantic_diff,
};
use workflow_ir::{MAX_DOCUMENT_BYTES, Workflow};
use workflow_registry_sqlite::SqliteRegistry;

pub const HELP: &str = "DEFINITION REGISTRY\n  workflow schema patch\n  workflow diff <before.json|yaml> <after.json|yaml>\n  workflow draft create <db> <draft-id> <file.json|yaml>\n  workflow draft get <db> <draft-id>\n  workflow draft revision <db> <draft-id> <revision>\n  workflow draft diff <db> <draft-id> <before-revision> <after-revision>\n  workflow draft list <db> <after-id|-> <limit>\n  workflow draft edit <db> <draft-id> <patch.json>\n  workflow draft replace <db> <draft-id> <expected-revision> <file.json|yaml>\n  workflow draft delete <db> <draft-id> <expected-revision>\n  workflow draft publish <db> <draft-id> <expected-revision>\n  workflow draft export <db> <draft-id> <json|yaml>\n  workflow release get <db> <workflow-id> <version>\n  workflow release digest <db> <sha256:digest>\n  workflow release list <db> <workflow-id> <after-version|-> <limit>\n  workflow release export <db> <workflow-id> <version> <json|yaml>\n\nOnly draft create initializes a database. Publication requires a valid definition.\nAll mutations of existing drafts require an expected revision.\nDrafts may contain graph errors; get/create/edit/replace return diagnostics.\nExit 1: invalid definition, missing identity, or conflict. Exit 2: invalid request, I/O or storage error.\n";

enum Output {
    Json(Value),
    Text(String),
}

pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    // Match shape before opening a database; a typo must never initialize storage.
    let result = match args {
        ["schema", "patch"] => workflow_definitions::patch_schema().map(Output::Text),
        ["diff", before, after] => diff(before, after),
        ["draft", "create", db, id, file] => create(db, id, file),
        ["draft", "get", db, id] => SqliteRegistry::open(db)
            .and_then(|r| r.get_draft(id))
            .map(|d| Output::Json(json!({"ok":true,"draft":d}))),
        ["draft", "revision", db, id, revision] => number(revision)
            .and_then(|n| SqliteRegistry::open(db)?.get_revision(id, n))
            .map(|d| Output::Json(json!({"ok":true,"draft":d}))),
        ["draft", "diff", db, id, before, after] => revision_diff(db, id, before, after),
        ["draft", "list", db, after, limit] => page(after, limit)
            .and_then(|p| SqliteRegistry::open(db)?.list_drafts(&p))
            .map(|p| Output::Json(json!({"ok":true,"page":p}))),
        ["draft", "edit", db, id, file] => read_patch(file)
            .and_then(|p| SqliteRegistry::open(db)?.edit_draft(id, &p))
            .map(|d| Output::Json(json!({"ok":true,"draft":d}))),
        ["draft", "replace", db, id, revision, file] => number(revision)
            .and_then(|n| {
                let w = definition(file)?;
                SqliteRegistry::open(db)?.replace_draft(id, n, &w)
            })
            .map(|d| Output::Json(json!({"ok":true,"draft":d}))),
        ["draft", "delete", db, id, revision] => number(revision)
            .and_then(|n| SqliteRegistry::open(db)?.delete_draft(id, n))
            .map(|d| Output::Json(json!({"ok":true,"draft":d}))),
        ["draft", "publish", db, id, revision] => number(revision)
            .and_then(|n| SqliteRegistry::open(db)?.publish(id, n))
            .map(|p| Output::Json(json!({"ok":true,"publication":p}))),
        ["draft", "export", db, id, format @ ("json" | "yaml")] => SqliteRegistry::open(db)
            .and_then(|r| r.get_draft(id))
            .and_then(|d| export(&d.workflow, format)),
        ["release", "get", db, id, version] => SqliteRegistry::open(db)
            .and_then(|r| r.get_published(id, version))
            .map(|p| Output::Json(json!({"ok":true,"publication":p}))),
        ["release", "digest", db, digest] => SqliteRegistry::open(db)
            .and_then(|r| r.get_by_digest(digest))
            .map(|p| Output::Json(json!({"ok":true,"publication":p}))),
        ["release", "list", db, id, after, limit] => page(after, limit)
            .and_then(|p| SqliteRegistry::open(db)?.list_published(id, &p))
            .map(|p| Output::Json(json!({"ok":true,"page":p}))),
        [
            "release",
            "export",
            db,
            id,
            version,
            format @ ("json" | "yaml"),
        ] => SqliteRegistry::open(db)
            .and_then(|r| r.get_published(id, version))
            .and_then(|p| export(&p.workflow, format)),
        _ => return write(stderr, HELP, 2),
    };
    match result {
        Ok(Output::Json(value)) => match serde_json::to_string_pretty(&value) {
            Ok(text) => write(stdout, &text, 0),
            Err(e) => write(stderr, &e.to_string(), 2),
        },
        Ok(Output::Text(text)) => write(stdout, &text, 0),
        Err(error) => {
            let code = match error.code {
                ErrorCode::InvalidRequest
                | ErrorCode::Storage
                | ErrorCode::Busy
                | ErrorCode::UnsupportedStorage
                | ErrorCode::CorruptStorage => 2,
                _ => 1,
            };
            let value = json!({"ok":false,"error":error});
            let text = value.to_string();
            if matches!(args, ["draft" | "release", "export", ..]) {
                write(stderr, &text, code)
            } else {
                write(stdout, &text, code)
            }
        }
    }
}
fn definition(path: &str) -> Result<Workflow> {
    load(path).map_err(|d| {
        let code = match d.code.as_str() {
            "io_error" => ErrorCode::Storage,
            "format_error" => ErrorCode::InvalidRequest,
            _ => ErrorCode::InvalidDefinition,
        };
        let mut error = Error::new(code, d.message.clone());
        error.diagnostics = vec![*d];
        error
    })
}
fn create(db: &str, id: &str, file: &str) -> Result<Output> {
    let w = definition(file)?;
    workflow_definitions::validate_draft_id(id)?;
    workflow_definitions::check_draft(&w)?;
    let d = SqliteRegistry::create(db)?.create_draft(id, &w)?;
    Ok(Output::Json(json!({"ok":true,"draft":d})))
}
fn diff(before: &str, after: &str) -> Result<Output> {
    let d = semantic_diff(&definition(before)?, &definition(after)?)?;
    Ok(Output::Json(json!({"ok":true,"diff":d})))
}
fn number(value: &str) -> Result<u64> {
    value
        .parse()
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "expected an unsigned integer"))
}
fn page(after: &str, limit: &str) -> Result<PageRequest> {
    let limit = u32::try_from(number(limit)?)
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "page limit must be 1..100"))?;
    let p = PageRequest {
        after: if after == "-" {
            None
        } else {
            Some(after.into())
        },
        limit,
    };
    p.validate()?;
    Ok(p)
}
fn read_patch(path: &str) -> Result<Patch> {
    let mut document = String::new();
    std::fs::File::open(path)
        .and_then(|f| {
            f.take((MAX_DOCUMENT_BYTES + 1) as u64)
                .read_to_string(&mut document)
        })
        .map_err(|e| Error::new(ErrorCode::Storage, e.to_string()))?;
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::new(ErrorCode::InvalidRequest, "patch exceeds 1 MiB"));
    }
    serde_json::from_str(&document)
        .map_err(|e| Error::new(ErrorCode::InvalidRequest, format!("{path}: {e}")))
}
fn export(w: &Workflow, format: &str) -> Result<Output> {
    let text = if format == "json" {
        w.canonical_json().map_err(|e| e.to_string())
    } else {
        w.to_yaml().map_err(|e| e.to_string())
    };
    text.map(Output::Text)
        .map_err(|e| Error::new(ErrorCode::InvalidDefinition, e))
}

#[cfg(test)]
mod tests;

fn revision_diff(db: &str, id: &str, before: &str, after: &str) -> Result<Output> {
    let before = number(before)?;
    let after = number(after)?;
    let registry = SqliteRegistry::open(db)?;
    let before = registry.get_revision(id, before)?;
    let after = registry.get_revision(id, after)?;
    let diff = semantic_diff(&before.workflow, &after.workflow)?;
    Ok(Output::Json(
        json!({"ok":true,"diff":diff,"before_deleted":before.deleted,"after_deleted":after.deleted}),
    ))
}
