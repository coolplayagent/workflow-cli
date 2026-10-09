use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use workflow_runstore::{Lease, RunStatus, validate_id};
use workflow_runstore_postgres::access::{Dispatch, EffectDispatch, RoutedDispatch};

fn call(client: &(impl TaskTransport + ?Sized), operation: Operation) -> Result<Response> {
    client.call(&Request {
        protocol_version: PROTOCOL_VERSION,
        request_id: "reconcile".into(),
        operation,
    })
}
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct WorkerReport {
    pub completed: u32,
    pub fenced: u32,
    pub failed: u32,
}
/// Effect delivery is single-use. Never retry retrieval or I/O after an unknown
/// transport outcome; the durable ledger will query under a successor lease.
pub fn work_effects_once(
    client: &(impl TaskTransport + ?Sized),
    effects: &impl workflow_effects::EffectAdapter,
    limit: u32,
) -> Result<WorkerReport> {
    work_effects_once_bound(client, effects, limit, None)
}
pub fn work_effects_once_bound(
    client: &(impl TaskTransport + ?Sized),
    effects: &impl workflow_effects::EffectAdapter,
    limit: u32,
    principal: Option<&workflow_credentials::Principal>,
) -> Result<WorkerReport> {
    workflow_runstore::validate_limit(limit)?;
    let Response::Pending(page) = call(
        client,
        Operation::PendingEffects {
            after: String::new(),
            limit,
        },
    )?
    else {
        return Err(invalid());
    };
    let mut report = WorkerReport::default();
    for assignment_id in page.items {
        let attempt = match call(
            client,
            match principal {
                Some(principal) => Operation::EffectAssignmentForPrincipal {
                    assignment_id: assignment_id.clone(),
                    principal: principal.clone(),
                },
                None => Operation::EffectAssignment {
                    assignment_id: assignment_id.clone(),
                },
            },
        ) {
            Ok(Response::EffectAssignment(attempt)) => attempt,
            Err(e)
                if matches!(
                    e.code,
                    ErrorCode::LeaseConflict
                        | ErrorCode::ReceiptConflict
                        | ErrorCode::TransitionRejected
                ) =>
            {
                report.fenced += 1;
                continue;
            }
            Err(e) => return Err(e),
            _ => return Err(invalid()),
        };
        let observation = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            effects.execute(&attempt, &workflow_worker::SystemClock)
        })) {
            Ok(Ok(value)) => value,
            _ => workflow_effects::Observation::Unknown {
                reason: "effect adapter returned no verified observation".into(),
            },
        };
        match call(
            client,
            Operation::ObserveEffect {
                assignment_id,
                observation: Box::new(observation),
            },
        ) {
            Ok(Response::Finished(_)) => report.completed += 1,
            Err(e)
                if matches!(
                    e.code,
                    ErrorCode::LeaseConflict | ErrorCode::ReceiptConflict
                ) =>
            {
                report.fenced += 1
            }
            Err(e) => return Err(e),
            _ => return Err(invalid()),
        }
    }
    Ok(report)
}
/// One bounded scan of this credential's durable assignments. Execution uses the
/// same Worker contract as local runtime, while result authority remains remote.
pub fn work_once(
    client: &(impl TaskTransport + ?Sized),
    worker: &impl workflow_runtime::TaskExecutor,
    limit: u32,
) -> Result<WorkerReport> {
    work_once_bound(client, worker, limit, None)
}
pub fn work_once_bound(
    client: &(impl TaskTransport + ?Sized),
    worker: &impl workflow_runtime::TaskExecutor,
    limit: u32,
    principal: Option<&workflow_credentials::Principal>,
) -> Result<WorkerReport> {
    workflow_runstore::validate_limit(limit)?;
    let Response::Pending(page) = call(
        client,
        Operation::Pending {
            after: String::new(),
            limit,
        },
    )?
    else {
        return Err(invalid());
    };
    let mut report = WorkerReport::default();
    for assignment_id in page.items {
        let task = match call(
            client,
            match principal {
                Some(principal) => Operation::AssignmentForPrincipal {
                    assignment_id: assignment_id.clone(),
                    principal: principal.clone(),
                },
                None => Operation::Assignment {
                    assignment_id: assignment_id.clone(),
                },
            },
        ) {
            Ok(Response::Assignment(task)) => task,
            Err(e)
                if matches!(
                    e.code,
                    ErrorCode::LeaseConflict | ErrorCode::ReceiptConflict
                ) =>
            {
                report.fenced += 1;
                continue;
            }
            Err(e) => return Err(e),
            _ => return Err(invalid()),
        };
        let submission = match execute_assignment(client, worker, &assignment_id, &task) {
            Ok(result) => Operation::Finish {
                assignment_id,
                result: Box::new(result),
            },
            Err(e) => Operation::Fail {
                assignment_id,
                error: workflow_worker::Error::new(
                    e.code,
                    "worker could not execute assigned contract",
                ),
            },
        };
        match call(client, submission) {
            Ok(Response::Finished(_)) => report.completed += 1,
            Ok(Response::Unit) => report.failed += 1,
            Err(e)
                if matches!(
                    e.code,
                    ErrorCode::LeaseConflict | ErrorCode::ReceiptConflict
                ) =>
            {
                report.fenced += 1
            }
            Err(e) => return Err(e),
            _ => return Err(invalid()),
        }
    }
    Ok(report)
}

