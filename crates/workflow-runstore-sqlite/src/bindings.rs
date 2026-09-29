use crate::*;
use rusqlite::{OptionalExtension, params};
use workflow_kernel::CompiledBundle;
/// RunStore-local immutable version catalog. Registration and the run commit share
/// one transaction. It does not replace external registry authorization/provenance.
pub(crate) fn lock_bindings(c: &Connection, bundle: &CompiledBundle, create: bool) -> Result<()> {
    for binding in bundle_bindings(bundle)? {
        check(
            c,
            &binding.kind,
            &binding.id,
            &binding.version,
            &binding.digest,
            create,
        )?;
    }
    Ok(())
}

/// The complete immutable identity catalog, computed without starting a run.
/// Publishers and transaction reducers use the same digests and sorted lock order.
pub fn bundle_bindings(bundle: &CompiledBundle) -> Result<Vec<ImageBinding>> {
    let mut entries = vec![];
    let mut add = |kind: &str, id: &str, version: &str, digest: &str| {
        entries.push(ImageBinding {
            kind: kind.into(),
            id: id.into(),
            version: version.into(),
            digest: digest.into(),
        });
    };
    for w in &bundle.spec().workflows {
        let hash = w.digest().map_err(|e| corrupt(e.to_string()))?;
        add("workflow", &w.id, &w.version, &hash);
    }
    for d in &bundle.spec().capabilities {
        let capability = workflow_worker::Capability::new(d.clone())?;
        add(
            "capability",
            &d.capability.id,
            &d.capability.version,
            capability.digest(),
        );
    }
    for gate in &bundle.spec().postconditions {
        add(
            "gate_policy",
            &gate.policy.identity.id,
            &gate.policy.identity.version,
            &workflow_gates::digest(&gate.policy)?,
        );
    }
    for policy in &bundle.spec().model_policies {
        add(
            "model_policy",
            &policy.policy.id,
            &policy.policy.version,
            &workflow_models::Policy::new(policy.clone())?
                .binding()
                .digest,
        );
    }
    for binding in &bundle.spec().effect_bindings {
        let p = &binding.policy;
        add(
            "effect_policy",
            &p.identity.id,
            &p.identity.version,
            &digest(p)?,
        );
    }
    entries.sort_by(|a, b| (&a.kind, &a.id, &a.version).cmp(&(&b.kind, &b.id, &b.version)));
    for pair in entries.windows(2) {
        if (&pair[0].kind, &pair[0].id, &pair[0].version)
            == (&pair[1].kind, &pair[1].id, &pair[1].version)
            && pair[0].digest != pair[1].digest
        {
            return Err(Error::new(
                ErrorCode::BindingConflict,
                "bundle contains conflicting immutable versions",
            ));
        }
    }
    entries.dedup();
    Ok(entries)
}
fn check(
    c: &Connection,
    kind: &str,
    id: &str,
    version: &str,
    hash: &str,
    create: bool,
) -> Result<()> {
    let previous: Option<String> = c
        .query_row(
            "SELECT digest FROM binding_locks WHERE kind=?1 AND id=?2 AND version=?3",
            params![kind, id, version],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    match previous {
        Some(previous) if previous == hash => Ok(()),
        Some(_) if create => Err(Error::new(
            ErrorCode::BindingConflict,
            format!("{kind} {id}@{version} already locks different content; publish a new version"),
        )),
        None if create => {
            c.execute(
                "INSERT INTO binding_locks(kind,id,version,digest) VALUES(?1,?2,?3,?4)",
                params![kind, id, version, hash],
            )
            .map_err(storage)?;
            Ok(())
        }
        _ => Err(corrupt(format!(
            "missing or changed {kind} binding {id}@{version}"
        ))),
    }
}
