use crate::*;
use rusqlite::{OptionalExtension, params};
use workflow_kernel::CompiledBundle;
/// RunStore-local immutable version catalog. Registration and the run commit share
/// one transaction. It does not replace external registry authorization/provenance.
pub(crate) fn lock_bindings(c: &Connection, bundle: &CompiledBundle, create: bool) -> Result<()> {
    for w in &bundle.spec().workflows {
        let hash = w.digest().map_err(|e| corrupt(e.to_string()))?;
        check(c, "workflow", &w.id, &w.version, &hash, create)?;
    }
    for d in &bundle.spec().capabilities {
        let capability = workflow_worker::Capability::new(d.clone())?;
        check(
            c,
            "capability",
            &d.capability.id,
            &d.capability.version,
            capability.digest(),
            create,
        )?;
    }
    Ok(())
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
