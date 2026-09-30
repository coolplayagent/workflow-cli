use crate::*;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
use workflow_runstore_postgres::access::WorkerStatus;

#[derive(Clone)]
pub struct DrainHandle(Arc<AtomicBool>);
impl DrainHandle {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
/// Registered worker liveness, independent from synchronous task execution.
/// A failed heartbeat stops admission locally; frozen deadlines still fence
/// in-flight tasks, and server-side drain survives process exit or restart.
pub struct WorkerSession {
    client: Arc<RemoteClient>,
    version: String,
    drain: DrainHandle,
    healthy: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}
fn heartbeat(client: &RemoteClient, version: &str, drain: bool) -> Result<WorkerStatus> {
    match client.call(&Request {
        protocol_version: PROTOCOL_VERSION,
        request_id: "worker-heartbeat".into(),
        operation: Operation::WorkerHeartbeat {
            runtime_version: version.into(),
            drain,
        },
    })? {
        Response::WorkerStatus(status) => Ok(status),
        _ => Err(invalid()),
    }
}
impl WorkerSession {
    pub fn start(binding: ClientBinding, version: String, interval_ms: u64) -> Result<Self> {
        if !(100..=60000).contains(&interval_ms) {
            return Err(invalid());
        }
        let request_timeout = binding.timeout_ms;
        let client = Arc::new(RemoteClient::new(binding)?);
        let first = heartbeat(&client, &version, false)?;
        if interval_ms * 2 + request_timeout >= first.heartbeat_timeout_ms {
            return Err(invalid());
        }
        let drain = DrainHandle(Arc::new(AtomicBool::new(first.draining)));
        let healthy = Arc::new(AtomicBool::new(true));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let thread = {
            let (client, version, drain, healthy, wake) = (
                client.clone(),
                version.clone(),
                drain.clone(),
                healthy.clone(),
                wake.clone(),
            );
            std::thread::spawn(move || {
                loop {
                    let Ok(stopped) = wake.0.lock() else {
                        healthy.store(false, Ordering::SeqCst);
                        break;
                    };
                    if *stopped {
                        break;
                    }
                    let Ok((stopped, _)) = wake
                        .1
                        .wait_timeout(stopped, Duration::from_millis(interval_ms))
                    else {
                        healthy.store(false, Ordering::SeqCst);
                        break;
                    };
                    if *stopped {
                        break;
                    }
                    drop(stopped);
                    match heartbeat(&client, &version, drain.requested()) {
                        Ok(status) => {
                            if status.draining {
                                drain.request();
                            }
                        }
                        Err(_) => {
                            healthy.store(false, Ordering::SeqCst);
                            drain.request();
                            break;
                        }
                    }
                }
            })
        };
        Ok(Self {
            client,
            version,
            drain,
            healthy,
            wake,
            thread: Some(thread),
        })
    }
    pub fn drain_handle(&self) -> DrainHandle {
        self.drain.clone()
    }
    pub fn check(&self) -> Result<()> {
        if self.healthy.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
    pub fn draining(&self) -> bool {
        self.drain.requested()
    }
    /// Returns only after the database acknowledges sticky drain. A lost reply
    /// is an error, never an assertion that assignments have been drained.
    pub fn drain(&self) -> Result<WorkerStatus> {
        self.drain.request();
        self.check()?;
        heartbeat(&self.client, &self.version, true)
    }
}
impl Drop for WorkerSession {
    fn drop(&mut self) {
        if let Ok(mut stopped) = self.wake.0.lock() {
            *stopped = true;
            self.wake.1.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
