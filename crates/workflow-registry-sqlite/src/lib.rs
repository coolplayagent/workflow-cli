//! Transactional local definition registry. No model/provider execution lives here.
mod verification;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{path::Path, time::Duration};
use workflow_definitions::*;
use workflow_ir::{Format, Workflow};

const APPLICATION_ID: i64 = 0x57464431;
const STORAGE_VERSION: i64 = 1;
const SCHEMA: &str = "
CREATE TABLE drafts (
 draft_id TEXT PRIMARY KEY NOT NULL,
 revision INTEGER NOT NULL CHECK (revision>0),
 deleted INTEGER NOT NULL CHECK (deleted IN (0,1))
);
CREATE TABLE draft_revisions (
 draft_id TEXT NOT NULL REFERENCES drafts(draft_id),
 revision INTEGER NOT NULL CHECK (revision>0),
 deleted INTEGER NOT NULL CHECK (deleted IN (0,1)),
 document TEXT NOT NULL CHECK (length(CAST(document AS BLOB))<=1048576),
 digest TEXT NOT NULL,
 PRIMARY KEY (draft_id,revision)
);
CREATE TABLE publications (
 workflow_id TEXT NOT NULL,
 version TEXT NOT NULL,
 document TEXT NOT NULL CHECK (length(CAST(document AS BLOB))<=1048576),
 digest TEXT NOT NULL UNIQUE,
 source_draft_id TEXT NOT NULL,
 source_revision INTEGER NOT NULL,
 PRIMARY KEY (workflow_id,version),
 FOREIGN KEY (source_draft_id,source_revision) REFERENCES draft_revisions(draft_id,revision)
);
CREATE TRIGGER immutable_revisions_update BEFORE UPDATE ON draft_revisions BEGIN SELECT RAISE(ABORT,'immutable revision'); END;
CREATE TRIGGER immutable_revisions_delete BEFORE DELETE ON draft_revisions BEGIN SELECT RAISE(ABORT,'immutable revision'); END;
CREATE TRIGGER immutable_publications_update BEFORE UPDATE ON publications BEGIN SELECT RAISE(ABORT,'immutable publication'); END;
CREATE TRIGGER immutable_publications_delete BEFORE DELETE ON publications BEGIN SELECT RAISE(ABORT,'immutable publication'); END;
";

pub struct SqliteRegistry {
    connection: Connection,
}
impl SqliteRegistry {
    /// Create or reopen a registry. Refuse other applications and future schema versions.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let mut connection = connect(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage)?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(storage)?;
        if version == 0 && app == 0 {
            let tables: i64 = tx
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
                .map_err(storage)?;
            if tables != 0 {
                return Err(Error::new(
                    ErrorCode::UnsupportedStorage,
                    "refusing to initialize a nonempty foreign database",
                ));
            }
            tx.execute_batch(SCHEMA).map_err(storage)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(storage)?;
            tx.pragma_update(None, "user_version", STORAGE_VERSION)
                .map_err(storage)?;
        } else {
            check_version(&tx)?;
        }
        tx.commit().map_err(storage)?;
        Ok(Self { connection })
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = connect(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        check_version(&connection)?;
        Ok(Self { connection })
    }
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let connection = connect(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        check_version(&connection)?;
        Ok(Self { connection })
    }

