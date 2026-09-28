//! Node-local model policy execution. No provider, network, database or authoritative state writer.
mod model;
mod policy;
mod session;
pub use model::*;
pub use policy::*;
pub use session::*;
pub use workflow_worker::{Error, ErrorCode, Result, digest, parse_message, to_message};
#[cfg(test)]
mod tests;
pub fn schema(kind: &str) -> Result<String> {
    let s = match kind {
        "policy" => schemars::schema_for!(PolicySpec),
        "proposal" => schemars::schema_for!(Proposal),
        "record" => schemars::schema_for!(ModelRecord),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "model schema must be policy, proposal or record",
            ));
        }
    };
    serde_json::to_string_pretty(&s)
        .map_err(|_| Error::new(ErrorCode::InvalidMessage, "model schema encoding failed"))
}
