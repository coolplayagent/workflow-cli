use crate::*;
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
pub(crate) fn snapshot(source: &Path, destination: &Path) -> Result<u32> {
    files::regular(source)?;
    let mut source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(files::io)?;
    source
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(files::io)?;
    let tx = source.transaction().map_err(files::io)?;
    // Pin a read snapshot before measuring/copying; later WAL commits cannot
    // change this image. In rollback mode writers may wait for this bounded copy.
    let pages: i64 = tx
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .map_err(files::io)?;
    let size: i64 = tx
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(files::io)?;
    if pages
        .checked_mul(size)
        .is_none_or(|n| n <= 0 || n > MAX_DATABASE_BYTES as i64)
    {
        return Err(Error::new(
            ErrorCode::Budget,
            "database snapshot exceeds 256 MiB",
        ));
    }
    let version = tx
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(files::io)?;
    files::create_file(destination)?
        .sync_all()
        .map_err(files::io)?;
    let mut dest = Connection::open_with_flags(destination, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(files::io)?;
    dest.pragma_update(None, "synchronous", "FULL")
        .map_err(files::io)?;
    {
        let backup = Backup::new(&tx, &mut dest).map_err(files::io)?;
        match backup.step(-1).map_err(files::io)? {
            StepResult::Done => {}
            _ => {
                return Err(Error::new(
                    ErrorCode::Storage,
                    "SQLite snapshot was busy or incomplete",
                ));
            }
        }
    }
    drop(dest);
    tx.commit().map_err(files::io)?;
    std::fs::File::open(destination)
        .map_err(files::io)?
        .sync_all()
        .map_err(files::io)?;
    verify(destination)?;
    Ok(version)
}
pub(crate) fn verify(path: &Path) -> Result<u32> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(files::corrupt)?;
    let checks: Vec<String> = c
        .prepare("PRAGMA integrity_check")
        .map_err(files::corrupt)?
        .query_map([], |r| r.get(0))
        .map_err(files::corrupt)?
        .collect::<std::result::Result<_, _>>()
        .map_err(files::corrupt)?;
    if checks != ["ok"] {
        return Err(files::corrupt("SQLite backup integrity check failed"));
    }
    if c.prepare("PRAGMA foreign_key_check")
        .map_err(files::corrupt)?
        .query([])
        .map_err(files::corrupt)?
        .next()
        .map_err(files::corrupt)?
        .is_some()
    {
        return Err(files::corrupt("SQLite backup foreign key check failed"));
    }
    c.pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(files::corrupt)
}
