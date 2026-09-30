use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoutedDispatch {
    Task { assignment_id: String },
    Effect { assignment_id: String },
    Handled,
    Idle,
    Deferred,
    Waiting { not_before_unix_ms: u64 },
    Manual { operation_key: String },
    Parked { letter: DeadLetter },
}
fn candidate(
    tx: &mut Transaction<'_>,
    who: &Identity,
    lease: &Lease,
    worker: &str,
    with_effects: bool,
) -> Result<RoutedDispatch> {
    if with_effects {
        match effects::dispatch_in(tx, who, lease, worker)? {
            EffectDispatch::Call { assignment_id } => {
                return Ok(RoutedDispatch::Effect { assignment_id });
            }
            EffectDispatch::Handled => return Ok(RoutedDispatch::Handled),
            EffectDispatch::Waiting { not_before_unix_ms } => {
                return Ok(RoutedDispatch::Waiting { not_before_unix_ms });
            }
            EffectDispatch::Manual { operation_key } => {
                return Ok(RoutedDispatch::Manual { operation_key });
            }
            EffectDispatch::Idle => {}
        }
    }
    Ok(match tasks::dispatch_in(tx, who, lease, worker)? {
        Dispatch::Task { assignment_id } => RoutedDispatch::Task { assignment_id },
        Dispatch::Handled => RoutedDispatch::Handled,
        Dispatch::Idle => RoutedDispatch::Idle,
    })
}
impl AuthenticatedService {
    /// Route on exact capability/model/effect contracts inside the authority
    /// transaction. Wrong candidates leave no speculative attempt behind.
    pub fn dispatch_routed(
        &mut self,
        token: &str,
        lease: &Lease,
        workers: &[String],
        with_effects: bool,
    ) -> Result<RoutedDispatch> {
        self.transact(
            token,
            &[Role::Scheduler],
            "dispatch_routed",
            &lease.run_id,
            |tx, who| {
                configuration_guard(tx, &who.tenant)?;
                required(tx, who)?;
                if lease.owner != who.id {
                    return Err(denied());
                }
                if workers.is_empty()
                    || workers.len() > 32
                    || workers.iter().collect::<BTreeSet<_>>().len() != workers.len()
                {
                    return Err(invalid_policy());
                }
                for worker in workers {
                    validate_id(worker)?;
                }
                if let Some(letter) = control::active_letter(tx, who, &lease.run_id)? {
                    return Ok(RoutedDispatch::Parked { letter });
                }
                let mut deferred = false;
                for worker in workers {
                    let bounds = (
                        who.not_before.get(),
                        who.deadline.get(),
                        who.execution_not_before.get(),
                        who.execution_deadline.get(),
                    );
                    let mut save = tx.savepoint("route_candidate").map_err(storage)?;
                    match candidate(&mut save, who, lease, worker, with_effects) {
                        Ok(result) => {
                            save.commit().map_err(storage)?;
                            return Ok(result);
                        }
                        Err(e) => {
                            save.rollback().map_err(storage)?;
                            // SQL rollback cannot restore in-memory admission cells.
                            // Only failed, uncommitted candidate restrictions are reset.
                            who.not_before.set(bounds.0);
                            who.deadline.set(bounds.1);
                            who.execution_not_before.set(bounds.2);
                            who.execution_deadline.set(bounds.3);
                            match e.code {
                                ErrorCode::Unauthorized => {}
                                ErrorCode::Busy => deferred = true,
                                ErrorCode::AttemptInProgress => {
                                    return Ok(RoutedDispatch::Deferred);
                                }
                                _ => return Err(e),
                            }
                        }
                    }
                }
                if deferred {
                    return Ok(RoutedDispatch::Deferred);
                }
                // Even a completely invalid worker list cannot park a run without
                // current ownership. Database time, not a caller timestamp, decides.
                let at = now(tx)? as u64;
                who.read(tx, &lease.run_id, |s| {
                    effects::authority(s, &lease.run_id)?.check_live(lease, at)
                })?;
                who.fence_execution(
                    lease.issued_at_unix_ms as i64,
                    lease.expires_at_unix_ms as i64,
                );
                let letter = control::park(tx, who, &lease.run_id, "no_compatible_worker")?
                    .ok_or_else(invalid_policy)?;
                Ok(RoutedDispatch::Parked { letter })
            },
        )
    }
}
