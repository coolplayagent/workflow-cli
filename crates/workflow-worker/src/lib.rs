//! Versioned capability boundary. Adapters receive inputs, never authoritative run state.
mod codec;
mod descriptor;
mod protocol;
mod worker;

pub use codec::{digest, parse_message, to_message};
pub use descriptor::*;
pub use protocol::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub use worker::*;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_MESSAGE_BYTES: usize = 2_097_152;
pub type Values = BTreeMap<String, serde_json::Value>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidMessage,
    UnsupportedProtocol,
    InvalidDescriptor,
    InvalidRequest,
    Unauthorized,
    Expired,
    DigestMismatch,
    InvalidInput,
    InvalidOutput,
    MissingCapability,
    DuplicateCapability,
    UnsupportedEffect,
    InvalidBinding,
    PreconditionFailed,
    InvalidResult,
    AdapterPanicked,
    DeadlineExceeded,
    ClockError,
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
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

pub fn schema(kind: &str) -> Result<String> {
    let value = match kind {
        "capability" => schemars::schema_for!(CapabilityDescriptor),
        "request" => schemars::schema_for!(WorkRequest),
        "grant" => schemars::schema_for!(ExecutionGrant),
        "result" => schemars::schema_for!(WorkResult),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "schema must be capability, request, grant or result",
            ));
        }
    };
    serde_json::to_string_pretty(&value)
        .map_err(|e| Error::new(ErrorCode::InvalidMessage, e.to_string()))
}

#[cfg(test)]
mod tests;
