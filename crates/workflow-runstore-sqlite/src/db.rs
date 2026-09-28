use crate::*;
use rusqlite::{Connection, OpenFlags};
use serde::{Serialize, de::DeserializeOwned};
use std::{path::Path, time::Duration};
pub(crate) const APPLICATION_ID: i64 = 0x57465231;
pub(crate) const STORAGE_VERSION: i64 = 6;
pub(crate) const SCHEMA: &str = "
CREATE TABLE bundles (digest TEXT PRIMARY KEY NOT NULL, document TEXT NOT NULL);
CREATE TABLE binding_locks (
 kind TEXT NOT NULL,
 id TEXT NOT NULL,
 version TEXT NOT NULL,
 digest TEXT NOT NULL,
 PRIMARY KEY(kind,id,version)
);
CREATE TABLE runs (
 run_id TEXT PRIMARY KEY NOT NULL,
 run_digest TEXT NOT NULL UNIQUE,
 bundle_digest TEXT NOT NULL REFERENCES bundles(digest),
 seed TEXT NOT NULL,
 seed_digest TEXT NOT NULL
);
CREATE TABLE heads (
 run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
 revision INTEGER NOT NULL CHECK(revision>0),
 snapshot TEXT NOT NULL,
 state_digest TEXT NOT NULL
);
CREATE TABLE events (
 run_id TEXT NOT NULL REFERENCES runs(run_id),
 revision INTEGER NOT NULL CHECK(revision>1),
 event_id TEXT NOT NULL,
 document TEXT NOT NULL,
 digest TEXT NOT NULL,
 PRIMARY KEY(run_id,revision), UNIQUE(run_id,event_id)
);
CREATE TABLE checkpoints (
 run_id TEXT NOT NULL REFERENCES runs(run_id),
 revision INTEGER NOT NULL CHECK(revision>0),
 document TEXT NOT NULL,
 digest TEXT NOT NULL,
 PRIMARY KEY(run_id,revision)
);
CREATE TABLE outbox (
 run_id TEXT NOT NULL REFERENCES runs(run_id),
 sequence INTEGER NOT NULL CHECK(sequence>0),
 revision INTEGER NOT NULL CHECK(revision>0),
 command_index INTEGER NOT NULL CHECK(command_index>=0),
 command_id TEXT NOT NULL UNIQUE,
 document TEXT NOT NULL,
 digest TEXT NOT NULL,
 PRIMARY KEY(run_id,sequence), UNIQUE(run_id,revision,command_index)
);
CREATE TABLE delivery_heads (
 run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
 sequence INTEGER NOT NULL CHECK(sequence>=0),
 chain_digest TEXT NOT NULL
);
CREATE TABLE receipts (
 run_id TEXT NOT NULL,
 sequence INTEGER NOT NULL,
 document TEXT NOT NULL,
 digest TEXT NOT NULL,
 PRIMARY KEY(run_id,sequence),
 FOREIGN KEY(run_id,sequence) REFERENCES outbox(run_id,sequence)
);
CREATE TRIGGER immutable_bindings_update BEFORE UPDATE ON binding_locks BEGIN SELECT RAISE(ABORT,'immutable binding'); END;
CREATE TRIGGER immutable_bindings_delete BEFORE DELETE ON binding_locks BEGIN SELECT RAISE(ABORT,'immutable binding'); END;
CREATE TRIGGER immutable_runs_update BEFORE UPDATE ON runs BEGIN SELECT RAISE(ABORT,'immutable run seed'); END;
CREATE TRIGGER immutable_runs_delete BEFORE DELETE ON runs BEGIN SELECT RAISE(ABORT,'immutable run seed'); END;
CREATE TRIGGER immutable_bundles_update BEFORE UPDATE ON bundles BEGIN SELECT RAISE(ABORT,'immutable bundle'); END;
CREATE TRIGGER immutable_bundles_delete BEFORE DELETE ON bundles BEGIN SELECT RAISE(ABORT,'immutable bundle'); END;
CREATE TRIGGER immutable_events_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT,'immutable event'); END;
CREATE TRIGGER immutable_events_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT,'immutable event'); END;
CREATE TRIGGER immutable_checkpoints_update BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(ABORT,'immutable checkpoint'); END;
CREATE TRIGGER immutable_checkpoints_delete BEFORE DELETE ON checkpoints BEGIN SELECT RAISE(ABORT,'immutable checkpoint'); END;
CREATE TRIGGER immutable_outbox_update BEFORE UPDATE ON outbox BEGIN SELECT RAISE(ABORT,'immutable command'); END;
CREATE TRIGGER immutable_outbox_delete BEFORE DELETE ON outbox BEGIN SELECT RAISE(ABORT,'immutable command'); END;
CREATE TRIGGER immutable_receipts_update BEFORE UPDATE ON receipts BEGIN SELECT RAISE(ABORT,'immutable receipt'); END;
CREATE TRIGGER immutable_receipts_delete BEFORE DELETE ON receipts BEGIN SELECT RAISE(ABORT,'immutable receipt'); END;
CREATE TRIGGER immutable_delivery_delete BEFORE DELETE ON delivery_heads BEGIN SELECT RAISE(ABORT,'delivery deletion is unsupported'); END;
CREATE TRIGGER monotonic_delivery BEFORE UPDATE ON delivery_heads WHEN NEW.run_id!=OLD.run_id OR NEW.sequence!=OLD.sequence+1 BEGIN SELECT RAISE(ABORT,'delivery must advance one sequence'); END;
CREATE TRIGGER monotonic_heads BEFORE UPDATE ON heads WHEN NEW.run_id!=OLD.run_id OR NEW.revision!=OLD.revision+1 BEGIN SELECT RAISE(ABORT,'head must advance one revision'); END;
CREATE TRIGGER immutable_heads_delete BEFORE DELETE ON heads BEGIN SELECT RAISE(ABORT,'run deletion is unsupported'); END;
";
pub(crate) fn connect(path: &Path, flags: OpenFlags) -> Result<Connection> {
    if path.as_os_str().is_empty() || path == Path::new(":memory:") {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "run store requires a persistent filesystem path",
        ));
    }
    let c = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(storage)?;
    c.busy_timeout(Duration::from_secs(5)).map_err(storage)?;
    c.pragma_update(None, "foreign_keys", "ON")
        .map_err(storage)?;
    if !flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        c.pragma_update(None, "synchronous", "FULL")
            .map_err(storage)?;
    }
    Ok(c)
}
pub(crate) fn check_version(c: &Connection) -> Result<()> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(storage)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if app != APPLICATION_ID || version != STORAGE_VERSION {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            format!(
                "expected run store application {APPLICATION_ID} schema {STORAGE_VERSION}, found {app}/{version}"
            ),
        ));
    }
    Ok(())
}
pub(crate) fn storage(e: rusqlite::Error) -> Error {
    let code = match &e {
        rusqlite::Error::SqliteFailure(s, _)
            if matches!(
                s.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            ErrorCode::Busy
        }
        rusqlite::Error::SqliteFailure(s, _)
            if matches!(
                s.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            ) =>
        {
            ErrorCode::CorruptStorage
        }
        _ => ErrorCode::Storage,
    };
    Error::new(code, e.to_string())
}
pub(crate) fn corrupt(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::CorruptStorage, message)
}
pub(crate) fn document<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(workflow_worker::to_message(value)?).expect("JSON UTF-8"))
}
pub(crate) fn digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(workflow_worker::digest(value)?)
}
pub(crate) fn decode<T: Serialize + DeserializeOwned>(text: &str, expected: &str) -> Result<T> {
    let value: T =
        workflow_worker::parse_message(text.as_bytes()).map_err(|e| corrupt(e.message))?;
    if digest(&value)? != expected {
        return Err(corrupt("stored document digest mismatch"));
    }
    Ok(value)
}
pub(crate) fn number(n: u64) -> Result<i64> {
    i64::try_from(n)
        .map_err(|_| Error::new(ErrorCode::InvalidRequest, "integer exceeds storage range"))
}
