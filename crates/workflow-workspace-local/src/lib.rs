//! Local Linux workspaces. Each attempt gets independent files, not a process sandbox.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "workflow-workspace-local currently requires Linux and /proc; portable contracts are in workflow-workspaces"
);
mod files;
mod git;
mod store;
pub use git::GitSource;
pub use store::LocalWorkspaceStore;
use workflow_workspaces::*;
fn io(e: std::io::Error) -> Error {
    Error::new(ErrorCode::Storage, e.to_string())
}
fn sql(e: rusqlite::Error) -> Error {
    Error::new(ErrorCode::Storage, e.to_string())
}
fn corrupt(m: impl Into<String>) -> Error {
    Error::new(ErrorCode::CorruptStorage, m)
}

#[cfg(test)]
mod tests;
