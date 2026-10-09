//! Local read-only execution over a durable, fenced host port. No SQLite/provider dependency.
use serde::Serialize;
use workflow_runstore::{Claimed, ExecutionStore, LeaseRequest, Snapshot};
use workflow_worker::{Clock, Worker};

/// Host task execution, including any attempt workspace and retained evidence.
/// Storage still owns claim/lease/result validation and durable completion.
pub trait TaskExecutor {
    fn start_assigned_task(
        &self,
        _assignment_id: &str,
        request: &workflow_worker::WorkRequest,
        grant: &workflow_worker::ExecutionGrant,
    ) -> workflow_worker::Result<Option<Box<dyn RunningTask>>> {
        self.start_task(request, grant)
    }

    /// Return a cancellable operation when the host can run work independently.
    /// Legacy in-process adapters remain cooperative and synchronous.
    fn start_task(
        &self,
        _request: &workflow_worker::WorkRequest,
        _grant: &workflow_worker::ExecutionGrant,
    ) -> workflow_worker::Result<Option<Box<dyn RunningTask>>> {
        Ok(None)
    }
    fn execute_task(
        &self,
        request: &workflow_worker::WorkRequest,
        grant: &workflow_worker::ExecutionGrant,
        clock: &dyn Clock,
    ) -> workflow_worker::Result<workflow_worker::WorkResult>;
}
/// A host-owned operation. Dropping it must cancel and reap any child processes.
pub trait RunningTask {
    fn poll(&mut self) -> workflow_worker::Result<Option<workflow_worker::WorkResult>>;
    fn cancel(&mut self) -> workflow_worker::Result<()>;
}
impl TaskExecutor for Worker {
    fn execute_task(
        &self,
        request: &workflow_worker::WorkRequest,
        grant: &workflow_worker::ExecutionGrant,
        clock: &dyn Clock,
    ) -> workflow_worker::Result<workflow_worker::WorkResult> {
        Ok(self
            .execute_with_clock(request, grant, clock)?
            .into_result())
    }
}

