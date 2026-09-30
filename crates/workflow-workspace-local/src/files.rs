//! Descriptor-relative Linux file operations. No workspace symlink is followed.
use crate::*;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
pub(crate) struct Dir {
    file: File,
}
fn name(n: &str) -> Result<CString> {
    if n.is_empty() || n == "." || n == ".." || n.contains('/') {
        return Err(corrupt("invalid directory entry"));
    }
    CString::new(n).map_err(|_| corrupt("NUL directory entry"))
}
fn syscall(n: i32) -> Result<i32> {
    if n < 0 {
        Err(io(std::io::Error::last_os_error()))
    } else {
        Ok(n)
    }
}
impl Dir {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .map_err(io)?,
        })
    }
    pub fn proc_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }
    pub fn child(&self, n: &str) -> Result<Self> {
        let n = name(n)?;
        // SAFETY: live directory descriptor and NUL-terminated relative component.
        let fd = syscall(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                n.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        // SAFETY: successful openat returns a new owned descriptor.
        Ok(Self {
            file: unsafe { File::from_raw_fd(fd) },
        })
    }
    pub fn create(&self, n: &str) -> Result<Self> {
        let c = name(n)?;
        // SAFETY: live directory descriptor and validated component.
        syscall(unsafe { libc::mkdirat(self.file.as_raw_fd(), c.as_ptr(), 0o700) })?;
        self.sync()?;
        self.child(n)
    }
    pub fn sync(&self) -> Result<()> {
        self.file.sync_all().map_err(io)
    }
    pub fn entries(&self) -> Result<Vec<String>> {
        let mut names = vec![];
        for e in std::fs::read_dir(self.proc_path()).map_err(io)? {
            let n = e
                .map_err(io)?
                .file_name()
                .into_string()
                .map_err(|_| corrupt("non-UTF8 workspace entry"))?;
            name(&n)?;
            names.push(n);
            if names.len() > MAX_FILES {
                return Err(Error::new(
                    ErrorCode::Budget,
                    "directory entry budget exceeded",
                ));
            }
        }
        names.sort();
        Ok(names)
    }
    pub fn is_dir(&self, n: &str) -> Result<bool> {
        name(n)?;
        let m = std::fs::symlink_metadata(self.proc_path().join(n)).map_err(io)?;
        if m.is_symlink() || (!m.is_file() && !m.is_dir()) {
            return Err(corrupt("symlinks and special files are unsupported"));
        }
        Ok(m.is_dir())
    }
    fn parent(&self, p: &str, create: bool) -> Result<(Self, String)> {
        validate_path(p)?;
        let mut d = Self {
            file: self.file.try_clone().map_err(io)?,
        };
        let mut parts = p.split('/').peekable();
        while let Some(n) = parts.next() {
            if parts.peek().is_none() {
                return Ok((d, n.into()));
            }
            if create && !d.entries()?.iter().any(|s| s == n) {
                d.create(n)?;
            }
            d = d.child(n)?;
        }
        Err(corrupt("empty file path"))
    }
    pub fn write(&self, p: &str, bytes: &[u8], executable: bool) -> Result<()> {
        let (d, n) = self.parent(p, true)?;
        let c = name(&n)?;
        // SAFETY: openat is relative to an owned parent; exclusive creation cannot replace a file.
        let fd = syscall(unsafe {
            libc::openat(
                d.file.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                if executable { 0o700 } else { 0o600 },
            )
        })?;
        // SAFETY: this successful descriptor has no other owner.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        d.sync()
    }
    pub fn regular(&self, p: &str) -> Result<File> {
        let (d, n) = self.parent(p, false)?;
        let c = name(&n)?;
        // SAFETY: descriptor-relative no-follow open; nonblocking avoids waiting on a substituted FIFO.
        let fd = syscall(unsafe {
            libc::openat(
                d.file.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        })?;
        // SAFETY: transfer unique ownership of the newly opened descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let m = file.metadata().map_err(io)?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(corrupt("regular unshared workspace file required"));
        }
        Ok(file)
    }
    pub fn read(&self, p: &str) -> Result<(Vec<u8>, bool)> {
        let mut file = self.regular(p)?;
        let before = file.metadata().map_err(io)?;
        if !before.is_file() || before.nlink() != 1 {
            return Err(corrupt("regular unshared workspace file required"));
        }
        if before.len() > MAX_FILE_BYTES {
            return Err(Error::new(
                ErrorCode::Budget,
                "workspace file exceeds 64 MiB",
            ));
        }
        let mut bytes = vec![];
        (&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        let after = file.metadata().map_err(io)?;
        let key = |m: &std::fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mode(),
                m.nlink(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        if key(&before) != key(&after) || after.len() != bytes.len() as u64 {
            return Err(Error::new(
                ErrorCode::Changed,
                "file changed during observation",
            ));
        }
        Ok((bytes, after.mode() & 0o111 != 0))
    }
    pub fn scan(&self) -> Result<Vec<FileEntry>> {
        self.scan_inner(false)
    }
    pub(crate) fn scan_git_worktree(&self) -> Result<Vec<FileEntry>> {
        self.scan_inner(true)
    }
    fn scan_inner(&self, ignore_git: bool) -> Result<Vec<FileEntry>> {
        fn visit(
            d: &Dir,
            prefix: &str,
            files: &mut Vec<FileEntry>,
            dirs: &mut usize,
            total: &mut u64,
            ignore_git: bool,
        ) -> Result<()> {
            *dirs += 1;
            if *dirs > MAX_FILES {
                return Err(Error::new(
                    ErrorCode::Budget,
                    "workspace directory budget exceeded",
                ));
            }
            for n in d.entries()? {
                if ignore_git && prefix.is_empty() && n == ".git" {
                    continue;
                }
                let p = if prefix.is_empty() {
                    n.clone()
                } else {
                    format!("{prefix}/{n}")
                };
                validate_path(&p)?;
                if d.is_dir(&n)? {
                    visit(&d.child(&n)?, &p, files, dirs, total, ignore_git)?;
                } else {
                    let (bytes, executable) = d.read(&n)?;
                    *total += bytes.len() as u64;
                    if *total > MAX_TREE_BYTES || files.len() >= MAX_FILES {
                        return Err(Error::new(
                            ErrorCode::Budget,
                            "workspace tree budget exceeded",
                        ));
                    }
                    files.push(FileEntry {
                        path: p,
                        digest: content_digest(&bytes),
                        bytes: bytes.len() as u64,
                        executable,
                    });
                }
            }
            Ok(())
        }
        let mut files = vec![];
        visit(self, "", &mut files, &mut 0, &mut 0, ignore_git)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        validate_files(&files)?;
        Ok(files)
    }
    pub fn rename(&self, n: &str, target: &Dir, new: &str) -> Result<()> {
        let n = name(n)?;
        let new = name(new)?;
        // SAFETY: both owned parent descriptors and names remain live; never replace an existing destination.
        syscall(unsafe {
            libc::renameat2(
                self.file.as_raw_fd(),
                n.as_ptr(),
                target.file.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        })?;
        self.sync()?;
        target.sync()
    }
    pub fn remove_tree(&self, n: &str) -> Result<()> {
        let d = self.child(n)?;
        for entry in d.entries()? {
            if d.is_dir(&entry)? {
                d.remove_tree(&entry)?;
            } else {
                d.unlink(&entry, false)?;
            }
        }
        d.sync()?;
        self.unlink(n, true)
    }
    fn unlink(&self, n: &str, dir: bool) -> Result<()> {
        let n = name(n)?;
        // SAFETY: unlink one validated entry relative to the owned directory; symlinks are never traversed.
        syscall(unsafe {
            libc::unlinkat(
                self.file.as_raw_fd(),
                n.as_ptr(),
                if dir { libc::AT_REMOVEDIR } else { 0 },
            )
        })?;
        self.sync()
    }
}
