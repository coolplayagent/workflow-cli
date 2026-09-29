use crate::*;
use serde::{Deserialize, Serialize};
use workflow_runstore::*;
use workflow_runstore_postgres::access::{
    AuditEntry, AuthenticatedService, Dispatch, OutstandingAssignment, TaskReceipt,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 2_097_152;
pub const MAX_RESPONSE_BYTES: usize = 16_777_216;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: Operation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Publish {
        bundle: Box<BundleSpec>,
    },
    Start {
        request: Box<StartRun>,
    },
    Get {
        run_id: String,
    },
    History {
        run_id: String,
        after: u64,
        limit: u32,
    },
    Waits {
        run_id: String,
        after: u64,
        limit: u32,
    },
    Inbox {
        run_id: String,
        after: u64,
        limit: u32,
    },
    Acquire {
        run_id: String,
        acquisition_id: String,
        ttl_ms: u64,
    },
    Release {
        lease: Lease,
    },
    Tick {
        lease: Lease,
    },
    Dispatch {
        lease: Lease,
        worker_id: String,
    },
    Assignment {
        assignment_id: String,
    },
    Finish {
        assignment_id: String,
        result: Box<workflow_worker::WorkResult>,
    },
    Fail {
        assignment_id: String,
        error: workflow_worker::Error,
    },
    Approve {
        request: Box<SignalSubmission>,
    },
    Revoke {
        credential_id: String,
    },
    Audit {
        after: i64,
        limit: u32,
    },
    Outstanding {
        after: String,
        limit: u32,
    },
    Pending {
        after: String,
        limit: u32,
    },
    Runs {
        after: Option<String>,
        limit: u32,
    },
    AcknowledgeRecovery {
        run_id: String,
        request: RecoveryAcknowledgement,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub protocol_version: u32,
    pub request_id: String,
    pub result: Result<Response>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Response {
    Published(String),
    Committed(Box<Committed>),
    Snapshot(Box<Snapshot>),
    History(Page<RecordedEvent, u64>),
    Waits(Page<WaitRegistration, u64>),
    Inbox(Page<workflow_kernel::InboxEntry, u64>),
    Lease(Lease),
    Tick(Option<Box<Committed>>),
    Dispatch(Dispatch),
    Assignment(Box<PreparedTask>),
    Finished(TaskReceipt),
    Approved(Box<SignalReceipt>),
    Unit,
    Audit(Page<AuditEntry, i64>),
    Outstanding(Page<OutstandingAssignment, String>),
    Pending(Page<String, String>),
    Runs(Page<RunSummary, String>),
    Recovery(bool),
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(invalid());
        }
        validate_id(&self.request_id)
    }
    pub fn execute(&self, service: &mut AuthenticatedService, token: &str) -> Result<Response> {
        self.validate()?;
        match &self.operation {
            Operation::Publish { bundle } => {
                service.publish(token, bundle).map(Response::Published)
            }
            Operation::Start { request } => service
                .start(token, request)
                .map(|r| Response::Committed(Box::new(r))),
            Operation::Get { run_id } => service
                .get(token, run_id)
                .map(|s| Response::Snapshot(Box::new(s))),
            Operation::History {
                run_id,
                after,
                limit,
            } => service
                .history(token, run_id, *after, *limit)
                .map(Response::History),
            Operation::Waits {
                run_id,
                after,
                limit,
            } => service
                .waits(token, run_id, *after, *limit)
                .map(Response::Waits),
            Operation::Inbox {
                run_id,
                after,
                limit,
            } => service
                .inbox(token, run_id, *after, *limit)
                .map(Response::Inbox),
            Operation::Acquire {
                run_id,
                acquisition_id,
                ttl_ms,
            } => service
                .acquire(token, run_id, acquisition_id, *ttl_ms)
                .map(Response::Lease),
            Operation::Release { lease } => service.release(token, lease).map(|_| Response::Unit),
            Operation::Tick { lease } => service
                .tick(token, lease)
                .map(|r| Response::Tick(r.map(Box::new))),
            Operation::Dispatch { lease, worker_id } => service
                .dispatch(token, lease, worker_id)
                .map(Response::Dispatch),
            Operation::Assignment { assignment_id } => service
                .assignment(token, assignment_id)
                .map(|r| Response::Assignment(Box::new(r))),
            Operation::Finish {
                assignment_id,
                result,
            } => service
                .finish(token, assignment_id, result)
                .map(Response::Finished),
            Operation::Fail {
                assignment_id,
                error,
            } => service
                .fail(token, assignment_id, error)
                .map(|_| Response::Unit),
            Operation::Approve { request } => service
                .approve(token, request)
                .map(|r| Response::Approved(Box::new(r))),
            Operation::Revoke { credential_id } => {
                service.revoke(token, credential_id).map(|_| Response::Unit)
            }
            Operation::Audit { after, limit } => {
                service.audit(token, *after, *limit).map(Response::Audit)
            }
            Operation::Outstanding { after, limit } => service
                .outstanding(token, after, *limit)
                .map(Response::Outstanding),
            Operation::Pending { after, limit } => {
                service.pending(token, after, *limit).map(Response::Pending)
            }
            Operation::Runs { after, limit } => service
                .runs(token, after.as_deref(), *limit)
                .map(Response::Runs),
            Operation::AcknowledgeRecovery { run_id, request } => service
                .acknowledge_recovery(token, run_id, request)
                .map(Response::Recovery),
        }
    }
    pub fn accepts(&self, response: &Response) -> bool {
        matches!(
            (&self.operation, response),
            (Operation::Publish { .. }, Response::Published(_))
                | (Operation::Start { .. }, Response::Committed(_))
                | (Operation::Get { .. }, Response::Snapshot(_))
                | (Operation::History { .. }, Response::History(_))
                | (Operation::Waits { .. }, Response::Waits(_))
                | (Operation::Inbox { .. }, Response::Inbox(_))
                | (Operation::Acquire { .. }, Response::Lease(_))
                | (Operation::Release { .. }, Response::Unit)
                | (Operation::Tick { .. }, Response::Tick(_))
                | (Operation::Dispatch { .. }, Response::Dispatch(_))
                | (Operation::Assignment { .. }, Response::Assignment(_))
                | (Operation::Finish { .. }, Response::Finished(_))
                | (Operation::Fail { .. }, Response::Unit)
                | (Operation::Approve { .. }, Response::Approved(_))
                | (Operation::Revoke { .. }, Response::Unit)
                | (Operation::Audit { .. }, Response::Audit(_))
                | (Operation::Outstanding { .. }, Response::Outstanding(_))
                | (Operation::Pending { .. }, Response::Pending(_))
                | (Operation::Runs { .. }, Response::Runs(_))
                | (Operation::AcknowledgeRecovery { .. }, Response::Recovery(_))
        )
    }
}
