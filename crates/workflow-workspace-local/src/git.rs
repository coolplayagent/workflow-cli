use crate::*;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use workflow_artifacts::SourceRevision;
/// Host config maps a logical repository identity to a local object database.
/// The editable source checkout and its index never supply workspace bytes.
pub struct GitSource {
    repository: String,
    path: PathBuf,
}
impl GitSource {
    pub fn open(repository: &str, path: impl AsRef<Path>) -> Result<Self> {
        if repository.trim().is_empty() || repository.len() > 1024 {
            return Err(Error::new(
                ErrorCode::InvalidContract,
                "repository identity required",
            ));
        }
        Ok(Self {
            repository: repository.into(),
            path: std::fs::canonicalize(path).map_err(io)?,
        })
    }

    /// Export a reviewed merge as an actual deterministic Git commit in a new
    /// bare repository. No caller checkout, index or existing ref is mutated.
    /// Source ancestry is in the retained merge plan; this is a new root commit.
    pub fn write_merge(
        path: impl AsRef<Path>,
        plan: &MergePlan,
        files: &[SourceFile],
    ) -> Result<workflow_artifacts::SourceRevision> {
        let entries: Vec<_> = files
            .iter()
            .map(|f| FileEntry {
                path: f.path.clone(),
                digest: content_digest(&f.bytes),
                bytes: f.bytes.len() as u64,
                executable: f.executable,
            })
            .collect();
        validate_files(&entries)?;
        if entries != plan.files || !plan.conflicts.is_empty() || !plan.requires_revalidation {
            return Err(Error::new(
                ErrorCode::Conflict,
                "merge content differs from reviewed plan",
            ));
        }
        // create_dir rejects an existing destination, including symlinks.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path.as_ref())
            .map_err(io)?;
        let source = Self::open(&plan.source_revision.repository, path)?;
        source.command(
            &["init", "--bare", "--object-format=sha256", "--template="],
            vec![],
            4096,
        )?;
        let mut stream = vec![];
        for (index, file) in files.iter().enumerate() {
            stream.extend_from_slice(
                format!("blob\nmark :{}\ndata {}\n", index + 1, file.bytes.len()).as_bytes(),
            );
            stream.extend_from_slice(&file.bytes);
            stream.push(b'\n');
        }
        let message = format!(
            "Workflow merge plan {}\nBase repository {}\nBase revision {}\nRevalidation required\n",
            digest(plan)?,
            plan.source_revision.repository,
            plan.source_revision.revision
        );
        stream.extend_from_slice(format!("commit refs/heads/merged\ncommitter Workflow Merge <merge@workflow.invalid> 0 +0000\ndata {}\n{}\n",message.len(),message).as_bytes());
        for (index, file) in files.iter().enumerate() {
            let quoted = serde_json::to_string(&file.path).map_err(|e| corrupt(e.to_string()))?;
            stream.extend_from_slice(
                format!(
                    "M {} :{} {quoted}\n",
                    if file.executable { "100755" } else { "100644" },
                    index + 1
                )
                .as_bytes(),
            );
        }
        stream.extend_from_slice(b"\ndone\n");
        source.command(
            &["fast-import", "--date-format=raw", "--done", "--quiet"],
            stream,
            4096,
        )?;
        let revision =
            String::from_utf8(source.command(&["rev-parse", "refs/heads/merged"], vec![], 128)?)
                .map_err(|_| corrupt("invalid merged revision"))?
                .trim()
                .to_owned();
        let reference = workflow_artifacts::SourceRevision {
            repository: plan.source_revision.repository.clone(),
            revision,
        };
        let checked = source.read(&reference)?;
        if checked.files.len() != files.len()
            || checked.files.iter().zip(files).any(|(a, b)| {
                a.path != b.path || a.bytes != b.bytes || a.executable != b.executable
            })
        {
            return Err(corrupt("Git commit differs from merged file set"));
        }
        // Git fast-import has closed its pack/ref files. Sync every regular file
        // and directory before returning a revision to the host.
        fn sync(path: &Path) -> Result<()> {
            for entry in std::fs::read_dir(path).map_err(io)? {
                let entry = entry.map_err(io)?;
                let kind = entry.file_type().map_err(io)?;
                if kind.is_dir() {
                    sync(&entry.path())?;
                } else if kind.is_file() {
                    std::fs::File::open(entry.path())
                        .map_err(io)?
                        .sync_all()
                        .map_err(io)?;
                } else {
                    return Err(corrupt("unexpected merged repository entry"));
                }
            }
            std::fs::File::open(path)
                .map_err(io)?
                .sync_all()
                .map_err(io)
        }
        sync(&source.path)?;
        if let Some(parent) = source.path.parent() {
            std::fs::File::open(parent)
                .map_err(io)?
                .sync_all()
                .map_err(io)?;
        }
        Ok(reference)
    }
    fn command(&self, args: &[&str], input: Vec<u8>, limit: usize) -> Result<Vec<u8>> {
        let mut cmd = Command::new("git");
        // Git reads object bytes only. Provider/database credentials and loader,
        // proxy or shell variables have no purpose in this subprocess.
        cmd.env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("LC_ALL", "C")
            .arg("--no-replace-objects")
            .arg("--no-optional-locks")
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .arg("-C")
            .arg(&self.path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(io)?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let writer = thread::spawn(move || stdin.write_all(&input));
        let reader = thread::spawn(move || {
            let mut b = vec![];
            stdout.take(limit as u64 + 1).read_to_end(&mut b).map(|_| b)
        });
        let errors = thread::spawn(move || {
            let mut b = vec![];
            stderr.take(16_385).read_to_end(&mut b).map(|_| b)
        });
        let started = Instant::now();
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait().map_err(io)? {
                break s;
            }
            if started.elapsed() > Duration::from_secs(60) {
                timed_out = true;
                let _ = child.kill();
                break child.wait().map_err(io)?;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let sent = writer
            .join()
            .map_err(|_| corrupt("Git input thread failed"))?;
        let bytes = reader
            .join()
            .map_err(|_| corrupt("Git output thread failed"))?
            .map_err(io)?;
        let err = errors
            .join()
            .map_err(|_| corrupt("Git error thread failed"))?
            .map_err(io)?;
        if timed_out {
            return Err(Error::new(
                ErrorCode::Timeout,
                "Git source read exceeded 60 seconds",
            ));
        }
        if bytes.len() > limit || err.len() > 16_384 {
            return Err(Error::new(ErrorCode::Budget, "Git output budget exceeded"));
        }
        if !status.success() {
            return Err(Error::new(
                ErrorCode::UnsupportedSource,
                format!("Git source rejected: {}", String::from_utf8_lossy(&err)),
            ));
        }
        sent.map_err(io)?;
        Ok(bytes)
    }
}
fn object_id(kind: &str, bytes: &[u8], sha256: bool) -> String {
    use sha2::Digest;
    let header = format!("{kind} {}\0", bytes.len());
    if sha256 {
        let mut hash = sha2::Sha256::new();
        hash.update(header.as_bytes());
        hash.update(bytes);
        format!("{:x}", hash.finalize())
    } else {
        let mut hash = sha1::Sha1::new();
        hash.update(header.as_bytes());
        hash.update(bytes);
        format!("{:x}", hash.finalize())
    }
}
impl GitSource {
    fn objects(&self, objects: &[(String, &'static str)], limit: usize) -> Result<Vec<Vec<u8>>> {
        if objects.is_empty() {
            return Ok(vec![]);
        }
        let input = objects
            .iter()
            .map(|(id, _)| format!("{id}\n"))
            .collect::<String>()
            .into_bytes();
        let raw = self.command(&["cat-file", "--batch"], input, limit)?;
        let mut cursor = 0usize;
        let mut result = vec![];
        for (id, kind) in objects {
            let end = raw[cursor..]
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| corrupt("Git batch header missing"))?
                + cursor;
            let header = std::str::from_utf8(&raw[cursor..end])
                .map_err(|_| corrupt("invalid Git header"))?;
            let fields: Vec<_> = header.split(' ').collect();
            if fields.len() != 3 || fields[0] != id || fields[1] != *kind {
                return Err(Error::new(
                    ErrorCode::UnsupportedSource,
                    "exact full object identity/type required",
                ));
            }
            let size: usize = fields[2]
                .parse()
                .map_err(|_| corrupt("invalid Git object size"))?;
            if *kind == "blob" && size as u64 > MAX_FILE_BYTES {
                return Err(Error::new(ErrorCode::Budget, "source file exceeds 64 MiB"));
            }
            cursor = end + 1;
            let end = cursor
                .checked_add(size)
                .filter(|e| *e < raw.len())
                .ok_or_else(|| corrupt("truncated Git object"))?;
            let bytes = &raw[cursor..end];
            if raw[end] != b'\n' || object_id(kind, bytes, id.len() == 64) != *id {
                return Err(corrupt(
                    "Git object bytes do not match the committed object ID",
                ));
            }
            result.push(bytes.to_vec());
            cursor = end + 1;
        }
        if cursor != raw.len() {
            return Err(corrupt("unexpected Git batch bytes"));
        }
        Ok(result)
    }
}
impl WorkspaceSource for GitSource {
    fn read(&self, source: &SourceRevision) -> Result<SourceTree> {
        if source.repository != self.repository || !valid_oid(&source.revision) {
            return Err(Error::new(
                ErrorCode::InvalidContract,
                "source must match the configured repository and full commit ID",
            ));
        }
        let commits = self.objects(&[(source.revision.clone(), "commit")], 1_048_576)?;
        let first = commits[0].split(|b| *b == b'\n').next().unwrap();
        let git_tree = std::str::from_utf8(first)
            .ok()
            .and_then(|s| s.strip_prefix("tree "))
            .filter(|s| valid_oid(s) && s.len() == source.revision.len())
            .ok_or_else(|| corrupt("commit tree identity missing"))?
            .to_owned();
        let mut pending = vec![(String::new(), git_tree.clone())];
        let mut blobs = vec![];
        let mut directories = 0;
        let mut metadata_bytes = 0;
        let oid_bytes = source.revision.len() / 2;
        while !pending.is_empty() {
            directories += pending.len();
            if directories > MAX_FILES {
                return Err(Error::new(
                    ErrorCode::Budget,
                    "source directory budget exceeded",
                ));
            }
            let requests = pending
                .iter()
                .map(|(_, id)| (id.clone(), "tree"))
                .collect::<Vec<_>>();
            let trees = self.objects(&requests, 4_194_304)?;
            let mut next = vec![];
            for ((prefix, _), raw) in pending.into_iter().zip(trees) {
                metadata_bytes += raw.len();
                if metadata_bytes > 4_194_304 {
                    return Err(Error::new(
                        ErrorCode::Budget,
                        "source tree metadata exceeds 4 MiB",
                    ));
                }
                let mut cursor = 0;
                while cursor < raw.len() {
                    let end = raw[cursor..]
                        .iter()
                        .position(|b| *b == 0)
                        .ok_or_else(|| corrupt("tree entry terminator missing"))?
                        + cursor;
                    let entry = std::str::from_utf8(&raw[cursor..end]).map_err(|_| {
                        Error::new(ErrorCode::UnsupportedSource, "source paths must be UTF-8")
                    })?;
                    let (mode, name) = entry
                        .split_once(' ')
                        .ok_or_else(|| corrupt("tree entry mode missing"))?;
                    if name.contains('/') {
                        return Err(corrupt("Git tree entry contains slash"));
                    }
                    let path = if prefix.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{prefix}/{name}")
                    };
                    validate_path(&path)?;
                    cursor = end + 1;
                    let end = cursor
                        .checked_add(oid_bytes)
                        .filter(|e| *e <= raw.len())
                        .ok_or_else(|| corrupt("truncated tree object ID"))?;
                    let id = raw[cursor..end]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>();
                    cursor = end;
                    match mode {
                        "40000" => next.push((path, id)),
                        "100644" | "100755" => blobs.push((path, id, mode == "100755")),
                        _ => {
                            return Err(Error::new(
                                ErrorCode::UnsupportedSource,
                                "symlinks, submodules and nonregular Git modes require a separate source policy",
                            ));
                        }
                    }
                    if blobs.len() > MAX_FILES || next.len() > MAX_FILES {
                        return Err(Error::new(
                            ErrorCode::Budget,
                            "source entry budget exceeded",
                        ));
                    }
                }
            }
            pending = next;
        }
        let requests = blobs
            .iter()
            .map(|(_, id, _)| (id.clone(), "blob"))
            .collect::<Vec<_>>();
        let payloads = self.objects(&requests, MAX_TREE_BYTES as usize + MAX_FILES * 128)?;
        let mut files = blobs
            .into_iter()
            .zip(payloads)
            .map(|((path, _, executable), bytes)| SourceFile {
                path,
                bytes,
                executable,
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let version = self.command(&["--version"], vec![], 256)?;
        Ok(SourceTree {
            source_revision: source.clone(),
            git_tree,
            files,
            environment: Environment {
                os: std::env::consts::OS.into(),
                architecture: std::env::consts::ARCH.into(),
                git_version: String::from_utf8(version)
                    .map_err(|_| corrupt("Git version encoding"))?
                    .trim()
                    .into(),
                tools: Default::default(),
            },
        })
    }
}
