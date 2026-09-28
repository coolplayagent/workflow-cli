use crate::*;
use std::collections::{BTreeMap, BTreeSet};
use workflow_ir::VersionRef;
use workflow_worker::EffectContract;

pub fn compensation_key(original: &str, capability: &VersionRef) -> Result<String> {
    digest(&("workflow-effect-v1", original, "compensation", capability))
}
fn depends_on(ledger: &BTreeMap<String, EffectState>, effect: &EffectState, key: &str) -> bool {
    let mut pending: Vec<_> = effect
        .intent
        .dependencies
        .iter()
        .map(String::as_str)
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(current) = pending.pop() {
        if current == key {
            return true;
        }
        if seen.insert(current)
            && let Some(s) = ledger.get(current)
        {
            pending.extend(s.intent.dependencies.iter().map(String::as_str));
        }
    }
    false
}
/// Check declared business effect dependencies against the durable provider
/// observations. Control-flow reachability alone cannot establish these facts.
pub(crate) fn validate_admission(
    ledger: &BTreeMap<String, EffectState>,
    intent: &EffectIntent,
) -> Result<()> {
    for key in &intent.dependencies {
        if intent
            .compensates
            .as_ref()
            .is_some_and(|c| &c.operation_key == key)
        {
            continue;
        }
        let dependency = ledger
            .get(key)
            .ok_or_else(|| invalid("effect prerequisite has no durable receipt"))?;
        if !matches!(dependency.status, EffectStatus::Applied { .. })
            || dependency.compensated_by.is_some()
            || ledger.values().any(|s| {
                s.intent
                    .compensates
                    .as_ref()
                    .is_some_and(|c| &c.operation_key == key)
            })
        {
            return Err(invalid(
                "effect prerequisite is unresolved or compensation has begun",
            ));
        }
    }
    let Some(link) = &intent.compensates else {
        return Ok(());
    };
    let source = ledger
        .get(&link.operation_key)
        .ok_or_else(|| invalid("original effect is missing"))?;
    let EffectStatus::Applied { receipt } = &source.status else {
        return Err(invalid("only an applied effect can be compensated"));
    };
    let EffectContract::Write {
        irreversible: false,
        compensation: Some(capability),
        ..
    } = &source.intent.capability.effects
    else {
        return Err(invalid(
            "original effect does not permit automatic compensation",
        ));
    };
    if source.intent.compensates.is_some()
        || source.intent.run_digest != intent.run_digest
        || receipt != &link.receipt
        || capability != &intent.capability.capability
        || source.compensated_by.is_some()
        || ledger.values().any(|s| {
            s.intent.operation_key != intent.operation_key
                && s.intent
                    .compensates
                    .as_ref()
                    .is_some_and(|c| c.operation_key == link.operation_key)
        })
    {
        return Err(invalid(
            "compensation changed its original receipt, capability or ownership",
        ));
    }
    for dependent in ledger.values().filter(|s| {
        s.intent.compensates.is_none() && s.intent.operation_key != source.intent.operation_key
    }) {
        if depends_on(ledger, dependent, &link.operation_key)
            && !matches!(
                dependent.status,
                EffectStatus::Failed { .. } | EffectStatus::Cancelled
            )
            && dependent.compensated_by.is_none()
        {
            return Err(invalid(
                "compensate dependent effects first; unresolved effects cannot be assumed absent",
            ));
        }
    }
    Ok(())
}