fn execute_assignment(
    client: &(impl TaskTransport + ?Sized),
    worker: &impl workflow_runtime::TaskExecutor,
    assignment_id: &str,
    task: &workflow_runstore::PreparedTask,
) -> workflow_worker::Result<workflow_worker::WorkResult> {
    use workflow_worker::Clock;
    let Some(mut running) =
        worker.start_assigned_task(assignment_id, &task.request, &task.grant)?
    else {
        return worker.execute_task(&task.request, &task.grant, &workflow_worker::SystemClock);
    };
    let mut last_probe = None;
    let mut last_progress = None;
    loop {
        let now = workflow_worker::SystemClock.now_unix_ms()?;
        if now >= task.request.deadline_unix_ms {
            running.cancel()?;
            return Err(workflow_worker::Error::new(
                workflow_worker::ErrorCode::DeadlineExceeded,
                "shared activity deadline elapsed",
            ));
        }
        if last_probe.is_none_or(|at| now.saturating_sub(at) >= 1000) {
            let record = last_progress.is_none_or(|at| now.saturating_sub(at) >= 30000);
            match call(
                client,
                Operation::TaskProgress {
                    assignment_id: assignment_id.into(),
                    record,
                },
            ) {
                Ok(Response::TaskProgress { cancelled: false }) => {}
                _ => {
                    running.cancel()?;
                    return Err(workflow_worker::Error::new(
                        workflow_worker::ErrorCode::Expired,
                        "shared activity cancelled or ownership probe failed",
                    ));
                }
            }
            last_probe = Some(now);
            if record {
                last_progress = Some(now);
            }
        }
        if let Some(result) = running.poll()? {
            return Ok(result);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct ScheduleReport {
    pub scanned: u32,
    pub acquired: u32,
    pub dispatched: u32,
    pub busy: u32,
    pub fenced: u32,
    pub effect_waiting: u32,
    pub effect_manual: u32,
    pub renewed: u32,
    pub parked: u32,
}
/// A scheduler's volatile lease cache is only a convenience. Authority is always
/// rechecked in PostgreSQL. Losing this process permits another scheduler to
/// acquire expired leases; it does not remove durable commands or assignments.
pub struct Scheduler {
    leases: BTreeMap<String, Lease>,
    workers: Vec<String>,
    cursor: Option<String>,
    next_worker: usize,
    sequence: u64,
    id: String,
    effects: bool,
    cluster: bool,
}
impl Scheduler {
    pub fn new(id: &str, workers: Vec<String>) -> Result<Self> {
        validate_id(id)?;
        if id.len() > 64 || workers.is_empty() || workers.len() > 32 {
            return Err(invalid());
        }
        for w in &workers {
            validate_id(w)?;
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| unavailable())?;
        let suffix = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        Ok(Self {
            leases: BTreeMap::new(),
            workers,
            cursor: None,
            next_worker: 0,
            sequence: 0,
            id: format!("{id}.{suffix}"),
            effects: false,
            cluster: false,
        })
    }
    /// Explicit host opt-in; worker effect policies still authorize each target.
    pub fn with_effects(mut self) -> Self {
        self.effects = true;
        self
    }
    /// Requires an explicitly installed shared scheduling policy. Selection,
    /// quotas, routing and dead letters are then decided by the database.
    pub fn with_cluster_scheduling(mut self) -> Self {
        self.cluster = true;
        self
    }
    pub fn step(
        &mut self,
        client: &(impl TaskTransport + ?Sized),
        ttl_ms: u64,
        limit: u32,
    ) -> Result<ScheduleReport> {
        workflow_runstore::validate_limit(limit)?;
        if !(1..=workflow_runstore::MAX_LEASE_MS).contains(&ttl_ms) {
            return Err(invalid());
        }
        let items = if self.cluster {
            let Response::Candidates(items) =
                call(client, Operation::ScheduleCandidates { limit })?
            else {
                return Err(invalid());
            };
            items
        } else {
            let Response::Runs(page) = call(
                client,
                Operation::Runs {
                    after: self.cursor.clone(),
                    limit,
                },
            )?
            else {
                return Err(invalid());
            };
            self.cursor = page.next_cursor;
            page.items
        };
        // Local time only discards hints; server time still decides every lease.
        let local_now = workflow_worker::Clock::now_unix_ms(&workflow_worker::SystemClock)?;
        self.leases.retain(|_, l| l.expires_at_unix_ms > local_now);
        let mut report = ScheduleReport::default();
        for run in items {
            report.scanned += 1;
            if !matches!(run.status, RunStatus::Running | RunStatus::Cancelling) {
                if let Some(lease) = self.leases.remove(&run.run_id) {
                    match call(client, Operation::Release { lease }) {
                        Ok(_) => {}
                        Err(e) if e.code == ErrorCode::LeaseConflict => {}
                        Err(e) => return Err(e),
                    }
                }
                continue;
            }
            if run.pause.is_some() {
                continue;
            }
            if !self.leases.contains_key(&run.run_id) {
                // Bound local bookkeeping without deleting durable assignments.
                if self.leases.len() >= 1000 {
                    report.busy += 1;
                    continue;
                }
                self.sequence = self.sequence.checked_add(1).ok_or_else(invalid)?;
                let acquisition_id = format!("{}.{}", self.id, self.sequence);
                match call(
                    client,
                    Operation::Acquire {
                        run_id: run.run_id.clone(),
                        acquisition_id,
                        ttl_ms,
                    },
                ) {
                    Ok(Response::Lease(lease)) => {
                        self.leases.insert(run.run_id.clone(), lease);
                        report.acquired += 1;
                    }
                    Err(e) if e.code == ErrorCode::LeaseBusy => {
                        report.busy += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                    _ => return Err(invalid()),
                }
            }
            let mut lease = self.leases[&run.run_id].clone();
            let at = workflow_worker::Clock::now_unix_ms(&workflow_worker::SystemClock)?;
            if self.cluster && lease.expires_at_unix_ms.saturating_sub(at) < ttl_ms / 2 {
                match call(
                    client,
                    Operation::Renew {
                        lease: lease.clone(),
                        ttl_ms,
                    },
                ) {
                    Ok(Response::Lease(renewed)) => {
                        lease = renewed;
                        self.leases.insert(run.run_id.clone(), lease.clone());
                        report.renewed += 1;
                    }
                    Err(e) if e.code == ErrorCode::LeaseConflict => {
                        self.leases.remove(&run.run_id);
                        report.fenced += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                    _ => return Err(invalid()),
                }
            }
            match call(
                client,
                Operation::Tick {
                    lease: lease.clone(),
                },
            ) {
                Ok(Response::Tick(_)) => {}
                Err(e) if e.code == ErrorCode::LeaseConflict => {
                    self.leases.remove(&run.run_id);
                    report.fenced += 1;
                    continue;
                }
                Err(e) => return Err(e),
                _ => return Err(invalid()),
            }
            if self.cluster {
                let mut worker_ids = self.workers.clone();
                let offset = self.next_worker % worker_ids.len();
                worker_ids.rotate_left(offset);
                self.next_worker = self.next_worker.wrapping_add(1);
                let release = match call(
                    client,
                    Operation::DispatchRouted {
                        lease: lease.clone(),
                        worker_ids,
                        effects: self.effects,
                    },
                ) {
                    Ok(Response::RoutedDispatch(result)) => match result {
                        RoutedDispatch::Task { .. } | RoutedDispatch::Effect { .. } => {
                            report.dispatched += 1;
                            false
                        }
                        RoutedDispatch::Handled => false,
                        RoutedDispatch::Deferred => {
                            report.busy += 1;
                            false
                        }
                        RoutedDispatch::Waiting { .. } => {
                            report.effect_waiting += 1;
                            false
                        }
                        RoutedDispatch::Manual { .. } => {
                            report.effect_manual += 1;
                            true
                        }
                        RoutedDispatch::Parked { .. } => {
                            report.parked += 1;
                            true
                        }
                        RoutedDispatch::Idle => true,
                    },
                    Err(e) if e.code == ErrorCode::LeaseConflict => {
                        self.leases.remove(&run.run_id);
                        report.fenced += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                    _ => return Err(invalid()),
                };
                if release {
                    match call(client, Operation::Release { lease }) {
                        Ok(Response::Unit) => {}
                        Err(e) if e.code == ErrorCode::LeaseConflict => {}
                        Err(e) => return Err(e),
                        _ => return Err(invalid()),
                    }
                    self.leases.remove(&run.run_id);
                }
                continue;
            }
            let worker_id = self.workers[self.next_worker % self.workers.len()].clone();
            self.next_worker = self.next_worker.wrapping_add(1);
            if self.effects {
                match call(
                    client,
                    Operation::DispatchEffect {
                        lease: lease.clone(),
                        worker_id: worker_id.clone(),
                    },
                ) {
                    Ok(Response::EffectDispatch(EffectDispatch::Call { .. })) => {
                        report.dispatched += 1;
                        continue;
                    }
                    Ok(Response::EffectDispatch(EffectDispatch::Handled)) => continue,
                    Ok(Response::EffectDispatch(EffectDispatch::Waiting { .. })) => {
                        report.effect_waiting += 1;
                        continue;
                    }
                    Ok(Response::EffectDispatch(EffectDispatch::Manual { .. })) => {
                        report.effect_manual += 1;
                        call(client, Operation::Release { lease })?;
                        self.leases.remove(&run.run_id);
                        continue;
                    }
                    Ok(Response::EffectDispatch(EffectDispatch::Idle)) => {}
                    Err(e) if e.code == ErrorCode::AttemptInProgress => {
                        report.busy += 1;
                        continue;
                    }
                    Err(e) if e.code == ErrorCode::LeaseConflict => {
                        self.leases.remove(&run.run_id);
                        report.fenced += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                    _ => return Err(invalid()),
                }
            }
            match call(
                client,
                Operation::Dispatch {
                    lease: lease.clone(),
                    worker_id,
                },
            ) {
                Ok(Response::Dispatch(Dispatch::Task { .. })) => report.dispatched += 1,
                Ok(Response::Dispatch(Dispatch::Handled)) => {}
                Ok(Response::Dispatch(Dispatch::Idle)) => {
                    call(client, Operation::Release { lease })?;
                    self.leases.remove(&run.run_id);
                }
                Err(e) if e.code == ErrorCode::AttemptInProgress => report.busy += 1,
                Err(e) if e.code == ErrorCode::LeaseConflict => {
                    self.leases.remove(&run.run_id);
                    report.fenced += 1;
                }
                Err(e) => return Err(e),
                _ => return Err(invalid()),
            }
        }
        Ok(report)
    }
}
