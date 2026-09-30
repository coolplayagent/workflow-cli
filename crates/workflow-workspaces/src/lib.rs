//! Portable attempt workspace contracts. Filesystem/Git and worker SDKs belong in adapters.
mod merge;
mod model;
pub use merge::*;
mod validation;
pub use model::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub use validation::*;
pub use workflow_artifacts::{content_digest, digest, parse_message, to_message};
pub const MAX_FILES: usize = 4096;
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TREE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_WORKSPACES: usize = 1000;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidContract,
    InvalidReference,
    Conflict,
    NotFound,
    UnsupportedSource,
    UnsupportedStorage,
    Storage,
    CorruptStorage,
    Budget,
    Changed,
    Timeout,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}
impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<workflow_artifacts::Error> for Error {
    fn from(e: workflow_artifacts::Error) -> Self {
        Self::new(ErrorCode::InvalidReference, e.message)
    }
}
pub type Result<T> = std::result::Result<T, Error>;
pub trait WorkspaceSource {
    /// Read exact committed bytes, independent of the source checkout's dirty files.
    fn read(&self, source: &workflow_artifacts::SourceRevision) -> Result<SourceTree>;
}
pub trait WorkspaceStore {
    /// One immutable allocation per run/node/attempt; duplicate intent never resets edited files.
    fn checkout(
        &mut self,
        spec: &CheckoutSpec,
        source: &dyn WorkspaceSource,
    ) -> Result<WorkspaceRef>;
    fn resolve(&self, link: &WorkspaceLink) -> Result<WorkspaceRef>;
    fn observe(&self, link: &WorkspaceLink) -> Result<Observation>;
    /// Publish exact declared outputs and their manifest; leave no successful manifest on changed input.
    fn capture(
        &mut self,
        link: &WorkspaceLink,
        artifacts: &mut dyn workflow_artifacts::ArtifactStore,
    ) -> Result<Capture>;
}
pub fn schema(kind: &str) -> Result<String> {
    let schema = match kind {
        "checkout" => schemars::schema_for!(CheckoutSpec),
        "ref" => schemars::schema_for!(WorkspaceRef),
        "observation" => schemars::schema_for!(Observation),
        "output" => schemars::schema_for!(OutputManifest),
        "merge-plan" => schemars::schema_for!(MergePlan),
        "merge-proposal" => schemars::schema_for!(MergeProposal),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidContract,
                "workspace schema must be checkout, ref, observation or output",
            ));
        }
    };
    serde_json::to_string_pretty(&schema)
        .map_err(|e| Error::new(ErrorCode::InvalidContract, e.to_string()))
}

#[cfg(test)]
mod tests;
