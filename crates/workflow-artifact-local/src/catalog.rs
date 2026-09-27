use crate::*;
use rusqlite::params;
pub(super) const APPLICATION_ID: i64 = 0x57464131;
pub(super) const SCHEMA:&str="
CREATE TABLE head(id INTEGER PRIMARY KEY CHECK(id=1),sequence INTEGER NOT NULL CHECK(sequence>=0),chain TEXT NOT NULL);
CREATE TABLE artifacts(sequence INTEGER NOT NULL UNIQUE CHECK(sequence>0),id TEXT PRIMARY KEY NOT NULL,document TEXT NOT NULL,digest TEXT NOT NULL);
CREATE TRIGGER artifacts_no_update BEFORE UPDATE ON artifacts BEGIN SELECT RAISE(ABORT,'immutable artifact'); END;
CREATE TRIGGER artifacts_no_delete BEFORE DELETE ON artifacts BEGIN SELECT RAISE(ABORT,'retained artifact'); END;
CREATE TRIGGER head_no_delete BEFORE DELETE ON head BEGIN SELECT RAISE(ABORT,'retained head'); END;
CREATE TRIGGER head_monotonic BEFORE UPDATE ON head WHEN NEW.id!=OLD.id OR NEW.sequence!=OLD.sequence+1 BEGIN SELECT RAISE(ABORT,'nonmonotonic artifact head'); END;
";
pub(super) fn version(c: &Connection) -> Result<()> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(sql)?;
    let v: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(sql)?;
    if app != APPLICATION_ID || v != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "artifact catalog requires application WFA1/schema 1",
        ));
    }
    Ok(())
}
pub(super) fn read(c: &Connection) -> Result<BTreeMap<String, ArtifactRef>> {
    version(c)?;
    let (count, head): (i64, String) = c
        .query_row("SELECT sequence,chain FROM head WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(sql)?;
    if count < 0 || count as usize > MAX_CATALOG {
        return Err(corrupt("artifact catalog count out of bounds"));
    }
    let mut chain = digest(&"empty artifact catalog")?;
    let mut records = BTreeMap::new();
    let mut types = BTreeMap::new();
    let mut seq = 0;
    let mut q = c
        .prepare("SELECT sequence,id,document,digest FROM artifacts ORDER BY sequence")
        .map_err(sql)?;
    let mut rows = q.query([]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        seq += 1;
        let n: i64 = row.get(0).map_err(sql)?;
        let id: String = row.get(1).map_err(sql)?;
        let document: String = row.get(2).map_err(sql)?;
        let hash: String = row.get(3).map_err(sql)?;
        if seq > MAX_CATALOG || n != seq as i64 || document.len() > MAX_MANIFEST_BYTES {
            return Err(corrupt("artifact sequence or document budget mismatch"));
        }
        let r: ArtifactRef = parse_message(document.as_bytes()).map_err(|e| corrupt(e.message))?;
        validate_ref(&r).map_err(|e| corrupt(e.message))?;
        if id != r.artifact_id || digest(&r)? != hash {
            return Err(corrupt("artifact record identity or digest mismatch"));
        }
        dependencies(&records, &r).map_err(|e| corrupt(e.message))?;
        let t = &r.manifest.spec.artifact_type;
        let key = (t.identity.id.clone(), t.identity.version.clone());
        if types.insert(key, t.clone()).is_some_and(|old| old != *t) {
            return Err(corrupt("artifact type version rebound"));
        }
        chain = digest(&(chain, hash))?;
        records.insert(id, r);
    }
    if seq as i64 != count || chain != head {
        return Err(corrupt("artifact catalog differs from its head"));
    }
    Ok(records)
}
pub(super) fn dependencies(records: &BTreeMap<String, ArtifactRef>, r: &ArtifactRef) -> Result<()> {
    for input in &r.manifest.spec.inputs {
        let parent = records
            .get(&input.artifact_id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "input artifact missing"))?;
        if parent.link() != *input {
            return Err(corrupt("input artifact digest mismatch"));
        }
        if parent.manifest.spec.access != r.manifest.spec.access {
            return Err(Error::new(
                ErrorCode::ScopeMismatch,
                "input artifact belongs to another run",
            ));
        }
    }
    Ok(())
}
pub(super) fn append(
    c: &Connection,
    records: &BTreeMap<String, ArtifactRef>,
    r: &ArtifactRef,
) -> Result<()> {
    if records.len() >= MAX_CATALOG {
        return Err(Error::new(ErrorCode::Budget, "artifact catalog is full"));
    }
    dependencies(records, r)?;
    for old in records.values() {
        let a = &old.manifest.spec.artifact_type;
        let b = &r.manifest.spec.artifact_type;
        if a.identity == b.identity && a != b {
            return Err(Error::new(
                ErrorCode::TypeConflict,
                "artifact type version already has another immutable contract",
            ));
        }
    }
    let hash = digest(r)?;
    let prior: String = c
        .query_row("SELECT chain FROM head WHERE id=1", [], |r| r.get(0))
        .map_err(sql)?;
    let n = records.len() as i64 + 1;
    c.execute(
        "INSERT INTO artifacts(sequence,id,document,digest) VALUES(?1,?2,?3,?4)",
        params![
            n,
            r.artifact_id,
            String::from_utf8(to_message(r)?).expect("JSON"),
            hash
        ],
    )
    .map_err(sql)?;
    let changed = c
        .execute(
            "UPDATE head SET sequence=?1,chain=?2 WHERE id=1 AND sequence=?3",
            params![n, digest(&(prior, hash))?, n - 1],
        )
        .map_err(sql)?;
    if changed != 1 {
        return Err(corrupt("artifact head CAS failed"));
    }
    Ok(())
}
