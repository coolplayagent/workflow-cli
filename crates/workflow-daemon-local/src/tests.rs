use super::*;
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Dir(std::path::PathBuf);
impl Dir {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "wfd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn info() -> Info {
    Info {
        database: "test-database".into(),
        configuration_digest: "test-digest".into(),
        poll_interval_ms: 20,
    }
}
#[test]
fn live_control_fences_stop_generation_and_drains_prior_admission() {
    let d = Dir::new();
    assert_eq!(inspect(&d.0).unwrap().availability, Availability::Stopped);
    let server = Server::start(&d.0, info()).unwrap();
    assert!(Server::start(&d.0, info()).is_err());
    let control = server.control();
    assert!(control.admit("active"));
    let s = inspect(&d.0).unwrap().status.unwrap();
    assert_eq!(s.active_run.as_deref(), Some("active"));
    assert!(request_stop(&d.0, "old-instance").is_err());
    assert!(!control.stopping());
    let stopped = request_stop(&d.0, &s.instance).unwrap();
    assert_eq!(stopped.phase, Phase::Draining);
    assert!(!control.admit("new-run"));
    assert_eq!(stopped.active_run.as_deref(), Some("active"));
    control.complete("active");
    assert!(control.stopping());
    assert!(control.snapshot().active_run.is_none());
    drop(server);
    assert_eq!(inspect(&d.0).unwrap().availability, Availability::Stopped);
    let new = Server::start(&d.0, info()).unwrap();
    assert!(request_stop(&d.0, &s.instance).is_err());
    assert!(!new.control().stopping());
}
#[test]
fn refuses_public_directories_symlinks_and_non_socket_replacement() {
    let d = Dir::new();
    std::fs::create_dir(&d.0).unwrap();
    std::fs::set_permissions(&d.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Server::start(&d.0, info()).is_err());
    std::fs::set_permissions(&d.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let marker = d.0.join("control.sock");
    std::fs::write(&marker, b"keep").unwrap();
    assert!(Server::start(&d.0, info()).is_err());
    assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    std::fs::remove_file(&marker).unwrap();
    std::os::unix::fs::symlink("missing", &marker).unwrap();
    assert!(Server::start(&d.0, info()).is_err());
}
#[test]
fn child_process() {
    let Ok(p) = std::env::var("WORKFLOW_DAEMON_TEST_DIR") else {
        return;
    };
    let _server = Server::start(p, info()).unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[test]
fn killed_process_is_not_a_running_timer_service_and_stale_socket_is_recoverable() {
    let d = Dir::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::child_process", "--nocapture"])
        .env("WORKFLOW_DAEMON_TEST_DIR", &d.0)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while inspect(&d.0).unwrap().availability != Availability::Responsive {
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    let old = inspect(&d.0).unwrap().status.unwrap().instance;
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(d.0.join("control.sock").exists());
    assert_eq!(inspect(&d.0).unwrap().availability, Availability::Stopped);
    let server = Server::start(&d.0, info()).unwrap();
    assert_ne!(server.control().snapshot().instance, old);
    assert!(request_stop(&d.0, &old).is_err());
}
