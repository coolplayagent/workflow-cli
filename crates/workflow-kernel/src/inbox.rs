use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_INBOX_MESSAGES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitTarget {
    pub instance_id: u64,
    pub definition_digest: String,
    pub input_digest: String,
    pub event: String,
}

/// A host-verified observation. This type does not authenticate its source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalMessage {
    pub schema_version: u32,
    pub message_id: String,
    pub correlation_id: String,
    pub target: WaitTarget,
    pub source: String,
    pub decision: SignalDecision,
    pub reason: String,
    pub outputs: Values,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignalDecision {
    Approve,
    Reject,
    RequestChanges,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalRejection {
    UnknownInstance,
    NotAWait,
    DefinitionMismatch,
    EventMismatch,
    InputMismatch,
    InvalidOutputs,
    Expired,
    WaitExpired,
    RunCancelled,
    AlreadySettled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SignalStatus {
    Pending,
    Applied {
        revision: u64,
        at_unix_ms: u64,
        admission_deadline_unix_ms: u64,
    },
    Rejected {
        revision: u64,
        at_unix_ms: u64,
        reason: SignalRejection,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxEntry {
    pub message: SignalMessage,
    pub received_revision: u64,
    pub received_at_unix_ms: u64,
    pub status: SignalStatus,
}

pub fn signal_correlation(run_digest: &str, target: &WaitTarget) -> Result<String> {
    Ok(workflow_worker::digest(&(run_digest, target))?)
}

/// Minimum deadline of callbacks applied by this new transition. Hosts sample
/// their clock again before committing, including when early callbacks activate
/// as a result of a task, gate or timer event.
pub fn signal_admission_deadline(snapshot: &Snapshot) -> Option<u64> {
    snapshot
        .inbox
        .values()
        .filter_map(|entry| match entry.status {
            SignalStatus::Applied {
                revision,
                admission_deadline_unix_ms,
                ..
            } if revision == snapshot.revision => Some(admission_deadline_unix_ms),
            _ => None,
        })
        .min()
}

pub fn validate_signal(run_digest: &str, message: &SignalMessage) -> Result<()> {
    let sha = |s: &str| {
        s.strip_prefix("sha256:").is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    };
    if message.schema_version != 1
        || !workflow_validator::identifier(&message.message_id)
        || !workflow_validator::identifier(&message.source)
        || !workflow_validator::identifier(&message.target.event)
        || message.target.instance_id == 0
        || !sha(&message.target.definition_digest)
        || !sha(&message.target.input_digest)
        || message.correlation_id != signal_correlation(run_digest, &message.target)?
        || message.reason.trim().is_empty()
        || message.reason.len() > 1024
        || message.expires_at_unix_ms == 0
        || (message.decision != SignalDecision::Approve && !message.outputs.is_empty())
    {
        return Err(Error::new(
            ErrorCode::InvalidSignal,
            "signal requires schema 1, stable identities, exact correlation, digests, bounded reason, expiry and decision-compatible outputs",
        ));
    }
    workflow_worker::to_message(message)?;
    Ok(())
}
