use super::*;
impl SqliteRegistry {
    /// Verify the entire retained catalog, including deleted drafts and old revisions.
    pub fn verify_all(&self) -> Result<()> {
        let tx = self.connection.unchecked_transaction().map_err(storage)?;
        check_version(&tx)?;
        let mut q = tx
            .prepare("SELECT draft_id,revision,deleted FROM drafts ORDER BY draft_id")
            .map_err(storage)?;
        let heads = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })
            .map_err(storage)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(q);
        let mut verified = 0usize;
        for (id, head, deleted) in heads {
            validate_draft_id(&id).map_err(|e| corrupt(e.message))?;
            let mut q = tx
                .prepare("SELECT revision FROM draft_revisions WHERE draft_id=?1 ORDER BY revision")
                .map_err(storage)?;
            let revisions = q
                .query_map([&id], |r| r.get::<_, i64>(0))
                .map_err(storage)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(storage)?;
            if revisions.is_empty() || revisions.last() != Some(&head) {
                return Err(corrupt(
                    "draft head is missing or does not match its retained history",
                ));
            }
            for (index, rev) in revisions.into_iter().enumerate() {
                verified += 1;
                if verified > 10000 {
                    return Err(corrupt(
                        "registry verification exceeds 10000 retained revisions",
                    ));
                }
                if rev != index as i64 + 1 {
                    return Err(corrupt("draft history sequence gap"));
                }
                let d = read_draft(&tx, &id, Some(rev as u64))?;
                if rev == head && d.deleted != deleted {
                    return Err(corrupt("draft deletion status differs from its head"));
                }
            }
        }
        let mut q = tx
            .prepare("SELECT workflow_id,version FROM publications ORDER BY workflow_id,version")
            .map_err(storage)?;
        let publications = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(storage)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage)?;
        if publications.len() > 10000 {
            return Err(corrupt("registry verification exceeds 10000 publications"));
        }
        for (id, version) in publications {
            read_publication(&tx, Some((&id, &version)), None)?
                .ok_or_else(|| corrupt("publication missing"))?;
        }
        drop(q);
        tx.commit().map_err(storage)
    }
}
