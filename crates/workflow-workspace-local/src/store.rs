use crate::{files::Dir, *};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use workflow_artifacts::{ArtifactLink, ArtifactStore};
const APP: i64 = 1465078577;
const SCHEMA:&str="
CREATE TABLE workspaces(sequence INTEGER PRIMARY KEY CHECK(sequence>0),id TEXT NOT NULL UNIQUE,document TEXT NOT NULL CHECK(length(document)<=2097152),digest TEXT NOT NULL);
CREATE TABLE head(id INTEGER PRIMARY KEY CHECK(id=1),sequence INTEGER NOT NULL CHECK(sequence>=0),chain TEXT NOT NULL);
CREATE TRIGGER immutable_workspace_update BEFORE UPDATE ON workspaces BEGIN SELECT RAISE(ABORT,'immutable workspace allocation'); END;
CREATE TRIGGER immutable_workspace_delete BEFORE DELETE ON workspaces BEGIN SELECT RAISE(ABORT,'workspace retention required'); END;
CREATE TRIGGER immutable_head_delete BEFORE DELETE ON head BEGIN SELECT RAISE(ABORT,'workspace head retained'); END;
CREATE TRIGGER monotonic_head BEFORE UPDATE ON head WHEN NEW.id!=OLD.id OR NEW.sequence!=OLD.sequence+1 BEGIN SELECT RAISE(ABORT,'workspace sequence must advance'); END;
";
static NEXT: AtomicU64 = AtomicU64::new(0);
pub struct LocalWorkspaceStore {
    root: PathBuf,
    dir: Dir,
    pub(crate) connection: Connection,
}
#[derive(Debug, Serialize)]
pub struct CleanupReport {
    pub removed_staging: usize,
    pub removed_uncommitted: usize,
}
fn version(c: &Connection) -> Result<()> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(sql)?;
    let v: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(sql)?;
    if app != APP || v != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "workspace catalog application/schema mismatch",
        ));
    }
    Ok(())
}
fn catalog(c: &Connection) -> Result<BTreeMap<String, WorkspaceRef>> {
    catalog_internal(c, || {})
}
pub(crate) fn catalog_internal(
    c: &Connection,
    after_head: impl FnOnce(),
) -> Result<BTreeMap<String, WorkspaceRef>> {
    // Reuse a caller's write transaction, or hold one read snapshot across the head and rows.
    let snapshot = if c.is_autocommit() {
        Some(c.unchecked_transaction().map_err(sql)?)
    } else {
        None
    };
    version(c)?;
    let (count, expected): (i64, String) = c
        .query_row("SELECT sequence,chain FROM head WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(sql)?;
    if count < 0 || count > MAX_WORKSPACES as i64 {
        return Err(corrupt("workspace count exceeds budget"));
    }
    after_head();
    let mut chain = digest(&"empty workspace catalog")?;
    let mut all = BTreeMap::new();
    let mut total = 0usize;
    let mut q=c.prepare("SELECT sequence,id,length(CAST(document AS BLOB)),document,digest FROM workspaces ORDER BY sequence").map_err(sql)?;
    let mut rows = q.query([]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        let seq: i64 = row.get(0).map_err(sql)?;
        let id: String = row.get(1).map_err(sql)?;
        let len = usize::try_from(row.get::<_, i64>(2).map_err(sql)?)
            .map_err(|_| corrupt("negative catalog length"))?;
        total = total
            .checked_add(len)
            .ok_or_else(|| corrupt("catalog size overflow"))?;
        if seq != all.len() as i64 + 1
            || seq > MAX_WORKSPACES as i64
            || len > 2_097_152
            || total > 64 * 1024 * 1024
        {
            return Err(corrupt("workspace catalog sequence/size exceeds budget"));
        }
        let doc: String = row.get(3).map_err(sql)?;
        let hash: String = row.get(4).map_err(sql)?;
        let r: WorkspaceRef = parse_message(doc.as_bytes()).map_err(|e| corrupt(e.message))?;
        validate_ref(&r).map_err(|e| corrupt(e.message))?;
        if id != r.workspace_id || digest(&r)? != hash {
            return Err(corrupt("workspace catalog digest mismatch"));
        }
        chain = digest(&(chain, hash))?;
        if all.insert(id, r).is_some() {
            return Err(corrupt("duplicate workspace identity"));
        }
    }
    if all.len() as i64 != count || chain != expected {
        return Err(corrupt("workspace catalog differs from retained head"));
    }
    if let Some(snapshot) = snapshot {
        snapshot.commit().map_err(sql)?;
    }
    Ok(all)
}
fn connect(d: &Dir) -> Result<Connection> {
    // Verify the catalog itself through a no-follow descriptor before handing SQLite its host-owned path.
    d.regular("catalog.sqlite")?;
    let c = Connection::open_with_flags(
        std::fs::canonicalize(d.proc_path())
            .map_err(io)?
            .join("catalog.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(sql)?;
    c.busy_timeout(Duration::from_secs(5)).map_err(sql)?;
    c.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
        .map_err(sql)?;
    Ok(c)
}
impl LocalWorkspaceStore {
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let path = root.as_ref();
        if !path.exists() {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let n = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| corrupt("store directory name required"))?;
            Dir::open(parent)?.create(n)?;
        }
        let dir = Dir::open(path)?;
        for n in dir.entries()? {
            if !matches!(
                n.as_str(),
                "catalog.sqlite" | "catalog.sqlite-journal" | "workspaces" | "staging"
            ) {
                return Err(Error::new(
                    ErrorCode::UnsupportedStorage,
                    "refusing foreign nonempty workspace store",
                ));
            }
        }
        for n in ["workspaces", "staging"] {
            if !dir.entries()?.iter().any(|e| e == n) {
                dir.create(n)?;
            }
            dir.child(n)?;
        }
        if !dir.entries()?.iter().any(|e| e == "catalog.sqlite") {
            dir.write("catalog.sqlite", &[], false)?;
        }
        let mut connection = connect(&dir)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(sql)?;
        let v: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(sql)?;
        if app == 0 && v == 0 {
            let count: i64 = tx
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
                .map_err(sql)?;
            if count != 0
                || !dir.child("workspaces")?.entries()?.is_empty()
                || !dir.child("staging")?.entries()?.is_empty()
            {
                return Err(Error::new(
                    ErrorCode::UnsupportedStorage,
                    "foreign or uninitialized workspace data",
                ));
            }
            tx.execute_batch(SCHEMA).map_err(sql)?;
            tx.execute(
                "INSERT INTO head VALUES(1,0,?1)",
                [digest(&"empty workspace catalog")?],
            )
            .map_err(sql)?;
            tx.pragma_update(None, "application_id", APP).map_err(sql)?;
            tx.pragma_update(None, "user_version", 1).map_err(sql)?;
        } else {
            catalog(&tx)?;
        }
        tx.commit().map_err(sql)?;
        dir.sync()?;
        Ok(Self {
            root: std::fs::canonicalize(path).map_err(io)?,
            dir,
            connection,
        })
    }
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let dir = Dir::open(root.as_ref())?;
        let root = std::fs::canonicalize(root).map_err(io)?;
        dir.child("workspaces")?;
        dir.child("staging")?;
        let connection = connect(&dir)?;
        catalog(&connection)?;
        Ok(Self {
            root,
            dir,
            connection,
        })
    }
    pub fn find(&self, id: &str) -> Result<WorkspaceRef> {
        catalog(&self.connection)?
            .remove(id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace allocation missing"))
    }
    fn tree(&self, id: &str) -> Result<Dir> {
        self.dir.child("workspaces")?.child(id)?.child("tree")
    }
    /// Native paths are host bindings; portable references never contain these paths.
    pub fn path(&self, link: &WorkspaceLink) -> Result<PathBuf> {
        self.resolve(link)?;
        self.tree(&link.workspace_id)?;
        let path = self
            .root
            .join("workspaces")
            .join(&link.workspace_id)
            .join("tree");
        let actual = Dir::open(&path)?;
        // Reject a moved/replaced store root rather than exposing an unrelated path.
        use std::os::unix::fs::MetadataExt;
        let left = std::fs::metadata(actual.proc_path()).map_err(io)?;
        let right = std::fs::metadata(self.tree(&link.workspace_id)?.proc_path()).map_err(io)?;
        if left.dev() != right.dev() || left.ino() != right.ino() {
            return Err(corrupt(
                "workspace path no longer names the allocated directory",
            ));
        }
        Ok(path)
    }
    pub(crate) fn checkout_internal(
        &mut self,
        spec: &CheckoutSpec,
        source: &dyn WorkspaceSource,
        hook: impl Fn(&str),
    ) -> Result<WorkspaceRef> {
        validate_spec(spec)?;
        let id = workspace_id(spec)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let all = catalog(&tx)?;
        if let Some(old) = all.get(&id) {
            if &old.manifest.spec != spec {
                return Err(Error::new(
                    ErrorCode::Conflict,
                    "attempt already has a different workspace binding",
                ));
            }
            self.dir.child("workspaces")?.child(&id)?.child("tree")?;
            return Ok(old.clone());
        }
        if all.len() >= MAX_WORKSPACES {
            return Err(Error::new(
                ErrorCode::Budget,
                "workspace allocation limit reached",
            ));
        }
        let source = source.read(&spec.source_revision)?;
        if source.source_revision != spec.source_revision {
            return Err(corrupt("source adapter returned another revision"));
        }
        let baseline: Vec<_> = source
            .files
            .iter()
            .map(|f| FileEntry {
                path: f.path.clone(),
                digest: content_digest(&f.bytes),
                bytes: f.bytes.len() as u64,
                executable: f.executable,
            })
            .collect();
        let reference = reference(WorkspaceManifest {
            spec: spec.clone(),
            git_tree: source.git_tree,
            tree_digest: digest(&baseline)?,
            baseline,
            environment: source.environment,
        })?;
        let doc = to_message(&reference)?;
        let catalog_bytes: i64 = tx
            .query_row(
                "SELECT coalesce(sum(length(CAST(document AS BLOB))),0) FROM workspaces",
                [],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if catalog_bytes < 0 || catalog_bytes as usize + doc.len() > 64 * 1024 * 1024 {
            return Err(Error::new(
                ErrorCode::Budget,
                "workspace catalog exceeds 64 MiB",
            ));
        }
        hook("source_read");
        let staging = self.dir.child("staging")?;
        let ready = self.dir.child("workspaces")?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| corrupt(e.to_string()))?
            .as_nanos();
        let upload = format!(
            "upload-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let dir = staging.create(&upload)?;
        let tree = dir.create("tree")?;
        for file in source.files {
            tree.write(&file.path, &file.bytes, file.executable)?;
            hook("file_written");
        }
        if tree.scan()? != reference.manifest.baseline {
            return Err(Error::new(
                ErrorCode::Changed,
                "checkout files differ before publication",
            ));
        }
        tree.sync()?;
        dir.sync()?;
        hook("tree_synced");
        staging.rename(&upload, &ready, &id)?;
        hook("tree_published");
        let hash = digest(&reference)?;
        tx.execute(
            "INSERT INTO workspaces VALUES(?1,?2,?3,?4)",
            params![
                all.len() as i64 + 1,
                id,
                std::str::from_utf8(&doc).unwrap(),
                hash
            ],
        )
        .map_err(sql)?;
        let chain: String = tx
            .query_row("SELECT chain FROM head WHERE id=1", [], |r| r.get(0))
            .map_err(sql)?;
        tx.execute(
            "UPDATE head SET sequence=sequence+1,chain=?1 WHERE id=1",
            [digest(&(chain, hash))?],
        )
        .map_err(sql)?;
        hook("manifest_written");
        tx.commit().map_err(sql)?;
        hook("after_commit");
        Ok(reference)
    }
    pub(crate) fn capture_internal(
        &mut self,
        link: &WorkspaceLink,
        artifacts: &mut dyn ArtifactStore,
        hook: impl Fn(&str),
    ) -> Result<Capture> {
        let reference = self.resolve(link)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        catalog(&tx)?;
        let tree = self
            .dir
            .child("workspaces")?
            .child(&reference.workspace_id)?
            .child("tree")?;
        let observed = observation(&reference, tree.scan()?)?;
        let mut outputs = vec![];
        let mut refs = BTreeMap::new();
        for output in &reference.manifest.spec.outputs {
            let path = &output.path;
            let expected = observed
                .files
                .iter()
                .find(|f| &f.path == path)
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::NotFound,
                        format!("declared output missing: {path}"),
                    )
                })?;
            let (bytes, executable) = tree.read(path)?;
            if content_digest(&bytes) != expected.digest || executable != expected.executable {
                return Err(Error::new(
                    ErrorCode::Changed,
                    "output changed during capture",
                ));
            }
            let spec = publish_spec(&reference.manifest.spec, output.artifact_type.clone());
            let expected_ref = workflow_artifacts::reference(&spec, &bytes)?;
            let r = artifacts.publish(&spec, &mut bytes.as_slice())?;
            if r != expected_ref {
                return Err(corrupt(
                    "artifact adapter returned another output reference",
                ));
            }
            outputs.push(CapturedFile {
                path: path.clone(),
                executable,
                artifact_id: r.artifact_id.clone(),
                digest: r.digest.clone(),
            });
            refs.insert(r.artifact_id.clone(), r);
        }
        hook("outputs_published");
        if tree.scan()? != observed.files {
            return Err(Error::new(
                ErrorCode::Changed,
                "workspace changed before output manifest publication",
            ));
        }
        outputs.sort_by(|a, b| a.path.cmp(&b.path));
        let payload = OutputManifest {
            workspace_id: reference.workspace_id.clone(),
            workspace_digest: reference.digest.clone(),
            baseline_tree_digest: reference.manifest.tree_digest.clone(),
            observed_tree_digest: observed.tree_digest.clone(),
            clean: observed.clean,
            files: outputs,
        };
        let bytes = to_message(&payload)?;
        let mut spec = publish_spec(&reference.manifest.spec, output_type());
        let mut inputs: BTreeMap<_, _> = spec
            .inputs
            .iter()
            .map(|r| (r.artifact_id.clone(), r.clone()))
            .collect();
        for r in refs.values() {
            inputs.insert(
                r.artifact_id.clone(),
                ArtifactLink {
                    artifact_id: r.artifact_id.clone(),
                    digest: r.digest.clone(),
                },
            );
        }
        spec.inputs = inputs.into_values().collect();
        let capture = Capture {
            observation: observed,
            manifest: workflow_artifacts::reference(&spec, &bytes)?,
            files: refs.into_values().collect(),
        };
        to_message(&capture)?;
        let manifest = artifacts.publish(&spec, &mut bytes.as_slice())?;
        if manifest != capture.manifest {
            return Err(corrupt("artifact adapter returned another manifest"));
        }
        hook("manifest_published");
        tx.commit().map_err(sql)?;
        Ok(capture)
    }
    pub fn cleanup_orphans(&mut self) -> Result<CleanupReport> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let all = catalog(&tx)?;
        let ready = self.dir.child("workspaces")?;
        let staging = self.dir.child("staging")?;
        for id in all.keys() {
            ready.child(id)?.child("tree")?;
        }
        let mut report = CleanupReport {
            removed_staging: 0,
            removed_uncommitted: 0,
        };
        for n in staging.entries()? {
            if !n.strip_prefix("upload-").is_some_and(|s| {
                !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-')
            }) {
                return Err(corrupt("foreign staging entry"));
            }
            staging.remove_tree(&n)?;
            report.removed_staging += 1;
        }
        for n in ready.entries()? {
            validate_link(&WorkspaceLink {
                workspace_id: n.clone(),
                digest: format!("sha256:{}", "0".repeat(64)),
            })?;
            if !all.contains_key(&n) {
                ready.remove_tree(&n)?;
                report.removed_uncommitted += 1;
            }
        }
        tx.commit().map_err(sql)?;
        Ok(report)
    }
}
impl WorkspaceStore for LocalWorkspaceStore {
    fn checkout(
        &mut self,
        spec: &CheckoutSpec,
        source: &dyn WorkspaceSource,
    ) -> Result<WorkspaceRef> {
        self.checkout_internal(spec, source, |_| {})
    }
    fn resolve(&self, link: &WorkspaceLink) -> Result<WorkspaceRef> {
        validate_link(link)?;
        let r = self.find(&link.workspace_id)?;
        if r.link() != *link {
            return Err(Error::new(
                ErrorCode::InvalidReference,
                "workspace digest mismatch",
            ));
        }
        self.tree(&r.workspace_id)?;
        Ok(r)
    }
    fn observe(&self, link: &WorkspaceLink) -> Result<Observation> {
        let r = self.resolve(link)?;
        let tree = self.tree(&r.workspace_id)?;
        let files = tree.scan()?;
        if tree.scan()? != files {
            return Err(Error::new(
                ErrorCode::Changed,
                "workspace changed during observation",
            ));
        }
        observation(&r, files)
    }
    fn capture(
        &mut self,
        link: &WorkspaceLink,
        artifacts: &mut dyn ArtifactStore,
    ) -> Result<Capture> {
        self.capture_internal(link, artifacts, |_| {})
    }
}
