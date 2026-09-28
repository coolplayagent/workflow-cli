//! Bounded, data-only transaction images for an external authoritative store.
//! SQL, native database pages and user-selected table names are never imported.
use crate::*;
use rusqlite::{params_from_iter, types::Value};
use serde::{Deserialize, Serialize};

pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
// Order respects foreign keys. Columns are fixed by STORAGE_VERSION, not input.
const TABLES: &[(&str, usize)] = &[
    ("bundles", 2),
    ("binding_locks", 4),
    ("runs", 5),
    ("heads", 4),
    ("events", 5),
    ("checkpoints", 4),
    ("outbox", 7),
    ("delivery_heads", 3),
    ("receipts", 4),
    ("execution_heads", 3),
    ("execution_events", 4),
];

/// The external transaction must atomically admit its write inside this window.
/// This is a runtime obligation, never trusted from a serialized image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionWindow {
    pub not_before_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}
pub(crate) fn record_admission(
    cell: &std::cell::Cell<Option<AdmissionWindow>>,
    start: u64,
    end: u64,
) {
    let prior = cell.get().unwrap_or(AdmissionWindow {
        not_before_unix_ms: 0,
        expires_at_unix_ms: u64::MAX,
    });
    cell.set(Some(AdmissionWindow {
        not_before_unix_ms: prior.not_before_unix_ms.max(start),
        expires_at_unix_ms: prior.expires_at_unix_ms.min(end),
    }));
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Cell {
    Integer(i64),
    Text(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunImage {
    schema_version: i64,
    run_id: String,
    tables: Vec<Vec<Vec<Cell>>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageBinding {
    pub kind: String,
    pub id: String,
    pub version: String,
    pub digest: String,
}
impl RunImage {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(corrupt("run image exceeds 64 MiB"));
        }
        let image: Self =
            serde_json::from_slice(bytes).map_err(|_| corrupt("invalid run image"))?;
        if image.schema_version != STORAGE_VERSION || image.tables.len() != TABLES.len() {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "unsupported run image schema",
            ));
        }
        validate_id(&image.run_id)?;
        for (table, (_, columns)) in image.tables.iter().zip(TABLES) {
            if table.iter().any(|r| r.len() != *columns) {
                return Err(corrupt("run image column mismatch"));
            }
        }
        Ok(image)
    }
    pub fn bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| corrupt("run image encoding failed"))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(Error::new(ErrorCode::Storage, "run image exceeds 64 MiB"));
        }
        Ok(bytes)
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn bindings(&self) -> Result<Vec<ImageBinding>> {
        self.tables[1]
            .iter()
            .map(|row| {
                let fields = row
                    .iter()
                    .map(|c| match c {
                        Cell::Text(s) => Ok(s.clone()),
                        _ => Err(corrupt("invalid image binding")),
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(ImageBinding {
                    kind: fields[0].clone(),
                    id: fields[1].clone(),
                    version: fields[2].clone(),
                    digest: fields[3].clone(),
                })
            })
            .collect()
    }
}
impl SqliteRunStore {
    /// Volatile reducer only: success is not a durable receipt until the host
    /// atomically stores the image under its ownership and admission checks.
    pub fn image_reducer(
        artifacts: Option<Box<dyn workflow_artifacts::ArtifactReader>>,
    ) -> Result<Self> {
        let connection = Connection::open_in_memory().map_err(storage)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage)?;
        connection.execute_batch(SCHEMA).map_err(storage)?;
        connection
            .execute_batch(execution::SCHEMA)
            .map_err(storage)?;
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(storage)?;
        connection
            .pragma_update(None, "user_version", STORAGE_VERSION)
            .map_err(storage)?;
        Ok(Self {
            connection,
            artifacts,
            admission: Default::default(),
        })
    }
    /// Rebuild only the compiled schema and replay both journals before use.
    /// An image is data; it cannot install triggers or change executable SQL.
    pub fn from_image(
        image: &RunImage,
        artifacts: Option<Box<dyn workflow_artifacts::ArtifactReader>>,
    ) -> Result<Self> {
        // Validate even images created by this crate; do not rely on parse callers.
        let image = RunImage::parse(&image.bytes()?)?;
        let mut store = Self::image_reducer(artifacts)?;
        let tx = store.connection.transaction().map_err(storage)?;
        for ((name, columns), rows) in TABLES.iter().zip(&image.tables) {
            let placeholders = vec!["?"; *columns].join(",");
            let mut stmt = tx
                .prepare(&format!("INSERT INTO {name} VALUES({placeholders})"))
                .map_err(storage)?;
            for row in rows {
                let values = row.iter().map(|c| match c {
                    Cell::Integer(n) => Value::Integer(*n),
                    Cell::Text(s) => Value::Text(s.clone()),
                });
                stmt.execute(params_from_iter(values))
                    .map_err(|_| corrupt("invalid run image rows"))?;
            }
        }
        tx.commit().map_err(storage)?;
        Self::check_single_run(&store.connection, &image.run_id)?;
        store.verify(&image.run_id)?;
        store.execution_history(&image.run_id, 0, 1)?;
        Ok(store)
    }
    fn check_single_run(connection: &Connection, id: &str) -> Result<()> {
        let ids = connection
            .prepare("SELECT run_id FROM runs")
            .map_err(storage)?
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(storage)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage)?;
        if ids != [id] {
            return Err(corrupt("transaction image requires exactly the named run"));
        }
        Ok(())
    }
    pub fn export_image(&mut self, id: &str) -> Result<RunImage> {
        let tx = self.connection.transaction().map_err(storage)?;
        Self::check_single_run(&tx, id)?;
        let recovered = crate::recovery::recover(&tx, id, self.artifacts.as_deref())?;
        execution::read(&tx, &recovered, self.artifacts.as_deref())?;
        let mut tables = vec![];
        for (name, columns) in TABLES {
            let order = (1..=*columns)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut q = tx
                .prepare(&format!("SELECT * FROM {name} ORDER BY {order}"))
                .map_err(storage)?;
            let mut rows = q.query([]).map_err(storage)?;
            let mut table = vec![];
            while let Some(row) = rows.next().map_err(storage)? {
                let mut cells = vec![];
                for column in 0..*columns {
                    cells.push(match row.get::<_, Value>(column).map_err(storage)? {
                        Value::Integer(n) => Cell::Integer(n),
                        Value::Text(s) => Cell::Text(s),
                        _ => return Err(corrupt("unsupported image cell type")),
                    });
                }
                table.push(cells);
            }
            tables.push(table);
        }
        tx.commit().map_err(storage)?;
        let image = RunImage {
            schema_version: STORAGE_VERSION,
            run_id: id.into(),
            tables,
        };
        image.bytes()?;
        Ok(image)
    }
    /// Drain constraints accumulated by successful reducer mutations. Failed
    /// mutations must discard the reducer. The host also checks callback windows.
    pub fn take_admission_window(&self) -> Option<AdmissionWindow> {
        self.admission.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> StartRun {
        let root = if let Ok(root) = std::env::var("TEST_SRCDIR") {
            std::path::PathBuf::from(root).join(std::env::var("TEST_WORKSPACE").unwrap())
        } else {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
        };
        serde_json::from_slice(
            &std::fs::read(root.join("examples/runs/review-start.json")).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn image_preserves_both_journals_and_rejects_corruption_or_multiple_runs() {
        let r = request();
        let mut reducer = SqliteRunStore::image_reducer(None).unwrap();
        let mut actual = reducer
            .connection
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let mut expected: Vec<_> = TABLES.iter().map(|(name, _)| name.to_string()).collect();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "schema evolution must update transaction image coverage"
        );
        reducer.start(&r).unwrap();
        struct Time(u64);
        impl workflow_worker::Clock for Time {
            fn now_unix_ms(&self) -> workflow_worker::Result<u64> {
                Ok(self.0)
            }
        }
        let lease = reducer
            .acquire(
                &LeaseRequest {
                    run_id: r.run_id.clone(),
                    owner: "test".into(),
                    acquisition_id: "first".into(),
                    ttl_ms: 1000,
                },
                &Time(r.started_at_unix_ms),
            )
            .unwrap();
        assert_eq!(
            reducer.take_admission_window(),
            Some(AdmissionWindow {
                not_before_unix_ms: r.started_at_unix_ms,
                expires_at_unix_ms: lease.expires_at_unix_ms
            })
        );
        let image = reducer.export_image(&r.run_id).unwrap();
        let bytes = image.bytes().unwrap();
        let mut restored =
            SqliteRunStore::from_image(&RunImage::parse(&bytes).unwrap(), None).unwrap();
        assert_eq!(
            restored.get(&r.run_id).unwrap(),
            reducer.get(&r.run_id).unwrap()
        );
        assert_eq!(
            restored.execution_history(&r.run_id, 0, 100).unwrap(),
            reducer.execution_history(&r.run_id, 0, 100).unwrap()
        );
        assert_eq!(
            restored.export_image(&r.run_id).unwrap().bytes().unwrap(),
            bytes
        );
        let mut corrupt = image.clone();
        corrupt.tables[10].clear();
        assert!(SqliteRunStore::from_image(&corrupt, None).is_err());
        let mut corrupt = image.clone();
        corrupt.run_id = "different".into();
        assert!(SqliteRunStore::from_image(&corrupt, None).is_err());
        let mut corrupt = image.clone();
        corrupt.tables[0][0].push(Cell::Text("extra".into()));
        assert!(SqliteRunStore::from_image(&corrupt, None).is_err());
        let mut second = r.clone();
        second.run_id = "second".into();
        reducer.start(&second).unwrap();
        assert!(reducer.export_image(&r.run_id).is_err());
        assert!(RunImage::parse(&vec![b' '; MAX_IMAGE_BYTES + 1]).is_err());
    }
}
