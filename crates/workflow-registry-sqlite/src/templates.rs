//! Local trusted-host template catalog. Review identity is supplied by the local
//! operator; the shared adapter attests it from scoped service credentials.
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{path::Path, time::Duration};
use workflow_templates::*;
#[cfg(test)]
mod tests;
const APPLICATION: i64 = 0x57465431;
const SCHEMA:&str="
CREATE TABLE owners(singleton INTEGER PRIMARY KEY CHECK(singleton=1), document TEXT NOT NULL);
CREATE TABLE candidates(digest TEXT PRIMARY KEY, document TEXT NOT NULL);
CREATE TABLE reviews(candidate TEXT PRIMARY KEY REFERENCES candidates(digest), document TEXT NOT NULL);
CREATE TABLE publications(id TEXT NOT NULL, version TEXT NOT NULL, digest TEXT NOT NULL UNIQUE, document TEXT NOT NULL, PRIMARY KEY(id,version));
CREATE TABLE bindings(kind TEXT NOT NULL,id TEXT NOT NULL,version TEXT NOT NULL,digest TEXT NOT NULL,PRIMARY KEY(kind,id,version));
CREATE TRIGGER candidate_update BEFORE UPDATE ON candidates BEGIN SELECT RAISE(ABORT,'immutable candidate'); END;
CREATE TRIGGER candidate_delete BEFORE DELETE ON candidates BEGIN SELECT RAISE(ABORT,'immutable candidate'); END;
CREATE TRIGGER review_update BEFORE UPDATE ON reviews BEGIN SELECT RAISE(ABORT,'immutable review'); END;
CREATE TRIGGER review_delete BEFORE DELETE ON reviews BEGIN SELECT RAISE(ABORT,'immutable review'); END;
CREATE TRIGGER publication_update BEFORE UPDATE ON publications BEGIN SELECT RAISE(ABORT,'immutable publication'); END;
CREATE TRIGGER publication_delete BEFORE DELETE ON publications BEGIN SELECT RAISE(ABORT,'immutable publication'); END;
CREATE TRIGGER binding_update BEFORE UPDATE ON bindings BEGIN SELECT RAISE(ABORT,'immutable binding'); END;
CREATE TRIGGER binding_delete BEFORE DELETE ON bindings BEGIN SELECT RAISE(ABORT,'immutable binding'); END;
";
fn fail(message: &str) -> Error {
    Error {
        path: "catalog".into(),
        message: message.into(),
    }
}
fn storage(_: rusqlite::Error) -> Error {
    fail("template catalog transaction failed; no publication confirmed")
}
fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| fail("catalog encoding failed"))
}
fn parse<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|_| fail("catalog content invalid"))
}
fn check(c: &Connection) -> Result<()> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(storage)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if app != APPLICATION || version != 1 {
        return Err(fail("unsupported template catalog"));
    }
    Ok(())
}
fn candidate(c: &Connection, key: &str) -> Result<Candidate> {
    let raw: Option<String> = c
        .query_row(
            "SELECT document FROM candidates WHERE digest=?1",
            [key],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    let result: Candidate = parse(&raw.ok_or_else(|| fail("candidate not found"))?)?;
    if result.content_digest()? != key {
        return Err(fail("candidate digest mismatch"));
    }
    Ok(result)
}
pub struct SqliteTemplateCatalog {
    connection: Connection,
}
impl SqliteTemplateCatalog {
    pub fn create(path: impl AsRef<Path>, policy: &OwnerPolicy) -> Result<Self> {
        policy.validate()?;
        let mut c = Connection::open(path).map_err(storage)?;
        c.busy_timeout(Duration::from_secs(5)).map_err(storage)?;
        c.pragma_update(None, "foreign_keys", true)
            .map_err(storage)?;
        c.pragma_update(None, "synchronous", "FULL")
            .map_err(storage)?;
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let tables: i64 = tx
            .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
            .map_err(storage)?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(storage)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage)?;
        if tables == 0 && app == 0 && version == 0 {
            tx.execute_batch(SCHEMA).map_err(storage)?;
            tx.pragma_update(None, "application_id", APPLICATION)
                .map_err(storage)?;
            tx.pragma_update(None, "user_version", 1).map_err(storage)?;
            tx.execute("INSERT INTO owners VALUES(1,?1)", [json(policy)?])
                .map_err(storage)?;
        } else {
            check(&tx)?;
            let stored: OwnerPolicy = parse(
                &tx.query_row("SELECT document FROM owners WHERE singleton=1", [], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(storage)?,
            )?;
            if &stored != policy {
                return Err(fail(
                    "existing owner policy differs; initialization cannot replace owners",
                ));
            }
        }
        tx.commit().map_err(storage)?;
        Ok(Self { connection: c })
    }
    pub fn open(path: impl AsRef<Path>, readonly: bool) -> Result<Self> {
        let c = Connection::open_with_flags(
            path,
            if readonly {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            } else {
                OpenFlags::SQLITE_OPEN_READ_WRITE
            },
        )
        .map_err(storage)?;
        c.busy_timeout(Duration::from_secs(5)).map_err(storage)?;
        c.pragma_update(None, "foreign_keys", true)
            .map_err(storage)?;
        if !readonly {
            c.pragma_update(None, "synchronous", "FULL")
                .map_err(storage)?;
        }
        check(&c)?;
        Ok(Self { connection: c })
    }
    pub fn propose(&mut self, proposed: &Candidate, actor: &str) -> Result<String> {
        let mut value = proposed.clone();
        value.proposed_by = actor.into();
        let key = value.content_digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        check(&tx)?;
        tx.execute(
            "INSERT INTO candidates VALUES(?1,?2) ON CONFLICT(digest) DO NOTHING",
            params![key, json(&value)?],
        )
        .map_err(storage)?;
        if candidate(&tx, &key)? != value {
            return Err(fail("candidate identity conflicts"));
        }
        tx.commit().map_err(storage)?;
        Ok(key)
    }
    pub fn candidate(&self, key: &str) -> Result<Candidate> {
        check(&self.connection)?;
        candidate(&self.connection, key)
    }
    pub fn review(
        &mut self,
        key: &str,
        actor: &str,
        decision: ReviewDecision,
        reason: &str,
        at: u64,
    ) -> Result<Review> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        check(&tx)?;
        let candidate = candidate(&tx, key)?;
        let owners: OwnerPolicy = parse(
            &tx.query_row("SELECT document FROM owners WHERE singleton=1", [], |r| {
                r.get::<_, String>(0)
            })
            .map_err(storage)?,
        )?;
        owners.validate()?;
        if !owners.permits(&candidate.template.owner_role, actor) {
            return Err(fail("actor is not the configured process owner"));
        }
        let review = Review {
            candidate_digest: key.into(),
            actor: actor.into(),
            owner_role: candidate.template.owner_role.clone(),
            decision,
            reason: reason.into(),
            reviewed_at_unix_ms: at,
        };
        review.validate(&candidate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT document FROM reviews WHERE candidate=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage)?;
        if let Some(old) = old {
            let old: Review = parse(&old)?;
            old.validate(&candidate)?;
            if old.actor != review.actor
                || old.decision != review.decision
                || old.reason != review.reason
            {
                return Err(fail(
                    "review already recorded; propose a new candidate after changes",
                ));
            }
            tx.commit().map_err(storage)?;
            return Ok(old);
        }
        tx.execute(
            "INSERT INTO reviews VALUES(?1,?2)",
            params![key, json(&review)?],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(review)
    }
    pub fn publish(&mut self, key: &str) -> Result<Publication> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        check(&tx)?;
        let candidate = candidate(&tx, key)?;
        let review: Option<String> = tx
            .query_row(
                "SELECT document FROM reviews WHERE candidate=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage)?;
        let publication = Publication::new(
            candidate,
            parse(&review.ok_or_else(|| fail("owner review required before publication"))?)?,
        )?;
        let identity = &publication.candidate.template.identity;
        let existing: Option<String> = tx
            .query_row(
                "SELECT document FROM publications WHERE id=?1 AND version=?2",
                params![identity.id, identity.version],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage)?;
        if let Some(existing) = existing {
            let old: Publication = parse(&existing)?;
            old.verify()?;
            if old != publication {
                return Err(fail("template version is immutable; propose a new version"));
            }
            tx.commit().map_err(storage)?;
            return Ok(old);
        }
        let compiled = publication.candidate.template.validate()?;
        for binding in workflow_runstore_sqlite::bundle_bindings(&compiled)
            .map_err(|_| fail("invalid immutable bundle identities"))?
        {
            tx.execute(
                "INSERT INTO bindings VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",
                params![binding.kind, binding.id, binding.version, binding.digest],
            )
            .map_err(storage)?;
            let old: String = tx
                .query_row(
                    "SELECT digest FROM bindings WHERE kind=?1 AND id=?2 AND version=?3",
                    params![binding.kind, binding.id, binding.version],
                    |r| r.get(0),
                )
                .map_err(storage)?;
            if old != binding.digest {
                return Err(fail(
                    "shared subworkflow or policy version conflicts; publish a new identity",
                ));
            }
        }
        tx.execute(
            "INSERT INTO publications VALUES(?1,?2,?3,?4)",
            params![
                identity.id,
                identity.version,
                publication.digest,
                json(&publication)?
            ],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(publication)
    }
    pub fn get(&self, identity: &workflow_ir::VersionRef) -> Result<Publication> {
        check(&self.connection)?;
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT document FROM publications WHERE id=?1 AND version=?2",
                params![identity.id, identity.version],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage)?;
        let p: Publication = parse(&raw.ok_or_else(|| fail("published template not found"))?)?;
        p.verify()?;
        if &p.candidate.template.identity != identity {
            return Err(fail("publication identity differs"));
        }
        Ok(p)
    }
}
