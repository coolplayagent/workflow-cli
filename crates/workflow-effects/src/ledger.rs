use crate::*;
use std::collections::BTreeMap;
use workflow_worker::FailureClass;

pub fn decision(s: &EffectState, epoch: u64, now: u64, allow_write: bool) -> Decision {
    let kind = match &s.status {
        EffectStatus::Applied { .. } | EffectStatus::Failed { .. } | EffectStatus::Cancelled => {
            return Decision::Done;
        }
        EffectStatus::Uncertain { reason } => return Decision::Manual(reason.clone()),
        EffectStatus::NeedsAttention { code } => {
            return Decision::Manual(format!("compensation rejected without applying: {code}"));
        }
        EffectStatus::InFlight => {
            let previous = &s.calls.last().expect("ledger has a prepared call").attempt;
            if previous.epoch == epoch {
                return Decision::InProgress;
            }
            if s.intent.has_query() {
                CallKind::Query
            } else if s.intent.deduplicates() {
                CallKind::Write
            } else {
                return Decision::Manual(
                    "orphan write has no query or idempotency guarantee".into(),
                );
            }
        }
        EffectStatus::Retry {
            kind,
            not_before_unix_ms,
        } => {
            if now < *not_before_unix_ms {
                return Decision::Wait(*not_before_unix_ms);
            }
            *kind
        }
    };
    if s.calls.len() >= s.intent.policy.retry.max_calls as usize {
        return Decision::Manual("effect call budget exhausted".into());
    }
    if kind == CallKind::Write && (!allow_write || now >= s.intent.write_deadline()) {
        if s.intent.has_query() {
            return Decision::Call(CallKind::Query);
        }
        return Decision::Manual(
            "write admission closed by cancellation, deadline or deduplication retention".into(),
        );
    }
    Decision::Call(kind)
}
fn bounded_text(value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        return Err(invalid("empty or oversized effect annotation"));
    }
    Ok(())
}
fn retry(s: &EffectState, kind: CallKind, now: u64) -> Result<EffectStatus> {
    let r = &s.intent.policy.retry;
    let base = r
        .initial_backoff_ms
        .saturating_mul(1u64 << (s.calls.len().saturating_sub(1).min(31)))
        .min(r.max_backoff_ms);
    let hash = digest(&(&s.intent.operation_key, s.calls.len()))?;
    let sample =
        u64::from_str_radix(&hash[7..23], 16).map_err(|_| invalid("invalid jitter digest"))?;
    // Deterministic full-range 50..100% jitter; replay never samples randomness.
    let delay = base / 2 + sample % (base - base / 2 + 1);
    Ok(EffectStatus::Retry {
        kind,
        not_before_unix_ms: now.saturating_add(delay.max(1)),
    })
}
fn unknown(s: &EffectState, now: u64, reason: &str) -> Result<EffectStatus> {
    if s.intent.has_query() {
        retry(s, CallKind::Query, now)
    } else if s.intent.deduplicates() {
        retry(s, CallKind::Write, now)
    } else {
        Ok(EffectStatus::Uncertain {
            reason: reason.into(),
        })
    }
}
/// Apply a record only after the enclosing host has verified its lease. This
/// function also runs during recovery and performs no I/O or clock sampling.
pub fn apply(ledger: &mut BTreeMap<String, EffectState>, r: &EffectRecord) -> Result<()> {
    if r.epoch == 0 || r.at_unix_ms == 0 {
        return Err(invalid("effect record needs an epoch and time"));
    }
    let key = r.change.key();
    let next = match &r.change {
        EffectChange::Imported { intent, resolution } => {
            intent.validate()?;
            if ledger.contains_key(key) || intent.created_at_unix_ms > r.at_unix_ms {
                return Err(invalid("import requires a missing historical effect"));
            }
            crate::compensation::validate_admission(ledger, intent)?;
            if !workflow_validator::identifier(&resolution.resolution_id) {
                return Err(invalid("stable import resolution ID required"));
            }
            bounded_text(&resolution.actor, 128)?;
            bounded_text(&resolution.reason, 1024)?;
            bounded_text(&resolution.evidence, 8192)?;
            let ManualOutcome::Applied { receipt } = &resolution.outcome else {
                return Err(invalid(
                    "effect import requires an actual applied provider receipt",
                ));
            };
            receipt.validate(intent)?;
            EffectState {
                compensated_by: None,
                intent: *intent.clone(),
                calls: vec![],
                status: EffectStatus::Applied {
                    receipt: receipt.clone(),
                },
            }
        }
        EffectChange::Prepared {
            attempt: p,
            request_digest,
        } => {
            if *request_digest != digest(p)? {
                return Err(invalid("effect request digest mismatch"));
            }
            p.validate()?;
            if p.kind == CallKind::Write {
                crate::compensation::validate_admission(ledger, &p.intent)?;
            }
            if p.epoch != r.epoch
                || p.issued_at_unix_ms != r.at_unix_ms
                || p.prepared_revision == 0
                || p.deadline_unix_ms <= p.issued_at_unix_ms
                || p.deadline_unix_ms
                    > p.issued_at_unix_ms
                        .saturating_add(p.intent.capability.timeout_ms)
                || (p.kind == CallKind::Write && p.deadline_unix_ms > p.intent.write_deadline())
                || !workflow_validator::identifier(&p.attempt_id)
            {
                return Err(invalid(
                    "invalid effect attempt identity or bounded deadline",
                ));
            }
            let mut state = if let Some(s) = ledger.get(key) {
                let planned = decision(s, r.epoch, r.at_unix_ms, true);
                let query_instead_of_write = p.kind == CallKind::Query
                    && s.intent.has_query()
                    && planned == Decision::Call(CallKind::Write);
                if s.intent != p.intent
                    || (planned != Decision::Call(p.kind) && !query_instead_of_write)
                {
                    return Err(invalid(
                        "effect retry is ineligible or changed its immutable intent",
                    ));
                }
                s.clone()
            } else {
                if p.kind != CallKind::Write || p.intent.created_at_unix_ms != r.at_unix_ms {
                    return Err(invalid(
                        "initial effect intent must precede its first write",
                    ));
                }
                EffectState {
                    compensated_by: None,
                    intent: p.intent.clone(),
                    calls: vec![],
                    status: EffectStatus::InFlight,
                }
            };
            if p.number != state.calls.len() as u32 + 1
                || state
                    .calls
                    .iter()
                    .any(|c| c.attempt.attempt_id == p.attempt_id)
            {
                return Err(invalid("effect attempt count or ID mismatch"));
            }
            state.calls.push(AttemptRecord {
                attempt: *p.clone(),
                request_digest: request_digest.clone(),
                observation: None,
            });
            state.status = EffectStatus::InFlight;
            state
        }
        EffectChange::Observed {
            attempt_id,
            observation,
            ..
        } => {
            let mut s = ledger
                .get(key)
                .ok_or_else(|| invalid("effect intent missing"))?
                .clone();
            let last = s
                .calls
                .last()
                .ok_or_else(|| invalid("effect attempt missing"))?;
            if last.attempt.attempt_id != *attempt_id
                || last.attempt.epoch != r.epoch
                || last.observation.is_some()
                || s.status != EffectStatus::InFlight
                || last.attempt.issued_at_unix_ms > r.at_unix_ms
            {
                return Err(invalid(
                    "effect observation is stale, duplicate or from another call",
                ));
            }
            s.status = match observation {
                Observation::Applied { receipt } => {
                    receipt.validate(&s.intent)?;
                    EffectStatus::Applied {
                        receipt: receipt.clone(),
                    }
                }
                Observation::NotApplied {
                    code,
                    class,
                    message,
                } => {
                    bounded_text(message, 8192)?;
                    if last.attempt.kind != CallKind::Write
                        || s.intent.capability.error_codes.get(code) != Some(class)
                        || *class == FailureClass::UnknownEffect
                    {
                        return Err(invalid(
                            "known-no-effect failure must match a declared write error",
                        ));
                    }
                    let unresolved_older_write = s.calls.iter().take(s.calls.len() - 1).any(|c| {
                        c.attempt.kind == CallKind::Write
                            && matches!(c.observation, None | Some(Observation::Unknown { .. }))
                    });
                    if unresolved_older_write {
                        unknown(
                            &s,
                            r.at_unix_ms,
                            "current invocation rejection does not settle an older unknown write",
                        )?
                    } else if *class == FailureClass::Transient
                        && s.intent.deduplicates()
                        && s.calls.len() < s.intent.policy.retry.max_calls as usize
                        && r.at_unix_ms < s.intent.write_deadline()
                    {
                        retry(&s, CallKind::Write, r.at_unix_ms)?
                    } else if s.intent.compensates.is_some() {
                        EffectStatus::NeedsAttention { code: code.clone() }
                    } else {
                        EffectStatus::Failed { code: code.clone() }
                    }
                }
                Observation::Absent => {
                    if last.attempt.kind != CallKind::Query {
                        return Err(invalid("only a query can observe absence"));
                    }
                    if s.intent.deduplicates() {
                        retry(&s, CallKind::Write, r.at_unix_ms)?
                    } else {
                        EffectStatus::Uncertain {
                            reason: "query absence cannot fence a still-running old writer".into(),
                        }
                    }
                }
                Observation::Unknown { reason } => {
                    bounded_text(reason, 1024)?;
                    unknown(&s, r.at_unix_ms, reason)?
                }
            };
            s.calls.last_mut().unwrap().observation = Some(observation.clone());
            s
        }
        EffectChange::Stopped { reason, .. } => {
            bounded_text(reason, 1024)?;
            let mut s = ledger
                .get(key)
                .ok_or_else(|| invalid("effect missing"))?
                .clone();
            if matches!(
                s.status,
                EffectStatus::Applied { .. }
                    | EffectStatus::Failed { .. }
                    | EffectStatus::Cancelled
                    | EffectStatus::Uncertain { .. }
                    | EffectStatus::NeedsAttention { .. }
            ) {
                return Err(invalid("effect already settled or stopped"));
            }
            // Keep known failed compensation distinct from unknown effects when
            // its retry window closes. An unresolved older write still forbids
            // any inference of absence.
            let writes: Vec<_> = s
                .calls
                .iter()
                .filter(|c| c.attempt.kind == CallKind::Write)
                .collect();
            s.status = if s.intent.compensates.is_some()
                && !writes.is_empty()
                && writes
                    .iter()
                    .all(|c| matches!(c.observation, Some(Observation::NotApplied { .. })))
            {
                let Some(Observation::NotApplied { code, .. }) =
                    &writes.last().unwrap().observation
                else {
                    unreachable!()
                };
                EffectStatus::NeedsAttention { code: code.clone() }
            } else {
                EffectStatus::Uncertain {
                    reason: reason.clone(),
                }
            };
            s
        }
        EffectChange::Resolved { resolution: m, .. } => {
            let mut s = ledger
                .get(key)
                .ok_or_else(|| invalid("effect missing"))?
                .clone();
            if !matches!(
                s.status,
                EffectStatus::Uncertain { .. } | EffectStatus::NeedsAttention { .. }
            ) || !workflow_validator::identifier(&m.resolution_id)
            {
                return Err(invalid(
                    "manual resolution needs an unresolved effect and stable resolution ID",
                ));
            }
            bounded_text(&m.actor, 128)?;
            bounded_text(&m.reason, 1024)?;
            bounded_text(&m.evidence, 8192)?;
            s.status = match &m.outcome {
                ManualOutcome::Applied { receipt } => {
                    receipt.validate(&s.intent)?;
                    EffectStatus::Applied {
                        receipt: receipt.clone(),
                    }
                }
                ManualOutcome::ConfirmedNotApplied => EffectStatus::Cancelled,
            };
            s
        }
    };
    if let (Some(original), EffectStatus::Applied { .. }) = (&next.intent.compensates, &next.status)
    {
        let source = ledger
            .get_mut(&original.operation_key)
            .ok_or_else(|| invalid("original compensated effect missing"))?;
        if source.compensated_by.as_ref().is_some_and(|k| k != key) {
            return Err(invalid(
                "original effect already has a different completed compensation",
            ));
        }
        source.compensated_by = Some(key.into());
    }
    ledger.insert(key.into(), next);
    Ok(())
}
