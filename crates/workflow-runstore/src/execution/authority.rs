use super::*;
use crate::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Authority {
    pub run_id: String,
    pub sequence: u64,
    pub lease: Option<Lease>,
    pub released: bool,
    pub last_now: u64,
    pub generation: Option<String>,
    pub recovery: Option<RecoveryBarrier>,
    pub effects: BTreeMap<String, workflow_effects::EffectState>,
    pub attempts: BTreeMap<String, AttemptState>,
    acquisitions: BTreeSet<String>,
    generations: BTreeSet<String>,
}
impl Authority {
    pub fn new(run_id: &str, started: u64) -> Self {
        Self {
            run_id: run_id.into(),
            sequence: 0,
            lease: None,
            released: true,
            last_now: started,
            generation: None,
            recovery: None,
            attempts: BTreeMap::new(),
            effects: BTreeMap::new(),
            acquisitions: BTreeSet::new(),
            generations: BTreeSet::new(),
        }
    }
    pub fn check_clock(&self, now: u64) -> Result<()> {
        if now < self.last_now || now == 0 {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "execution clock moved backwards",
            ));
        }
        Ok(())
    }
    pub fn check_live(&self, lease: &Lease, now: u64) -> Result<()> {
        self.check_clock(now)?;
        if self.lease.as_ref() != Some(lease) || self.released || now >= lease.expires_at_unix_ms {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "lease is expired, released or no longer current",
            ));
        }
        Ok(())
    }
    pub fn next_lease(&self, request: &LeaseRequest, now: u64) -> Result<Lease> {
        self.check_clock(now)?;
        for id in [&request.run_id, &request.owner, &request.acquisition_id] {
            validate_id(id)?;
        }
        if request.run_id != self.run_id || self.acquisitions.contains(&request.acquisition_id) {
            return Err(Error::new(
                ErrorCode::LeaseConflict,
                "run mismatch or acquisition ID already used",
            ));
        }
        if !self.released
            && self
                .lease
                .as_ref()
                .is_some_and(|l| now < l.expires_at_unix_ms)
        {
            return Err(Error::new(ErrorCode::LeaseBusy, "run has a live owner"));
        }
        let epoch = self.lease.as_ref().map_or(1, |l| l.epoch + 1);
        Ok(Lease {
            generation: self.generation.clone(),
            run_id: self.run_id.clone(),
            owner: request.owner.clone(),
            acquisition_id: request.acquisition_id.clone(),
            epoch,
            issued_at_unix_ms: now,
            expires_at_unix_ms: lease_deadline(now, request.ttl_ms)?,
        })
    }
    pub fn attempt_number(&self, command_id: &str) -> Result<u32> {
        let attempts: Vec<_> = self
            .attempts
            .values()
            .filter(|a| a.prepared.command_id == command_id)
            .collect();
        if attempts.iter().any(|a| {
            a.outcome.is_none()
                && self
                    .lease
                    .as_ref()
                    .is_some_and(|l| l.epoch == a.prepared.epoch)
        }) {
            return Err(Error::new(
                ErrorCode::AttemptInProgress,
                "current owner already prepared this command; do not invoke it twice",
            ));
        }
        if attempts.len() >= MAX_ATTEMPTS as usize {
            return Err(Error::new(
                ErrorCode::AttemptBudget,
                "read-only attempt budget exhausted",
            ));
        }
        Ok(attempts.len() as u32 + 1)
    }
    pub fn apply(&mut self, action: &ExecutionAction) -> Result<()> {
        if self.sequence >= MAX_EXECUTION_RECORDS {
            return Err(Error::new(
                ErrorCode::AttemptBudget,
                "execution journal budget exhausted",
            ));
        }
        let mut next = self.clone();
        next.reduce(action)?;
        next.sequence += 1;
        *self = next;
        Ok(())
    }
    fn current(&self, epoch: u64, now: u64) -> Result<()> {
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| Error::new(ErrorCode::LeaseConflict, "no owner"))?;
        if lease.epoch != epoch {
            return Err(Error::new(ErrorCode::LeaseConflict, "stale lease epoch"));
        }
        self.check_live(lease, now)
    }
    fn reduce(&mut self, action: &ExecutionAction) -> Result<()> {
        let now = match action {
            ExecutionAction::Migrated { migration } => {
                migration.validate(&self.run_id)?;
                self.current(migration.epoch, migration.at_unix_ms)?;
                if self.recovery.is_some() || !self.effects.is_empty() {
                    return Err(Error::new(
                        ErrorCode::MigrationBlocked,
                        "definition migration cannot replay a recovery barrier or retained business effects",
                    ));
                }
                if self.attempts.values().any(|a| {
                    a.outcome.is_none()
                        && a.prepared.epoch == migration.epoch
                        && a.prepared.request.deadline_unix_ms > migration.at_unix_ms
                }) {
                    return Err(Error::new(
                        ErrorCode::AttemptInProgress,
                        "drain live task attempts before definition migration",
                    ));
                }
                if !self.generations.insert(migration.generation.clone()) {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "migration generation was already used",
                    ));
                }
                self.generation = Some(migration.generation.clone());
                self.released = true;
                migration.at_unix_ms
            }
            ExecutionAction::Restored {
                recovery,
                at_unix_ms,
            } => {
                recovery.validate()?;
                if !self.generations.insert(recovery.generation.clone()) {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "restore must create a new ownership generation",
                    ));
                }
                self.generation = Some(recovery.generation.clone());
                self.recovery = Some(recovery.clone());
                self.released = true;
                *at_unix_ms
            }
            ExecutionAction::RecoveryAcknowledged {
                resolution,
                at_unix_ms,
            } => {
                resolution.validate()?;
                let recovery = self.recovery.as_ref().ok_or_else(|| {
                    Error::new(ErrorCode::RecoveryRequired, "no pending recovery barrier")
                })?;
                if resolution.generation != recovery.generation
                    || resolution.backup_digest != recovery.backup_digest
                {
                    return Err(Error::new(
                        ErrorCode::RecoveryRequired,
                        "recovery acknowledgement names another backup/generation",
                    ));
                }
                if self.effects.values().any(|s| {
                    !matches!(
                        s.status,
                        workflow_effects::EffectStatus::Applied { .. }
                            | workflow_effects::EffectStatus::Failed { .. }
                            | workflow_effects::EffectStatus::Cancelled
                    )
                }) {
                    return Err(Error::new(
                        ErrorCode::RecoveryRequired,
                        "recorded external effects still require reconciliation",
                    ));
                }
                self.recovery = None;
                *at_unix_ms
            }
            ExecutionAction::Effect { record, .. } => {
                self.current(record.epoch, record.at_unix_ms)?;
                if let workflow_effects::EffectChange::Imported { intent, .. } = &record.change
                    && (self.recovery.is_none() || intent.run_id != self.run_id)
                {
                    return Err(Error::new(
                        ErrorCode::RecoveryRequired,
                        "historical effect import requires a restored run under reconciliation",
                    ));
                }
                if self.recovery.is_some()
                    && matches!(&record.change, workflow_effects::EffectChange::Prepared { attempt, .. } if attempt.kind == workflow_effects::CallKind::Write)
                {
                    return Err(Error::new(
                        ErrorCode::RecoveryRequired,
                        "restored run cannot admit writes before external reconciliation",
                    ));
                }
                if let workflow_effects::EffectChange::Prepared { attempt, .. } = &record.change
                    && (attempt.intent.run_id != self.run_id
                        || attempt.deadline_unix_ms
                            > self.lease.as_ref().unwrap().expires_at_unix_ms)
                {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "effect attempt exceeds this run lease",
                    ));
                }
                workflow_effects::apply(&mut self.effects, record)?;
                record.at_unix_ms
            }
            ExecutionAction::Acquired { lease } => {
                let ttl = lease
                    .expires_at_unix_ms
                    .checked_sub(lease.issued_at_unix_ms)
                    .ok_or_else(|| {
                        Error::new(ErrorCode::InvalidRequest, "invalid lease deadline")
                    })?;
                let expected = self.next_lease(
                    &LeaseRequest {
                        run_id: lease.run_id.clone(),
                        owner: lease.owner.clone(),
                        acquisition_id: lease.acquisition_id.clone(),
                        ttl_ms: ttl,
                    },
                    lease.issued_at_unix_ms,
                )?;
                if &expected != lease {
                    return Err(Error::new(
                        ErrorCode::LeaseConflict,
                        "lease identity/epoch mismatch",
                    ));
                }
                self.acquisitions.insert(lease.acquisition_id.clone());
                self.lease = Some(lease.clone());
                self.released = false;
                lease.issued_at_unix_ms
            }
            ExecutionAction::Renewed { lease, at_unix_ms } => {
                self.current(lease.epoch, *at_unix_ms)?;
                let old = self.lease.as_ref().unwrap();
                let mut expected = old.clone();
                expected.expires_at_unix_ms = lease.expires_at_unix_ms;
                if &expected != lease
                    || lease.expires_at_unix_ms <= old.expires_at_unix_ms
                    || lease.expires_at_unix_ms > lease_deadline(*at_unix_ms, MAX_LEASE_MS)?
                {
                    return Err(Error::new(
                        ErrorCode::LeaseConflict,
                        "renewal must preserve identity and extend a bounded deadline",
                    ));
                }
                self.lease = Some(lease.clone());
                *at_unix_ms
            }
            ExecutionAction::Released { epoch, at_unix_ms } => {
                self.current(*epoch, *at_unix_ms)?;
                self.released = true;
                *at_unix_ms
            }
            ExecutionAction::Prepared { attempt } => {
                let now = attempt.request.issued_at_unix_ms;
                self.current(attempt.epoch, now)?;
                validate_id(&attempt.attempt_id)?;
                attempt.request.validate_shape()?;
                if attempt.number != self.attempt_number(&attempt.command_id)?
                    || self.attempts.contains_key(&attempt.attempt_id)
                    || attempt.prepared_revision == 0
                    || attempt.command_sequence == 0
                    || attempt.request.deadline_unix_ms
                        > self.lease.as_ref().unwrap().expires_at_unix_ms
                    || workflow_worker::ExecutionGrant::bind(&attempt.request)? != attempt.grant
                {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "invalid prepared attempt binding",
                    ));
                }
                match &attempt.request.scope {
                    workflow_worker::InvocationScope::Workflow {
                        run_id,
                        attempt_id,
                        lease_epoch,
                        ..
                    } if run_id == &self.run_id
                        && attempt_id == &attempt.attempt_id
                        && *lease_epoch == attempt.epoch => {}
                    _ => {
                        return Err(Error::new(
                            ErrorCode::InvalidRequest,
                            "attempt scope mismatch",
                        ));
                    }
                }
                self.attempts.insert(
                    attempt.attempt_id.clone(),
                    AttemptState {
                        prepared: *attempt.clone(),
                        outcome: None,
                    },
                );
                now
            }
            ExecutionAction::Finished {
                attempt_id,
                result,
                event_id,
                event_revision,
                at_unix_ms,
            } => {
                validate_id(event_id)?;
                let a = self
                    .attempts
                    .get(attempt_id)
                    .ok_or_else(|| Error::new(ErrorCode::NotFound, "attempt missing"))?;
                self.current(a.prepared.epoch, *at_unix_ms)?;
                if a.outcome.is_some()
                    || *event_revision <= a.prepared.prepared_revision
                    || workflow_worker::digest(&a.prepared.request)? != result.request_digest
                {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "attempt result mismatch or duplicate settlement",
                    ));
                }
                self.attempts.get_mut(attempt_id).unwrap().outcome =
                    Some(AttemptOutcome::Finished {
                        result: result.clone(),
                        event_id: event_id.clone(),
                        revision: *event_revision,
                    });
                *at_unix_ms
            }
            ExecutionAction::Failed {
                attempt_id,
                error,
                at_unix_ms,
            } => {
                let a = self
                    .attempts
                    .get(attempt_id)
                    .ok_or_else(|| Error::new(ErrorCode::NotFound, "attempt missing"))?;
                self.current(a.prepared.epoch, *at_unix_ms)?;
                if a.outcome.is_some() || error.message.is_empty() || error.message.len() > 8192 {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "invalid attempt failure",
                    ));
                }
                self.attempts.get_mut(attempt_id).unwrap().outcome =
                    Some(AttemptOutcome::Failed(error.clone()));
                *at_unix_ms
            }
            ExecutionAction::Handled {
                epoch,
                event_id,
                event_revision,
                at_unix_ms,
                ..
            } => {
                self.current(*epoch, *at_unix_ms)?;
                if event_id.is_some() != event_revision.is_some() {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "control event reference mismatch",
                    ));
                }
                *at_unix_ms
            }
            ExecutionAction::GateChecked {
                epoch,
                event_id,
                event_revision,
                at_unix_ms,
                ..
            }
            | ExecutionAction::TimerAdvanced {
                epoch,
                event_id,
                event_revision,
                at_unix_ms,
            } => {
                self.current(*epoch, *at_unix_ms)?;
                validate_id(event_id)?;
                if *event_revision < 2 {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "invalid timer event revision",
                    ));
                }
                *at_unix_ms
            }
        };
        self.check_clock(now)?;
        self.last_now = now;
        Ok(())
    }
}
