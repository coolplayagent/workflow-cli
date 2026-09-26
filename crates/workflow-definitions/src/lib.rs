//! Definition editing and registry ports, independent of storage, CLI and providers.
use serde::{Deserialize, Serialize};
use workflow_ir::{Diagnostic, MAX_DOCUMENT_BYTES, Workflow};

mod diff;
mod edit;
pub use diff::{Change, ChangeKind, SemanticDiff, semantic_diff};
pub use edit::{Edit, Patch, apply_patch};

pub const MAX_REVISION: u64 = i64::MAX as u64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidDefinition,
    NotFound,
    AlreadyExists,
    RevisionConflict,
    PublicationConflict,
    RevisionExhausted,
    UnsupportedStorage,
    CorruptStorage,
    Busy,
    Storage,
}

#[derive(Clone, Debug, Serialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub expected_revision: Option<u64>,
    pub actual_revision: Option<u64>,
    pub diagnostics: Vec<Diagnostic>,
}
impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            expected_revision: None,
            actual_revision: None,
            diagnostics: vec![],
        }
    }
    pub fn conflict(expected: u64, actual: u64) -> Self {
        Self {
            expected_revision: Some(expected),
            actual_revision: Some(actual),
            ..Self::new(
                ErrorCode::RevisionConflict,
                "draft changed; read the current revision and rebase the edit",
            )
        }
    }
    pub fn validation(diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            diagnostics,
            ..Self::new(
                ErrorCode::InvalidDefinition,
                "definition has validation errors",
            )
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub draft_id: String,
    pub revision: u64,
    pub deleted: bool,
    pub digest: String,
    pub workflow: Workflow,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedDefinition {
    pub workflow: Workflow,
    pub digest: String,
    pub source_draft_id: String,
    pub source_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DefinitionSummary {
    pub draft_id: Option<String>,
    pub revision: Option<u64>,
    pub workflow_id: String,
    pub version: String,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PageRequest {
    pub after: Option<String>,
    pub limit: u32,
}
impl PageRequest {
    pub fn validate(&self) -> Result<()> {
        if !(1..=100).contains(&self.limit) || self.after.as_ref().is_some_and(|s| s.len() > 128) {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "page limit must be 1..100 and cursor at most 128 bytes",
            ));
        }
        Ok(())
    }
}

/// Adapters must atomically compare revisions and retain history/publication bytes.
/// Delete tombstones a draft identity permanently; that identity may never be reused.
/// Publishing is static acceptance only; execution still needs capability/bundle resolution.
pub trait DefinitionRegistry {
    fn create_draft(&mut self, draft_id: &str, workflow: &Workflow) -> Result<Draft>;
    fn get_draft(&self, draft_id: &str) -> Result<Draft>;
    fn get_revision(&self, draft_id: &str, revision: u64) -> Result<Draft>;
    fn list_drafts(&self, page: &PageRequest) -> Result<Page<DefinitionSummary>>;
    fn edit_draft(&mut self, draft_id: &str, patch: &Patch) -> Result<Draft>;
    fn replace_draft(
        &mut self,
        draft_id: &str,
        expected: u64,
        workflow: &Workflow,
    ) -> Result<Draft>;
    fn delete_draft(&mut self, draft_id: &str, expected: u64) -> Result<Draft>;
    fn publish(&mut self, draft_id: &str, expected: u64) -> Result<PublishedDefinition>;
    fn get_published(&self, workflow_id: &str, version: &str) -> Result<PublishedDefinition>;
    fn get_by_digest(&self, digest: &str) -> Result<PublishedDefinition>;
    fn list_published(
        &self,
        workflow_id: &str,
        page: &PageRequest,
    ) -> Result<Page<DefinitionSummary>>;
}

pub fn validate_draft_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
    {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "draft ID must be 1..128 ASCII letters, digits, dots, underscores or hyphens",
        ));
    }
    Ok(())
}
pub fn validate_revision(revision: u64) -> Result<()> {
    if revision == 0 || revision > MAX_REVISION {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "revision must be 1..9223372036854775807",
        ));
    }
    Ok(())
}

/// Drafts may have unresolved graph/type errors, but their addressing must be unambiguous.
pub fn check_draft(workflow: &Workflow) -> Result<Vec<Diagnostic>> {
    let document = workflow
        .canonical_json()
        .map_err(|e| Error::new(ErrorCode::InvalidDefinition, e.to_string()))?;
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidDefinition,
            "canonical definition exceeds 1 MiB",
        ));
    }
    // A persisted draft must be readable through the same bounded JSON decoder.
    workflow_ir::parse(&document, workflow_ir::Format::Json, "definition")
        .map_err(|diagnostic| Error::validation(vec![*diagnostic]))?;
    let diagnostics = workflow_validator::validate(workflow, "definition");
    if diagnostics.iter().any(|d| {
        matches!(
            d.code.as_str(),
            "duplicate_node"
                | "duplicate_edge"
                | "invalid_id"
                | "graph_limit"
                | "unsupported_schema"
        )
    }) {
        return Err(Error::validation(diagnostics));
    }
    Ok(diagnostics)
}

pub fn check_publish(workflow: &Workflow) -> Result<()> {
    let diagnostics = check_draft(workflow)?;
    if !diagnostics.is_empty() {
        return Err(Error::validation(diagnostics));
    }
    Ok(())
}

pub fn definition_digest(workflow: &Workflow) -> Result<String> {
    workflow
        .digest()
        .map_err(|e| Error::new(ErrorCode::InvalidDefinition, e.to_string()))
}

pub fn patch_schema() -> Result<String> {
    serde_json::to_string_pretty(&schemars::schema_for!(Patch))
        .map_err(|e| Error::new(ErrorCode::InvalidRequest, e.to_string()))
}

#[cfg(test)]
mod tests;
