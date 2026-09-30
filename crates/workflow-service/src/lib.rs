//! TLS transport for the authenticated application interface. Requests contain
//! operation data; credentials and database connections stay outside that data.
mod artifacts;
mod binding;
mod client;
mod execution;
mod protocol;
mod server;
mod transport;
mod worker_session;
pub use binding::*;
pub use client::*;
pub use execution::*;
pub use protocol::*;
pub use server::*;
pub use transport::{InProcessTransport, TaskTransport};
pub use worker_session::{DrainHandle, WorkerSession};
pub use workflow_runstore::{Error, ErrorCode, Result};

fn invalid() -> Error {
    Error::new(
        ErrorCode::InvalidRequest,
        "invalid service configuration or protocol message",
    )
}
fn unavailable() -> Error {
    Error::new(
        ErrorCode::Storage,
        "service unavailable; durable outcome may be unknown",
    )
}

#[cfg(test)]
mod tests;
