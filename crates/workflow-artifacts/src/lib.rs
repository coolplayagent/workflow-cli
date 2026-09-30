//! Portable, typed artifact identities and immutable provenance. No storage or worker dependency.
mod codec;
mod model;
mod revalidation;
pub use revalidation::*;
mod validation;
pub use codec::{digest, parse_message, to_message};
pub use model::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub use validation::*;
pub const MAX_JSON_BYTES: usize = 2_097_152;
pub const MAX_CONTENT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 65_536;
pub const MAX_LINEAGE: usize = 512;
pub const MAX_CATALOG: usize = 10_000;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidDocument,
    InvalidContract,
    InvalidContent,
    InvalidReference,
    TypeConflict,
    NotFound,
    CorruptStorage,
    UnsupportedStorage,
    Storage,
    Busy,
    Budget,
    ScopeMismatch,
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
pub type Result<T> = std::result::Result<T, Error>;
/// Implementations verify the immutable manifest, bytes, type and all source artifacts.
/// Access to the store is authorized by the host; digests are not credentials.
pub trait ArtifactReader {
    fn verify(&self, reference: &ArtifactLink) -> Result<ArtifactRef>;
}
/// Publication exposes a reference only after payload and manifest durability.
/// Implementations must not delete any committed manifest or its content.
pub trait ArtifactStore: ArtifactReader {
    fn publish(
        &mut self,
        spec: &PublishSpec,
        content: &mut dyn std::io::Read,
    ) -> Result<ArtifactRef>;
    fn read(&self, reference: &ArtifactLink) -> Result<Vec<u8>>;
}
/// Complete retained manifest inventory. Payload verification is still required.
pub trait ArtifactInventory: ArtifactStore {
    fn retained_manifests(&self) -> Result<Vec<ArtifactRef>>;
}
pub fn schema(kind: &str) -> Result<String> {
    let s = match kind {
        "publish" => schemars::schema_for!(PublishSpec),
        "ref" => schemars::schema_for!(ArtifactRef),
        "type" => schemars::schema_for!(ArtifactType),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidDocument,
                "artifact schema must be publish, ref or type",
            ));
        }
    };
    serde_json::to_string_pretty(&s)
        .map_err(|e| Error::new(ErrorCode::InvalidDocument, e.to_string()))
}
#[cfg(test)]
mod tests;
