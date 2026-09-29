use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use workflow_runstore::{Lease, RunStatus, validate_id};
use workflow_runstore_postgres::access::{Dispatch, EffectDispatch};

fn call(client: &RemoteClient, operation: Operation) -> Result<Response> {
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
    client: &RemoteClient,
    effects: &impl workflow_effects::EffectAdapter,
    limit: u32,
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
            Operation::EffectAssignment {
                assignment_id: assignment_id.clone(),
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
    client: &RemoteClient,
    worker: &workflow_worker::Worker,
    limit: u32,
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
            Operation::Assignment {
                assignment_id: assignment_id.clone(),
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
        let submission = match worker.execute(&task.request, &task.grant) {
            Ok(result) => Operation::Finish {
                assignment_id,
                result: Box::new(result.into_result()),
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

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct ScheduleReport {
    pub scanned: u32,
    pub acquired: u32,
    pub dispatched: u32,
    pub busy: u32,
    pub fenced: u32,
    pub effect_waiting: u32,
    pub effect_manual: u32,
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
        })
    }
    /// Explicit host opt-in; worker effect policies still authorize each target.
    pub fn with_effects(mut self) -> Self {
        self.effects = true;
        self
    }
    pub fn step(
        &mut self,
        client: &RemoteClient,
        ttl_ms: u64,
        limit: u32,
    ) -> Result<ScheduleReport> {
        workflow_runstore::validate_limit(limit)?;
        if !(1..=workflow_runstore::MAX_LEASE_MS).contains(&ttl_ms) {
            return Err(invalid());
        }
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
        // Local time only discards hints; server time still decides every lease.
        let local_now = workflow_worker::Clock::now_unix_ms(&workflow_worker::SystemClock)?;
        self.leases.retain(|_, l| l.expires_at_unix_ms > local_now);
        let mut report = ScheduleReport::default();
        for run in page.items {
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
            let lease = self.leases[&run.run_id].clone();
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
