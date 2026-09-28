use crate::*;
use std::io::{Read, Write};
pub(crate) fn io(e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Storage, e.to_string())
}
pub(crate) fn corrupt(e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::CorruptArchive, e.to_string())
}
pub(crate) fn directory(path: &Path) -> Result<()> {
    let absolute = std::path::absolute(path).map_err(io)?;
    let mut p = PathBuf::new();
    for part in absolute.components() {
        p.push(part.as_os_str());
        let m = std::fs::symlink_metadata(&p).map_err(io)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(corrupt("backup requires real directory components"));
        }
    }
    Ok(())
}
pub(crate) fn regular(path: &Path) -> Result<std::fs::Metadata> {
    directory(path.parent().unwrap_or(Path::new(".")))?;
    let m = std::fs::symlink_metadata(path).map_err(io)?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(corrupt("backup requires regular files, without symlinks"));
    }
    Ok(m)
}
pub(crate) fn create_dir(path: &Path) -> Result<()> {
    directory(path.parent().unwrap_or(Path::new(".")))?;
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path).map_err(io)?;
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}
pub(crate) fn create_file(path: &Path) -> Result<std::fs::File> {
    let mut b = std::fs::OpenOptions::new();
    b.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        b.mode(0o600);
    }
    b.open(path).map_err(io)
}
pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = create_file(path)?;
    f.write_all(bytes).map_err(io)?;
    f.sync_all().map_err(io)
}
pub(crate) fn read(path: &Path, max: u64) -> Result<Vec<u8>> {
    if regular(path)?.len() > max {
        return Err(Error::new(
            ErrorCode::Budget,
            "backup file exceeds its size budget",
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(io)?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > max {
        return Err(Error::new(
            ErrorCode::Budget,
            "backup file grew beyond its size budget",
        ));
    }
    Ok(bytes)
}
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    std::fs::File::open(if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    })
    .map_err(io)?
    .sync_all()
    .map_err(io)
}
pub(crate) fn inventory(root: &Path) -> Result<BTreeMap<String, FileImage>> {
    directory(root)?;
    let mut dirs = vec![String::new()];
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(root.join(&dir)).map_err(io)? {
            let e = e.map_err(io)?;
            let name = e
                .file_name()
                .into_string()
                .map_err(|_| corrupt("non UTF-8 backup path"))?;
            let path = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            let m = std::fs::symlink_metadata(e.path()).map_err(io)?;
            if m.file_type().is_symlink() {
                return Err(corrupt("symlink in backup"));
            }
            if m.is_dir() {
                if !matches!(
                    path.as_str(),
                    "artifacts" | "artifacts/objects" | "artifacts/uploads"
                ) {
                    return Err(corrupt("unexpected backup directory"));
                }
                dirs.push(path);
                continue;
            }
            if path == "backup.json" {
                regular(&e.path())?;
                continue;
            }
            if !allowed_file(&path) || !m.is_file() {
                return Err(corrupt("unlisted backup file or unsupported file type"));
            }
            total = total
                .checked_add(m.len())
                .ok_or_else(|| corrupt("backup length overflow"))?;
            if total > MAX_ARCHIVE_BYTES || files.len() >= MAX_FILES {
                return Err(Error::new(ErrorCode::Budget, "backup size/count exceeded"));
            }
            let limit = if path.ends_with(".sqlite") {
                MAX_DATABASE_BYTES
            } else {
                workflow_artifacts::MAX_CONTENT_BYTES
            };
            let bytes = read(&e.path(), limit)?;
            files.insert(
                path,
                FileImage {
                    bytes: bytes.len() as u64,
                    digest: bytes_digest(&bytes),
                },
            );
        }
    }
    Ok(files)
}
pub(crate) fn publish_json<T: Serialize>(root: &Path, name: &str, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Error::new(ErrorCode::Budget, "backup index exceeds 8 MiB"));
    }
    let temporary = root.join(format!(".{name}.pending"));
    write(&temporary, &bytes)?;
    std::fs::rename(temporary, root.join(name)).map_err(io)?;
    sync_dir(root)
}

pub(crate) fn generation() -> Result<String> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")
        .map_err(io)?
        .read_exact(&mut bytes)
        .map_err(io)?;
    Ok(bytes_digest(&bytes))
}
pub(crate) fn staging(destination: &Path) -> Result<PathBuf> {
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(Error::new(
            ErrorCode::InvalidArchive,
            "destination already exists; backups and restores never overwrite",
        ));
    }
    let destination = std::path::absolute(destination).map_err(io)?;
    let parent = destination
        .parent()
        .ok_or_else(|| corrupt("destination requires a parent"))?;
    directory(parent)?;
    let staging = parent.join(format!(".workflow-backup-{}", &generation()?[7..]));
    create_dir(&staging)?;
    Ok(staging)
}
pub(crate) fn publish_directory(staging: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(staging.as_os_str().as_bytes()).map_err(io)?;
        let to = std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(io)?;
        // Both NUL-terminated paths remain alive across the call. NOREPLACE
        // prevents overwriting even an empty destination created concurrently.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        sync_dir(destination.parent().unwrap_or(Path::new(".")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (staging, destination);
        Err(Error::new(
            ErrorCode::UnsupportedArchive,
            "atomic no-overwrite directory publication currently requires Linux",
        ))
    }
}
