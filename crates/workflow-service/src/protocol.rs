use crate::*;
use serde::{Deserialize, Serialize};
use workflow_runstore::*;
use workflow_runstore_postgres::access::{
    ArtifactCleanup, ArtifactDownloadChunk, ArtifactDownloadGrant, ArtifactUploadRequest,
    ArtifactUploadStatus, AuditEntry, AuditExport, AuthenticatedService, DeadLetter,
    DeadLetterResolution, Dispatch, EffectDispatch, OutstandingAssignment, OutstandingEffect,
    RoutedDispatch, TaskReceipt, WorkerStatus,
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
    TemplatePropose {
        candidate: Box<workflow_templates::Candidate>,
    },
    TemplateCandidate {
        digest: String,
    },
    TemplateReview {
        digest: String,
        decision: workflow_templates::ReviewDecision,
        reason: String,
    },
    TemplatePublish {
        digest: String,
    },
    TemplateGet {
        identity: workflow_ir::VersionRef,
    },
    Renew {
        lease: Lease,
        ttl_ms: u64,
    },
    WorkerHeartbeat {
        runtime_version: String,
        drain: bool,
    },
    WorkerStatus {
        worker_id: String,
    },
    ControlWorker {
        worker_id: String,
        drain: bool,
        reason: String,
    },
    SetPriority {
        run_id: String,
        priority: u8,
    },
    ScheduleCandidates {
        limit: u32,
    },
    DispatchRouted {
        lease: Lease,
        worker_ids: Vec<String>,
        effects: bool,
    },
    DeadLetters {
        after: String,
        limit: u32,
    },
    ResolveDeadLetter {
        resolution: DeadLetterResolution,
    },
    DeadLetter {
        id: String,
    },
    Acceptance {
        run_id: String,
    },
    ExportAudit,
    AssignmentForPrincipal {
        assignment_id: String,
        principal: workflow_credentials::Principal,
    },
    EffectAssignmentForPrincipal {
        assignment_id: String,
        principal: workflow_credentials::Principal,
    },
    PlanStorageUpgrade {
        run_id: String,
    },
    UpgradeStorage {
        run_id: String,
        plan: StorageUpgrade,
    },
    PlanMigration {
        run_id: String,
        request: Box<MigrationRequest>,
    },
    MigrateDefinition {
        lease: Lease,
        plan: Box<MigrationPlan>,
    },
    HistoricalSnapshot {
        run_id: String,
        revision: u64,
    },
    RecoveryBarrier {
        run_id: String,
    },
    ImportRestoredEffect {
        lease: Lease,
        request: Box<RestoredEffect>,
    },
    Control {
        request: workflow_runstore_postgres::access::RunControlRequest,
    },
    DispatchEffect {
        lease: Lease,
        worker_id: String,
    },
    PendingEffects {
        after: String,
        limit: u32,
    },
    EffectAssignment {
        assignment_id: String,
    },
    ObserveEffect {
        assignment_id: String,
        observation: Box<workflow_effects::Observation>,
    },
    Effects {
        run_id: String,
        after: u64,
        limit: u32,
    },
    ResolveEffect {
        lease: Lease,
        operation_key: String,
        resolution: Box<workflow_effects::ManualResolution>,
    },
    OutstandingEffects {
        after: String,
        limit: u32,
    },
    ValidateDefinition {
        source: String,
        format: workflow_ir::Format,
        file: String,
    },
    ArtifactBegin {
        request: Box<ArtifactUploadRequest>,
    },
    ArtifactPut {
        upload_id: String,
        offset: u64,
        content: Vec<u8>,
    },
    ArtifactComplete {
        upload_id: String,
    },
    ArtifactGrant {
        artifact: workflow_artifacts::ArtifactLink,
        assignment_id: Option<String>,
        ttl_ms: u64,
    },
    ArtifactGet {
        download_id: String,
        offset: u64,
    },
    ArtifactCleanup {
        limit: u32,
    },
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
    Signal {
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
    TemplateProposed(String),
    TemplateCandidate(Box<workflow_templates::Candidate>),
    TemplateReviewed(Box<workflow_templates::Review>),
    TemplatePublication(Box<workflow_templates::Publication>),
    DeadLetter(Box<workflow_runstore_postgres::access::DeadLetterRecord>),
    WorkerStatus(WorkerStatus),
    Candidates(Vec<RunSummary>),
    RoutedDispatch(RoutedDispatch),
    DeadLetters(Page<DeadLetter, String>),
    Acceptance(Box<AcceptanceManifest>),
    AuditExport(AuditExport),
    StorageUpgrade(StorageUpgrade),
    MigrationPlan(Box<MigrationPlan>),
    RecoveryBarrier(Option<Box<RecoveryBarrier>>),
    EffectDispatch(EffectDispatch),
    EffectAssignment(Box<workflow_effects::EffectAttempt>),
    Effects(Page<workflow_effects::EffectState, u64>),
    OutstandingEffects(Page<OutstandingEffect, String>),
    Validation(workflow_validator::ValidationReport),
    ArtifactUpload(Box<ArtifactUploadStatus>),
    Artifact(Box<workflow_artifacts::ArtifactRef>),
    ArtifactGrant(Box<ArtifactDownloadGrant>),
    ArtifactChunk(ArtifactDownloadChunk),
    ArtifactCleanup(ArtifactCleanup),
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
    SignalReceived(Box<SignalReceipt>),
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
        if let Operation::ArtifactPut { content, .. } = &self.operation
            && (content.is_empty()
                || content.len() > workflow_runstore_postgres::access::ARTIFACT_CHUNK_BYTES)
        {
            return Err(invalid());
        }
        validate_id(&self.request_id)
    }
    pub fn execute(&self, service: &mut AuthenticatedService, token: &str) -> Result<Response> {
        self.validate()?;
        if workflow_credentials::reflects(self, token) {
            return Err(invalid());
        }
        match &self.operation {
            Operation::TemplatePropose { candidate } => service
                .propose_template(token, candidate)
                .map(Response::TemplateProposed),
            Operation::TemplateCandidate { digest } => service
                .template_candidate(token, digest)
                .map(|c| Response::TemplateCandidate(Box::new(c))),
            Operation::TemplateReview {
                digest,
                decision,
                reason,
            } => service
                .review_template(token, digest, *decision, reason)
                .map(|r| Response::TemplateReviewed(Box::new(r))),
            Operation::TemplatePublish { digest } => service
                .publish_template(token, digest)
                .map(|p| Response::TemplatePublication(Box::new(p))),
            Operation::TemplateGet { identity } => service
                .template_publication(token, identity)
                .map(|p| Response::TemplatePublication(Box::new(p))),
            Operation::DeadLetter { id } => service
                .dead_letter(token, id)
                .map(|r| Response::DeadLetter(Box::new(r))),
            Operation::Renew { lease, ttl_ms } => {
                service.renew(token, lease, *ttl_ms).map(Response::Lease)
            }
            Operation::WorkerHeartbeat {
                runtime_version,
                drain,
            } => service
                .worker_heartbeat(token, runtime_version, *drain)
                .map(Response::WorkerStatus),
            Operation::WorkerStatus { worker_id } => service
                .worker_status(token, worker_id)
                .map(Response::WorkerStatus),
            Operation::ControlWorker {
                worker_id,
                drain,
                reason,
            } => service
                .control_worker(token, worker_id, *drain, reason)
                .map(Response::WorkerStatus),
            Operation::SetPriority { run_id, priority } => service
                .set_priority(token, run_id, *priority)
                .map(|_| Response::Unit),
            Operation::ScheduleCandidates { limit } => service
                .schedule_candidates(token, *limit)
                .map(Response::Candidates),
            Operation::DispatchRouted {
                lease,
                worker_ids,
                effects,
            } => service
                .dispatch_routed(token, lease, worker_ids, *effects)
                .map(Response::RoutedDispatch),
            Operation::DeadLetters { after, limit } => service
                .dead_letters(token, after, *limit)
                .map(Response::DeadLetters),
            Operation::ResolveDeadLetter { resolution } => service
                .resolve_dead_letter(token, resolution)
                .map(|_| Response::Unit),
            Operation::ExportAudit => service.export_audit(token).map(Response::AuditExport),
            Operation::Acceptance { run_id } => service
                .acceptance(token, run_id)
                .map(|m| Response::Acceptance(Box::new(m))),
            Operation::AssignmentForPrincipal {
                assignment_id,
                principal,
            } => service
                .assignment_bound(token, assignment_id, Some(principal))
                .map(|t| Response::Assignment(Box::new(t))),
            Operation::EffectAssignmentForPrincipal {
                assignment_id,
                principal,
            } => service
                .effect_assignment_bound(token, assignment_id, Some(principal))
                .map(|a| Response::EffectAssignment(Box::new(a))),
            Operation::PlanStorageUpgrade { run_id } => service
                .plan_storage_upgrade(token, run_id)
                .map(Response::StorageUpgrade),
            Operation::UpgradeStorage { run_id, plan } => service
                .upgrade_storage(token, run_id, plan)
                .map(Response::StorageUpgrade),
            Operation::PlanMigration { run_id, request } => service
                .plan_migration(token, run_id, request)
                .map(|p| Response::MigrationPlan(Box::new(p))),
            Operation::MigrateDefinition { lease, plan } => service
                .migrate_definition(token, lease, plan)
                .map(|r| Response::Committed(Box::new(r))),
            Operation::HistoricalSnapshot { run_id, revision } => service
                .historical_snapshot(token, run_id, *revision)
                .map(|s| Response::Snapshot(Box::new(s))),
            Operation::RecoveryBarrier { run_id } => service
                .recovery_barrier(token, run_id)
                .map(|r| Response::RecoveryBarrier(r.map(Box::new))),
            Operation::ImportRestoredEffect { lease, request } => service
                .import_restored_effect(token, lease, request)
                .map(|r| Response::Committed(Box::new(r))),
            Operation::Control { request } => service
                .control(token, request)
                .map(|r| Response::Committed(Box::new(r))),
            Operation::DispatchEffect { lease, worker_id } => service
                .dispatch_effect(token, lease, worker_id)
                .map(Response::EffectDispatch),
            Operation::PendingEffects { after, limit } => service
                .pending_effects(token, after, *limit)
                .map(Response::Pending),
            Operation::EffectAssignment { assignment_id } => service
                .effect_assignment(token, assignment_id)
                .map(|a| Response::EffectAssignment(Box::new(a))),
            Operation::ObserveEffect {
                assignment_id,
                observation,
            } => service
                .observe_assigned_effect(token, assignment_id, observation)
                .map(Response::Finished),
            Operation::Effects {
                run_id,
                after,
                limit,
            } => service
                .effects(token, run_id, *after, *limit)
                .map(Response::Effects),
            Operation::ResolveEffect {
                lease,
                operation_key,
                resolution,
            } => service
                .resolve_effect(token, lease, operation_key, resolution)
                .map(Response::Finished),
            Operation::OutstandingEffects { after, limit } => service
                .outstanding_effects(token, after, *limit)
                .map(Response::OutstandingEffects),
            Operation::ValidateDefinition {
                source,
                format,
                file,
            } => service
                .validate_definition(token, source, *format, file)
                .map(Response::Validation),
            Operation::ArtifactBegin { request } => service
                .begin_artifact_upload(token, request)
                .map(|r| Response::ArtifactUpload(Box::new(r))),
            Operation::ArtifactPut {
                upload_id,
                offset,
                content,
            } => service
                .put_artifact_chunk(token, upload_id, *offset, content)
                .map(|r| Response::ArtifactUpload(Box::new(r))),
            Operation::ArtifactComplete { upload_id } => service
                .complete_artifact_upload(token, upload_id)
                .map(|r| Response::Artifact(Box::new(r))),
            Operation::ArtifactGrant {
                artifact,
                assignment_id,
                ttl_ms,
            } => service
                .grant_artifact_download(token, artifact, assignment_id.as_deref(), *ttl_ms)
                .map(|r| Response::ArtifactGrant(Box::new(r))),
            Operation::ArtifactGet {
                download_id,
                offset,
            } => service
                .artifact_download_chunk(token, download_id, *offset)
                .map(Response::ArtifactChunk),
            Operation::ArtifactCleanup { limit } => service
                .cleanup_artifact_transfers(token, *limit)
                .map(Response::ArtifactCleanup),
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
            Operation::Signal { request } => service
                .signal(token, request)
                .map(|r| Response::SignalReceived(Box::new(r))),
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
            (
                Operation::PlanStorageUpgrade { .. } | Operation::UpgradeStorage { .. },
                Response::StorageUpgrade(_)
            ) | (Operation::PlanMigration { .. }, Response::MigrationPlan(_))
                | (Operation::MigrateDefinition { .. }, Response::Committed(_))
                | (Operation::HistoricalSnapshot { .. }, Response::Snapshot(_))
                | (
                    Operation::RecoveryBarrier { .. },
                    Response::RecoveryBarrier(_)
                )
                | (
                    Operation::ImportRestoredEffect { .. },
                    Response::Committed(_)
                )
                | (Operation::Control { .. }, Response::Committed(_))
                | (
                    Operation::DispatchEffect { .. },
                    Response::EffectDispatch(_)
                )
                | (Operation::PendingEffects { .. }, Response::Pending(_))
                | (
                    Operation::EffectAssignment { .. },
                    Response::EffectAssignment(_)
                )
                | (Operation::ObserveEffect { .. }, Response::Finished(_))
                | (Operation::Effects { .. }, Response::Effects(_))
                | (Operation::ResolveEffect { .. }, Response::Finished(_))
                | (
                    Operation::OutstandingEffects { .. },
                    Response::OutstandingEffects(_)
                )
                | (
                    Operation::ValidateDefinition { .. },
                    Response::Validation(_)
                )
                | (Operation::ArtifactBegin { .. }, Response::ArtifactUpload(_))
                | (Operation::ArtifactPut { .. }, Response::ArtifactUpload(_))
                | (Operation::ArtifactComplete { .. }, Response::Artifact(_))
                | (Operation::ArtifactGrant { .. }, Response::ArtifactGrant(_))
                | (Operation::ArtifactGet { .. }, Response::ArtifactChunk(_))
                | (
                    Operation::ArtifactCleanup { .. },
                    Response::ArtifactCleanup(_)
                )
                | (Operation::Publish { .. }, Response::Published(_))
                | (
                    Operation::TemplatePropose { .. },
                    Response::TemplateProposed(_)
                )
                | (
                    Operation::TemplateCandidate { .. },
                    Response::TemplateCandidate(_)
                )
                | (
                    Operation::TemplateReview { .. },
                    Response::TemplateReviewed(_)
                )
                | (
                    Operation::TemplatePublish { .. } | Operation::TemplateGet { .. },
                    Response::TemplatePublication(_)
                )
                | (Operation::Start { .. }, Response::Committed(_))
                | (Operation::Get { .. }, Response::Snapshot(_))
                | (Operation::Acceptance { .. }, Response::Acceptance(_))
                | (Operation::History { .. }, Response::History(_))
                | (Operation::Waits { .. }, Response::Waits(_))
                | (Operation::Inbox { .. }, Response::Inbox(_))
                | (Operation::Acquire { .. }, Response::Lease(_))
                | (Operation::Renew { .. }, Response::Lease(_))
                | (
                    Operation::WorkerHeartbeat { .. }
                        | Operation::WorkerStatus { .. }
                        | Operation::ControlWorker { .. },
                    Response::WorkerStatus(_)
                )
                | (
                    Operation::SetPriority { .. } | Operation::ResolveDeadLetter { .. },
                    Response::Unit
                )
                | (
                    Operation::ScheduleCandidates { .. },
                    Response::Candidates(_)
                )
                | (
                    Operation::DispatchRouted { .. },
                    Response::RoutedDispatch(_)
                )
                | (Operation::DeadLetters { .. }, Response::DeadLetters(_))
                | (Operation::DeadLetter { .. }, Response::DeadLetter(_))
                | (Operation::Release { .. }, Response::Unit)
                | (Operation::Tick { .. }, Response::Tick(_))
                | (Operation::Dispatch { .. }, Response::Dispatch(_))
                | (Operation::Assignment { .. }, Response::Assignment(_))
                | (Operation::Finish { .. }, Response::Finished(_))
                | (Operation::Fail { .. }, Response::Unit)
                | (Operation::Approve { .. }, Response::Approved(_))
                | (Operation::Signal { .. }, Response::SignalReceived(_))
                | (Operation::Revoke { .. }, Response::Unit)
                | (Operation::Audit { .. }, Response::Audit(_))
                | (Operation::ExportAudit, Response::AuditExport(_))
                | (
                    Operation::AssignmentForPrincipal { .. },
                    Response::Assignment(_)
                )
                | (
                    Operation::EffectAssignmentForPrincipal { .. },
                    Response::EffectAssignment(_)
                )
                | (Operation::Outstanding { .. }, Response::Outstanding(_))
                | (Operation::Pending { .. }, Response::Pending(_))
                | (Operation::Runs { .. }, Response::Runs(_))
                | (Operation::AcknowledgeRecovery { .. }, Response::Recovery(_))
        )
    }
}
