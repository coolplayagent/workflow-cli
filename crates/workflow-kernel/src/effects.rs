use crate::bundle::key;
use crate::{BundleSpec, Error, ErrorCode, Result};
use std::collections::{BTreeMap, BTreeSet};
use workflow_ir::{NodeKind, Workflow};
use workflow_worker::{Capability, EffectContract};
fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::ContractMismatch, message)
}
fn reachable(w: &Workflow, from: &str, target: &str, skip: Option<&str>) -> bool {
    let mut pending = vec![from];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if Some(id) == skip {
            continue;
        }
        if id == target {
            return true;
        }
        if seen.insert(id) {
            pending.extend(
                w.edges
                    .iter()
                    .filter(|e| e.from == id)
                    .map(|e| e.to.as_str()),
            );
        }
    }
    false
}
fn precedes(w: &Workflow, before: &str, after: &str) -> bool {
    before != after
        && reachable(w, before, after, None)
        && !reachable(w, &w.entry, after, Some(before))
}
pub(crate) fn validate(
    spec: &BundleSpec,
    workflows: &BTreeMap<String, Workflow>,
    caps: &BTreeMap<String, Capability>,
) -> Result<()> {
    let mut compensators = BTreeMap::new();
    for b in &spec.effect_bindings {
        let w = &workflows[&key(&b.workflow)];
        if let Some(release) = &b.release {
            let node = w
                .nodes
                .iter()
                .find(|n| n.id == b.node_id)
                .ok_or_else(|| invalid("release node missing"))?;
            let NodeKind::Task { capability, .. } = &node.kind else {
                return Err(invalid("release requires a write task"));
            };
            let cap = caps[&key(capability)].descriptor();
            let has_query = matches!(&cap.effects, EffectContract::Write { query: Some(_), .. });
            release.validate(has_query)?;
            if !node.inputs.get(&release.subject_field).is_some_and(|f| {
                f.required && f.value_type == workflow_effects::release_subject_type()
            }) {
                return Err(invalid(
                    "release requires an exact required delivery subject input",
                ));
            }
            for gate in &release.gate_nodes {
                if !precedes(w, gate, &b.node_id)
                    || !spec.postconditions.iter().any(|p| {
                        p.workflow == b.workflow && p.node_id == *gate && p.action == *capability
                    })
                {
                    return Err(invalid(
                        "each release gate must bind this action and precede every path to the write",
                    ));
                }
            }
            for required in &release.approvals {
                if !precedes(w, &required.approval.node_id, &b.node_id)
                    || !spec.wait_policies.iter().any(|p| {
                        p.workflow == b.workflow
                            && p.node_id == required.approval.node_id
                            && p.policy.kind == crate::WaitKind::HumanApproval
                            && p.policy.subjects.get(&required.approval.subject_field)
                                == Some(&crate::SubjectKind::Digest)
                    })
                {
                    return Err(invalid(
                        "release approval requires a preceding scoped human wait",
                    ));
                }
            }
        }
        if b.depends_on.len() > 128
            || b.depends_on.iter().collect::<BTreeSet<_>>().len() != b.depends_on.len()
        {
            return Err(invalid(
                "effect dependencies must be unique and bounded to 128",
            ));
        }
        for dependency in &b.depends_on {
            if !spec
                .effect_bindings
                .iter()
                .any(|d| d.workflow == b.workflow && d.node_id == *dependency)
                || !precedes(w, dependency, &b.node_id)
            {
                return Err(invalid(
                    "an effect prerequisite must be a managed node that precedes every path to this task",
                ));
            }
        }
        let Some(original) = &b.compensates else {
            continue;
        };
        let source = spec
            .effect_bindings
            .iter()
            .find(|s| s.workflow == b.workflow && s.node_id == *original)
            .ok_or_else(|| {
                invalid("compensation must name a managed original task in the same workflow frame")
            })?;
        if source.compensates.is_some()
            || !precedes(w, original, &b.node_id)
            || compensators
                .insert((key(&b.workflow), original.clone()), b.node_id.clone())
                .is_some()
        {
            return Err(invalid(
                "compensation requires one later task per original; compensation-of-compensation is unsupported",
            ));
        }
        let cap_for = |id: &str| -> Result<&Capability> {
            let node = w
                .nodes
                .iter()
                .find(|n| n.id == id)
                .ok_or_else(|| invalid("effect node missing"))?;
            let NodeKind::Task { capability, .. } = &node.kind else {
                return Err(invalid("effect node must be a task"));
            };
            Ok(&caps[&key(capability)])
        };
        let EffectContract::Write {
            irreversible: false,
            compensation: Some(reference),
            ..
        } = &cap_for(original)?.descriptor().effects
        else {
            return Err(invalid(
                "original capability must declare this compensator and permit reversal",
            ));
        };
        if &cap_for(&b.node_id)?.descriptor().capability != reference {
            return Err(invalid(
                "compensating task capability differs from the original effect contract",
            ));
        }
    }
    // Reject an unavoidable inverse ordering conflict. Runtime ledger checks
    // still decide which optional branch effects actually occurred.
    for b in spec
        .effect_bindings
        .iter()
        .filter(|b| b.compensates.is_none())
    {
        let w = &workflows[&key(&b.workflow)];
        for dep in &b.depends_on {
            if let (Some(undo_b), Some(undo_dep)) = (
                compensators.get(&(key(&b.workflow), b.node_id.clone())),
                compensators.get(&(key(&b.workflow), dep.clone())),
            ) && reachable(w, &b.node_id, undo_dep, None)
                && !reachable(w, undo_b, undo_dep, None)
            {
                return Err(invalid(
                    "compensation control flow contradicts reverse effect dependency order",
                ));
            }
        }
    }
    Ok(())
}
