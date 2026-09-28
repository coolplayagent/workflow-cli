use crate::*;
use schemars::JsonSchema;
use workflow_kernel::{InboxEntry, SignalMessage, WaitTarget};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalSubmission {
    pub schema_version: u32,
    pub run_id: String,
    pub run_digest: String,
    pub message: SignalMessage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignalReceipt {
    pub run_id: String,
    pub run_revision: u64,
    pub duplicate: bool,
    pub entry: InboxEntry,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaitRegistration {
    pub target: WaitTarget,
    pub correlation_id: String,
    pub deadline_unix_ms: u64,
    pub paused: bool,
}

/// Call only for a newly reduced transition, before acknowledging its commit.
/// Repeated acknowledgements retain the original receipt after expiry.
pub fn check_signal_admission(snapshot: &Snapshot, commit_at_unix_ms: u64) -> Result<()> {
    if commit_at_unix_ms < snapshot.now_unix_ms
        || workflow_kernel::signal_admission_deadline(snapshot)
            .is_some_and(|deadline| commit_at_unix_ms >= deadline)
    {
        return Err(Error::new(
            ErrorCode::TransitionRejected,
            "host clock reversed or an applied callback expired before commit admission",
        ));
    }
    Ok(())
}

/// Ingress is a trusted host boundary. The host authenticates sources before
/// submitting observations; a payload's source label grants no permission.
pub trait InboxStore: RunStore {
    fn receive_signal(
        &mut self,
        submission: &SignalSubmission,
        clock: &dyn workflow_worker::Clock,
    ) -> Result<SignalReceipt>;
    fn inbox(
        &mut self,
        run_id: &str,
        after_revision: u64,
        limit: u32,
    ) -> Result<Page<InboxEntry, u64>>;
    fn waits(
        &mut self,
        run_id: &str,
        after_instance: u64,
        limit: u32,
    ) -> Result<Page<WaitRegistration, u64>>;
}
