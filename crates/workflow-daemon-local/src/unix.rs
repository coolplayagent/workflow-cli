use crate::*;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread::JoinHandle,
};
const SOCKET: &str = "control.sock";
const LIMIT: u64 = 65536;
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Status,
    Stop { instance: String },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Status { status: Box<Status> },
    Rejected,
}

fn directory(path: &Path, create: bool) -> io::Result<PathBuf> {
    if create && !path.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let absolute = std::path::absolute(path)?;
    let mut part = PathBuf::new();
    for c in absolute.components() {
        part.push(c.as_os_str());
        let m = std::fs::symlink_metadata(&part)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(invalid("real control directory components required"));
        }
    }
    let m = std::fs::metadata(&absolute)?;
    // geteuid has no pointer arguments and cannot fail.
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(invalid(
            "control directory must be owned by this user with mode 0700",
        ));
    }
    // Linux sockaddr_un has 108 bytes, including the terminating NUL.
    if absolute.join(SOCKET).as_os_str().len() >= 108 {
        return Err(invalid("control socket path is too long"));
    }
    Ok(absolute)
}
fn lock_file(dir: &Path, create: bool) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("owner.lock"))?;
    let m = f.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.nlink() != 1
    {
        return Err(invalid("invalid private daemon lock file"));
    }
    Ok(f)
}
fn lock(f: &File) -> io::Result<()> {
    // The live file descriptor is retained by Server throughout the listener lifetime.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn send(dir: &Path, request: &Request) -> io::Result<Status> {
    let path = dir.join(SOCKET);
    if !std::fs::symlink_metadata(&path)?.file_type().is_socket() {
        return Err(invalid("control endpoint must be a Unix socket"));
    }
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(&serde_json::to_vec(request)?)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut bytes = vec![];
    stream.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("oversized daemon response"));
    }
    match serde_json::from_slice(&bytes)? {
        Reply::Status { status } => Ok(*status),
        Reply::Rejected => Err(invalid("daemon generation changed or request rejected")),
    }
}
pub fn inspect(path: impl AsRef<Path>) -> io::Result<Observation> {
    let stopped = || Observation {
        availability: Availability::Stopped,
        status: None,
    };
    let dir = match directory(path.as_ref(), false) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(stopped()),
        Err(e) => return Err(e),
    };
    match send(&dir, &Request::Status) {
        Ok(status) => Ok(Observation {
            availability: Availability::Responsive,
            status: Some(status),
        }),
        Err(_) => match lock_file(&dir, false) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(stopped()),
            Err(e) => Err(e),
            Ok(f) => match lock(&f) {
                Ok(()) => Ok(stopped()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Observation {
                    availability: Availability::Unreachable,
                    status: None,
                }),
                Err(e) => Err(e),
            },
        },
    }
}
pub fn request_stop(path: impl AsRef<Path>, instance: &str) -> io::Result<Status> {
    send(
        &directory(path.as_ref(), false)?,
        &Request::Stop {
            instance: instance.into(),
        },
    )
}
pub struct Server {
    control: Control,
    quit: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    path: PathBuf,
    _lock: File,
}
impl Server {
    pub fn start(path: impl AsRef<Path>, info: Info) -> io::Result<Self> {
        if !(20..=60000).contains(&info.poll_interval_ms)
            || info.database.len() > 4096
            || info.configuration_digest.len() > 128
        {
            return Err(invalid("invalid daemon information"));
        }
        let dir = directory(path.as_ref(), true)?;
        let lockfile = lock_file(&dir, true)?;
        lock(&lockfile)?;
        let path = dir.join(SOCKET);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_socket() => std::fs::remove_file(&path)?,
            Ok(_) => return Err(invalid("refusing to replace non-socket control endpoint")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let mut random = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let instance = random.iter().map(|b| format!("{b:02x}")).collect();
        let control = Control(Arc::new(Mutex::new(Status {
            instance,
            pid: std::process::id(),
            started_at_unix_ms: now(),
            observed_at_unix_ms: now(),
            info,
            phase: Phase::Polling,
            active_run: None,
            last_scan_unix_ms: None,
            last_completed_run: None,
            completed_drives: 0,
            diagnostics: vec![],
        })));
        let quit = Arc::new(AtomicBool::new(false));
        let q = quit.clone();
        let c = control.clone();
        let thread = std::thread::spawn(move || {
            while !q.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                        let mut bytes = vec![];
                        if (&mut stream).take(4097).read_to_end(&mut bytes).is_err()
                            || bytes.len() > 4096
                        {
                            continue;
                        }
                        let reply = match serde_json::from_slice::<Request>(&bytes) {
                            Ok(Request::Status) => Reply::Status {
                                status: Box::new(c.snapshot()),
                            },
                            Ok(Request::Stop { instance }) => {
                                let mut s = c.0.lock().expect("daemon state");
                                if instance != s.instance {
                                    Reply::Rejected
                                } else {
                                    s.phase = Phase::Draining;
                                    s.observed_at_unix_ms = now();
                                    Reply::Status {
                                        status: Box::new(s.clone()),
                                    }
                                }
                            }
                            Err(_) => Reply::Rejected,
                        };
                        if let Ok(bytes) = serde_json::to_vec(&reply) {
                            let _ = stream.write_all(&bytes);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => {
                        c.diagnose(None, "control_listener_failed");
                        c.0.lock().expect("daemon state").phase = Phase::Draining;
                        break;
                    }
                }
            }
        });
        Ok(Self {
            control,
            quit,
            thread: Some(thread),
            path,
            _lock: lockfile,
        })
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