#[derive(Clone, Debug)]
pub struct DriveOptions {
    pub owner: String,
    pub acquisition_id: String,
    pub lease_ms: u64,
    pub max_commands: u32,
}
#[derive(Clone, Debug, Serialize)]
pub struct DriveReport {
    pub snapshot: Snapshot,
    pub processed_commands: u32,
    pub executed_tasks: u32,
    pub effect_calls: u32,
    pub timer_transitions: u32,
    pub stop_reason: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Error {
    pub message: String,
    pub storage: Option<workflow_runstore::Error>,
    pub worker: Option<workflow_worker::Error>,
    pub release_error: Option<workflow_runstore::Error>,
}
impl From<workflow_runstore::Error> for Error {
    fn from(e: workflow_runstore::Error) -> Self {
        Self {
            message: e.message.clone(),
            storage: Some(e),
            worker: None,
            release_error: None,
        }
    }
}
impl From<workflow_worker::Error> for Error {
    fn from(e: workflow_worker::Error) -> Self {
        Self {
            message: e.message.clone(),
            storage: None,
            worker: Some(e),
            release_error: None,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}

/// The host clock is sampled inside storage transactions. A synchronous adapter
/// must cooperate with its deadline; expiry fences results but cannot kill code.
pub fn drive(
    store: &mut impl ExecutionStore,
    worker: &impl TaskExecutor,
    run_id: &str,
    options: &DriveOptions,
    clock: &impl Clock,
) -> Result<DriveReport, Error> {
    drive_inner(store, worker, run_id, options, clock, |_, _, _| {
        Ok(EffectProgress::Skip)
    })
}

/// Calls adapters only after the durable intent commits. A crash or transport
/// failure leaves an unknown effect for query/deduplication recovery.
pub fn drive_with_effects(
    store: &mut impl workflow_runstore::EffectStore,
    worker: &impl TaskExecutor,
    effects: &impl workflow_effects::EffectAdapter,
    run_id: &str,
    options: &DriveOptions,
    clock: &impl Clock,
) -> Result<DriveReport, Error> {
    use workflow_effects::Observation;
    use workflow_runstore::EffectClaim;
    drive_inner(store, worker, run_id, options, clock, |s, lease, clock| {
        Ok(match s.claim_effect(lease, clock)? {
            EffectClaim::Idle => EffectProgress::Skip,
            EffectClaim::Handled => EffectProgress::Handled,
            EffectClaim::Waiting { .. } => EffectProgress::Stop("effect_backoff"),
            EffectClaim::Manual { .. } => EffectProgress::Stop("effect_uncertain"),
            EffectClaim::Call { attempt } => {
                s.validate_effect_dispatch(&attempt, clock)?;
                let observed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    effects.execute(&attempt, clock)
                })) {
                    Ok(Ok(value)) => value,
                    // Provider errors may include sensitive response bytes. Keep
                    // stable, bounded reasons; adapter diagnostics stay host-side.
                    Ok(Err(_)) => Observation::Unknown {
                        reason: "effect adapter could not return a verified observation".into(),
                    },
                    Err(_) => Observation::Unknown {
                        reason: "effect adapter panicked after dispatch admission".into(),
                    },
                };
                s.observe_effect(lease, &attempt.attempt_id, &observed, clock)?;
                EffectProgress::Called
            }
        })
    })
}
enum EffectProgress {
    Skip,
    Handled,
    Called,
    Stop(&'static str),
}
fn drive_inner<S: ExecutionStore>(
    store: &mut S,
    worker: &impl TaskExecutor,
    run_id: &str,
    options: &DriveOptions,
    clock: &impl Clock,
    effect: impl Fn(&mut S, &workflow_runstore::Lease, &dyn Clock) -> Result<EffectProgress, Error>,
) -> Result<DriveReport, Error> {
    if !(1..=100).contains(&options.max_commands) {
        return Err(workflow_runstore::Error::new(
            workflow_runstore::ErrorCode::InvalidRequest,
            "drive command budget must be 1..100",
        )
        .into());
    }
    let mut lease = store.acquire(
        &LeaseRequest {
            run_id: run_id.into(),
            owner: options.owner.clone(),
            acquisition_id: options.acquisition_id.clone(),
            ttl_ms: options.lease_ms,
        },
        clock,
    )?;
    let outcome = (|| {
        let mut processed = 0;
        let mut executed = 0;
        let mut timers = 0;
        let mut effect_calls = 0;
        let mut stop = "budget";
        while processed < options.max_commands {
            let now = clock.now_unix_ms()?;
            if now.saturating_add(options.lease_ms / 2) >= lease.expires_at_unix_ms {
                lease = store.renew(&lease, options.lease_ms, clock)?;
            }
            if store.tick_due(&lease, clock)?.is_some() {
                timers += 1;
            }
            match effect(store, &lease, clock)? {
                EffectProgress::Skip => {}
                EffectProgress::Handled => {
                    processed += 1;
                    continue;
                }
                EffectProgress::Called => {
                    processed += 1;
                    effect_calls += 1;
                    continue;
                }
                EffectProgress::Stop(reason) => {
                    stop = reason;
                    break;
                }
            }
            match store.claim_next(&lease, clock)? {
                Claimed::Idle => {
                    stop = "idle";
                    break;
                }
                Claimed::Handled { .. } => {
                    processed += 1;
                }
                Claimed::Task { attempt } => {
                    let execution =
                        execute_owned(store, worker, &attempt, &mut lease, options, clock);
                    let result = match execution {
                        Ok(Some(accepted)) => accepted,
                        Ok(None) => {
                            processed += 1;
                            continue;
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    };
                    store.finish_task(&lease, &attempt.attempt_id, &result, clock)?;
                    processed += 1;
                    executed += 1;
                }
            }
        }
        let snapshot = store.get(run_id)?;
        if snapshot.pause.is_some() {
            stop = "paused";
        }
        Ok(DriveReport {
            snapshot,
            processed_commands: processed,
            executed_tasks: executed,
            timer_transitions: timers,
            effect_calls,
            stop_reason: stop.into(),
        })
    })();
    let released = store.release(&lease, clock);
    match (outcome, released) {
        (Ok(report), Ok(())) => Ok(report),
        (Ok(_), Err(e)) => Err(Error {
            message: format!("drive finished but lease release failed: {}", e.message),
            storage: None,
            worker: None,
            release_error: Some(e),
        }),
        (Err(mut e), release) => {
            e.release_error = release.err();
            Err(e)
        }
    }
}
// Ownership stays on this thread; a running operation never receives a store handle.
fn execute_owned(
    store: &mut impl ExecutionStore,
    worker: &impl TaskExecutor,
    attempt: &workflow_runstore::PreparedTask,
    lease: &mut workflow_runstore::Lease,
    options: &DriveOptions,
    clock: &impl Clock,
) -> Result<Option<workflow_worker::WorkResult>, Error> {
    let outcome = (|| {
        let Some(mut running) = worker.start_task(&attempt.request, &attempt.grant)? else {
            return worker
                .execute_task(&attempt.request, &attempt.grant, clock)
                .map(Some)
                .map_err(Error::from);
        };
        let mut observed = 0;
        loop {
            let now = clock.now_unix_ms()?;
            if now >= attempt.request.deadline_unix_ms {
                running.cancel()?;
                return Err(workflow_worker::Error::new(
                    workflow_worker::ErrorCode::DeadlineExceeded,
                    "activity deadline elapsed",
                )
                .into());
            }
            if now.saturating_add(options.lease_ms / 2) >= lease.expires_at_unix_ms {
                *lease = store.renew(lease, options.lease_ms, clock)?;
                store.progress(lease, &attempt.attempt_id, clock)?;
            }
            if now.saturating_sub(observed) >= 250 || observed == 0 {
                store.tick_due(lease, clock)?;
                let snapshot = store.get(&lease.run_id)?;
                let cancelled = match &attempt.request.scope {
                    workflow_worker::InvocationScope::Workflow {
                        node_instance_id, ..
                    } => snapshot
                        .frames
                        .values()
                        .flat_map(|f| f.nodes.values())
                        .any(|n| {
                            format!("instance-{}", n.instance_id) == *node_instance_id
                                && n.cancel_requested
                        }),
                    _ => false,
                };
                if cancelled {
                    running.cancel()?;
                    store.fail_task(
                        lease,
                        &attempt.attempt_id,
                        &workflow_worker::Error::new(
                            workflow_worker::ErrorCode::Expired,
                            "activity cancelled by workflow",
                        ),
                        clock,
                    )?;
                    return Ok(None);
                }
                observed = now;
            }
            if let Some(result) = running.poll()? {
                return Ok(Some(result));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    })();
    match outcome {
        Err(mut error) => {
            if let Some(e) = &error.worker
                && let Err(storage) = store.fail_task(lease, &attempt.attempt_id, e, clock)
            {
                error.storage = Some(storage);
            }
            Err(error)
        }
        other => other,
    }
}
#[cfg(test)]
mod tests;