    fn change(
        &mut self,
        id: &str,
        expected: u64,
        edit: impl FnOnce(&Workflow) -> Result<Workflow>,
        deleted: bool,
    ) -> Result<Draft> {
        validate_revision(expected)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        check_version(&tx)?;
        let current = read_draft(&tx, id, None)?;
        if current.revision != expected {
            return Err(Error::conflict(expected, current.revision));
        }
        let workflow = normalized(&edit(&current.workflow)?);
        if workflow.id != current.workflow.id {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "workflow ID is immutable; create a different draft to copy or rename it",
            ));
        }
        let diagnostics = check_draft(&workflow)?;
        let digest = definition_digest(&workflow)?;
        if !deleted && digest == current.digest {
            tx.commit().map_err(storage)?;
            return Ok(current);
        }
        if current.revision == MAX_REVISION {
            return Err(Error::new(
                ErrorCode::RevisionExhausted,
                "draft revision space exhausted",
            ));
        }
        let draft = Draft {
            draft_id: id.into(),
            revision: current.revision + 1,
            deleted,
            digest,
            workflow,
            diagnostics,
        };
        write_revision(&tx, &draft)?;
        tx.execute(
            "UPDATE drafts SET revision=?2,deleted=?3 WHERE draft_id=?1",
            params![id, sql_revision(draft.revision)?, draft.deleted],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(draft)
    }
}
fn normalized(workflow: &Workflow) -> Workflow {
    let mut workflow = workflow.clone();
    workflow.nodes.sort_by(|a, b| a.id.cmp(&b.id));
    workflow
}
fn connect(path: &Path, flags: OpenFlags) -> Result<Connection> {
    if path.as_os_str().is_empty() || path == Path::new(":memory:") {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "registry requires a persistent filesystem path",
        ));
    }
    let connection = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(storage)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(storage)?;
    if !flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(storage)?;
    }
    Ok(connection)
}
fn check_version(connection: &Connection) -> Result<()> {
    let app: i64 = connection
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(storage)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if app != APPLICATION_ID || version != STORAGE_VERSION {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            format!(
                "expected workflow registry schema {STORAGE_VERSION}, found application {app} version {version}"
            ),
        ));
    }
    Ok(())
}
fn storage(error: rusqlite::Error) -> Error {
    let code = match &error {
        rusqlite::Error::SqliteFailure(e, _)
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            ErrorCode::Busy
        }
        _ => ErrorCode::Storage,
    };
    Error::new(code, error.to_string())
}
fn corrupt(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::CorruptStorage, message)
}
fn decode(document: &str, digest: &str) -> Result<Workflow> {
    let workflow =
        workflow_ir::parse(document, Format::Json, "registry").map_err(|e| corrupt(e.message))?;
    check_draft(&workflow).map_err(|e| corrupt(e.message))?;
    if definition_digest(&workflow)? != digest {
        return Err(corrupt("definition digest mismatch"));
    }
    Ok(workflow)
}
fn sql_revision(revision: u64) -> Result<i64> {
    validate_revision(revision)?;
    i64::try_from(revision).map_err(|_| {
        Error::new(
            ErrorCode::InvalidRequest,
            "revision exceeds SQLite integer range",
        )
    })
}
fn write_revision(connection: &Connection, draft: &Draft) -> Result<()> {
    let document = draft
        .workflow
        .canonical_json()
        .map_err(|e| corrupt(e.to_string()))?;
    connection
        .execute(
            "INSERT INTO draft_revisions(draft_id,revision,deleted,document,digest)
         VALUES (?1,?2,?3,?4,?5)",
            params![
                draft.draft_id,
                sql_revision(draft.revision)?,
                draft.deleted,
                document,
                draft.digest
            ],
        )
        .map_err(storage)?;
    Ok(())
}
fn read_draft(connection: &Connection, id: &str, revision: Option<u64>) -> Result<Draft> {
    if let Some(rev) = revision {
        validate_revision(rev)?;
    }
    let row = connection
        .query_row(
            "SELECT r.revision,r.deleted,r.document,r.digest FROM draft_revisions r
         JOIN drafts d ON d.draft_id=r.draft_id
         WHERE r.draft_id=?1 AND r.revision=coalesce(?2,d.revision)
         AND (?2 IS NOT NULL OR d.deleted=0)",
            params![id, revision.map(sql_revision).transpose()?],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(storage)?
        .ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("draft/revision {id} not found"),
            )
        })?;
    let (revision, deleted, document, digest) = row;
    let revision = u64::try_from(revision).map_err(|_| corrupt("negative stored revision"))?;
    if revision == 0 || (deleted && !matches!(revision, 2..)) {
        return Err(corrupt("invalid stored draft revision"));
    }
    let workflow = decode(&document, &digest)?;
    let diagnostics = check_draft(&workflow)?;
    Ok(Draft {
        draft_id: id.into(),
        revision,
        deleted,
        workflow,
        digest,
        diagnostics,
    })
}

