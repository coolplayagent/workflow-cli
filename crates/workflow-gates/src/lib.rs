//! Deterministic evidence checks, independent of storage, transport and the workflow engine.
//! A PASS is an observation of the supplied target, not an execution grant or approval.
mod evaluation;
mod model;
mod validation;
pub use evaluation::{evaluate, revalidate};
pub use model::*;
pub use validation::validate;
use workflow_artifacts::{ArtifactReader, Producer};
pub use workflow_artifacts::{Error, ErrorCode, Result, digest, parse_message, to_message};

/// The host must authenticate/validate the execution ledger before returning a record.
/// Implementations verify artifact bytes and ancestors through ArtifactReader.
/// Unavailability is UNKNOWN. A matching manifest alone is not execution evidence.
pub trait EvidenceSource: ArtifactReader {
    fn executed_check(&self, producer: &Producer) -> Result<Option<ExecutedCheck>>;
}
pub fn schema(kind: &str) -> Result<String> {
    let value = match kind {
        "request" => schemars::schema_for!(Request),
        "decision" => schemars::schema_for!(Decision),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidDocument,
                "gate schema must be request or decision",
            ));
        }
    };
    serde_json::to_string_pretty(&value)
        .map_err(|e| Error::new(ErrorCode::InvalidDocument, e.to_string()))
}
#[cfg(test)]
mod tests;
