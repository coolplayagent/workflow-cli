use crate::*;
use std::io::Read;
pub(super) fn directory(path: &Path) -> Result<()> {
    // The store is owned by the host. Reject symlink components; hostile replacement
    // of a host-owned root by another OS principal is outside this adapter boundary.
    let absolute = std::path::absolute(path).map_err(io)?;
    let mut prefix = PathBuf::new();
    for part in absolute.components() {
        prefix.push(part.as_os_str());
        let m = std::fs::symlink_metadata(&prefix).map_err(io)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(Error::new(
                ErrorCode::UnsupportedStorage,
                "artifact directory must have real directory components",
            ));
        }
    }
    Ok(())
}
pub(super) fn regular(path: &Path) -> Result<()> {
    let m = std::fs::symlink_metadata(path).map_err(io)?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(corrupt("artifact entry must be a regular file"));
    }
    Ok(())
}
pub(super) fn sync_dir(path: &Path) -> Result<()> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    std::fs::File::open(path)
        .map_err(io)?
        .sync_all()
        .map_err(io)
}
pub(super) fn object(root: &Path, r: &ArtifactRef) -> PathBuf {
    root.join("objects").join(&r.manifest.content_digest[7..])
}
pub(super) fn content(root: &Path, r: &ArtifactRef) -> Result<Vec<u8>> {
    validate_ref(r)?;
    directory(&root.join("objects"))?;
    let path = object(root, r);
    regular(&path)?;
    let mut bytes = vec![];
    std::fs::File::open(path)
        .map_err(io)?
        .take(MAX_CONTENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 != r.manifest.bytes || content_digest(&bytes) != r.manifest.content_digest
    {
        return Err(corrupt("artifact bytes or digest mismatch"));
    }
    validate_content(&r.manifest.spec.artifact_type, &bytes).map_err(|e| corrupt(e.message))?;
    Ok(bytes)
}

pub(super) fn create_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io)
}
pub(super) fn create_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(io)
}