impl DefinitionRegistry for SqliteRegistry {
    fn create_draft(&mut self, id: &str, workflow: &Workflow) -> Result<Draft> {
        validate_draft_id(id)?;
        let workflow = normalized(workflow);
        let diagnostics = check_draft(&workflow)?;
        let draft = Draft {
            draft_id: id.into(),
            revision: 1,
            deleted: false,
            workflow: workflow.clone(),
            digest: definition_digest(&workflow)?,
            diagnostics,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        check_version(&tx)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM drafts WHERE draft_id=?1)",
                [id],
                |r| r.get(0),
            )
            .map_err(storage)?;
        if exists {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                "draft identity already exists, including tombstones",
            ));
        }
        tx.execute(
            "INSERT INTO drafts(draft_id,revision,deleted) VALUES (?1,1,0)",
            [id],
        )
        .map_err(storage)?;
        write_revision(&tx, &draft)?;
        tx.commit().map_err(storage)?;
        Ok(draft)
    }
    fn get_draft(&self, id: &str) -> Result<Draft> {
        read_draft(&self.connection, id, None)
    }
    fn get_revision(&self, id: &str, revision: u64) -> Result<Draft> {
        read_draft(&self.connection, id, Some(revision))
    }
    fn list_drafts(&self, page: &PageRequest) -> Result<Page<DefinitionSummary>> {
        page.validate()?;
        let tx = self.connection.unchecked_transaction().map_err(storage)?;
        let mut statement=tx.prepare("SELECT draft_id FROM drafts WHERE deleted=0 AND (?1 IS NULL OR draft_id>?1) ORDER BY draft_id LIMIT ?2").map_err(storage)?;
        let ids: Vec<String> = statement
            .query_map(params![page.after, page.limit + 1], |r| r.get(0))
            .map_err(storage)?
            .collect::<std::result::Result<_, _>>()
            .map_err(storage)?;
        drop(statement);
        let mut items = vec![];
        for id in ids.iter().take(page.limit as usize) {
            let d = read_draft(&tx, id, None)?;
            items.push(DefinitionSummary {
                draft_id: Some(id.clone()),
                revision: Some(d.revision),
                workflow_id: d.workflow.id,
                version: d.workflow.version,
                digest: d.digest,
            });
        }
        let next_cursor = if ids.len() > page.limit as usize {
            ids.get(page.limit as usize - 1).cloned()
        } else {
            None
        };
        tx.commit().map_err(storage)?;
        Ok(Page { items, next_cursor })
    }
    fn edit_draft(&mut self, id: &str, patch: &Patch) -> Result<Draft> {
        self.change(
            id,
            patch.expected_revision,
            |w| apply_patch(w, patch),
            false,
        )
    }
    fn replace_draft(&mut self, id: &str, expected: u64, workflow: &Workflow) -> Result<Draft> {
        self.change(id, expected, |_| Ok(workflow.clone()), false)
    }
    fn delete_draft(&mut self, id: &str, expected: u64) -> Result<Draft> {
        self.change(id, expected, |w| Ok(w.clone()), true)
    }
    fn publish(&mut self, id: &str, expected: u64) -> Result<PublishedDefinition> {
        validate_revision(expected)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        check_version(&tx)?;
        let draft = read_draft(&tx, id, None)?;
        if draft.revision != expected {
            return Err(Error::conflict(expected, draft.revision));
        }
        check_publish(&draft.workflow)?;
        if let Some(existing) = read_publication(
            &tx,
            Some((&draft.workflow.id, &draft.workflow.version)),
            None,
        )? {
            if existing.digest != draft.digest {
                return Err(Error::new(
                    ErrorCode::PublicationConflict,
                    "published ID/version already contains different content; choose a new version",
                ));
            }
            tx.commit().map_err(storage)?;
            return Ok(existing);
        }
        let result = PublishedDefinition {
            workflow: draft.workflow,
            digest: draft.digest,
            source_draft_id: draft.draft_id,
            source_revision: draft.revision,
        };
        let document = result
            .workflow
            .canonical_json()
            .map_err(|e| corrupt(e.to_string()))?;
        tx.execute(
            "INSERT INTO publications(workflow_id,version,document,digest,source_draft_id,source_revision)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![result.workflow.id, result.workflow.version, document, result.digest,
                    result.source_draft_id, sql_revision(result.source_revision)?],
        ).map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    fn get_published(&self, id: &str, version: &str) -> Result<PublishedDefinition> {
        read_publication(&self.connection, Some((id, version)), None)?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "published definition not found"))
    }
    fn get_by_digest(&self, digest: &str) -> Result<PublishedDefinition> {
        read_publication(&self.connection, None, Some(digest))?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "published definition digest not found"))
    }
    fn list_published(&self, id: &str, page: &PageRequest) -> Result<Page<DefinitionSummary>> {
        page.validate()?;
        let tx = self.connection.unchecked_transaction().map_err(storage)?;
        let mut statement=tx.prepare("SELECT version FROM publications WHERE workflow_id=?1 AND (?2 IS NULL OR version>?2) ORDER BY version LIMIT ?3").map_err(storage)?;
        let versions: Vec<String> = statement
            .query_map(params![id, page.after, page.limit + 1], |r| r.get(0))
            .map_err(storage)?
            .collect::<std::result::Result<_, _>>()
            .map_err(storage)?;
        drop(statement);
        let mut items = vec![];
        for version in versions.iter().take(page.limit as usize) {
            let p = read_publication(&tx, Some((id, version)), None)?
                .ok_or_else(|| corrupt("missing publication in page"))?;
            items.push(DefinitionSummary {
                draft_id: Some(p.source_draft_id),
                revision: Some(p.source_revision),
                workflow_id: p.workflow.id,
                version: p.workflow.version,
                digest: p.digest,
            });
        }
        let next_cursor = if versions.len() > page.limit as usize {
            versions.get(page.limit as usize - 1).cloned()
        } else {
            None
        };
        tx.commit().map_err(storage)?;
        Ok(Page { items, next_cursor })
    }
}
fn read_publication(
    connection: &Connection,
    identity: Option<(&str, &str)>,
    digest: Option<&str>,
) -> Result<Option<PublishedDefinition>> {
    let row = connection
        .query_row(
            "SELECT workflow_id,version,document,digest,source_draft_id,source_revision
         FROM publications WHERE (?1 IS NOT NULL AND workflow_id=?1 AND version=?2)
         OR (?3 IS NOT NULL AND digest=?3)",
            params![identity.map(|i| i.0), identity.map(|i| i.1), digest],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()
        .map_err(storage)?;
    let Some((id, version, document, digest, source_draft_id, source_revision)) = row else {
        return Ok(None);
    };
    let source_revision =
        u64::try_from(source_revision).map_err(|_| corrupt("negative source revision"))?;
    let workflow = decode(&document, &digest)?;
    if workflow.id != id || workflow.version != version {
        return Err(corrupt("publication identity disagrees with its content"));
    }
    check_publish(&workflow).map_err(|e| corrupt(e.message))?;
    let source = read_draft(connection, &source_draft_id, Some(source_revision))
        .map_err(|e| corrupt(e.message))?;
    if source.deleted || source.digest != digest {
        return Err(corrupt(
            "publication provenance disagrees with its source revision",
        ));
    }
    Ok(Some(PublishedDefinition {
        workflow,
        digest,
        source_draft_id,
        source_revision,
    }))
}

#[cfg(test)]
mod tests;
