use super::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
static TERMINATING: AtomicBool = AtomicBool::new(false);
extern "C" fn terminate(_: libc::c_int) {
    TERMINATING.store(true, Ordering::SeqCst);
}
struct Signals {
    old: Vec<(libc::c_int, libc::sigaction)>,
}
impl Signals {
    fn install() -> Result<Self> {
        TERMINATING.store(false, Ordering::SeqCst);
        let mut signals = Self { old: vec![] };
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: sigaction is initialized, the handler only stores an atomic
            // flag, and the previous action is restored by the owning CLI guard.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = terminate as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                let mut old = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut old) != 0 {
                    return Err(Error::new(ErrorCode::Storage, "signal setup failed"));
                }
                signals.old.push((signal, old));
            }
        }
        Ok(signals)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, action) in &self.old {
            // SAFETY: restore the previously captured action for this signal.
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
    }
}
struct Watch {
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watch {
    fn new(drain: DrainHandle) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let stopped = done.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                if TERMINATING.load(Ordering::SeqCst) {
                    drain.request();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            done,
            thread: Some(thread),
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Models {
    bundle: String,
    bindings: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Managed {
    runtime_version: String,
    heartbeat_interval_ms: u64,
    drain_timeout_ms: u64,
    #[serde(default)]
    models: Option<Models>,
    #[serde(default)]
    effects: Option<String>,
}
fn pause(poll: Duration, session: &WorkerSession) {
    let until = Instant::now() + poll;
    while Instant::now() < until && !session.draining() {
        std::thread::sleep(
            Duration::from_millis(20).min(until.saturating_duration_since(Instant::now())),
        );
    }
}
pub(super) fn run(
    binding: &str,
    config: &str,
    iterations: &str,
    poll: &str,
) -> Result<serde_json::Value> {
    let (iterations, poll) = bounds(iterations, poll)?;
    let config: Managed = read(config)?;
    if !(1000..=3600000).contains(&config.drain_timeout_ms) {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            "invalid worker drain timeout",
        ));
    }
    let (worker, mut principal) = if let Some(models) = config.models {
        let bundle: workflow_kernel::BundleSpec = read(&models.bundle)?;
        workflow_kernel::CompiledBundle::compile(bundle.clone())?;
        crate::models::shared_worker(&bundle, &models.bindings)?
    } else {
        (workflow_builtin_capabilities::worker()?, None)
    };
    let effects = if let Some(path) = config.effects {
        let bindings: Vec<workflow_effect_http::HttpEffectBinding> = read(&path)?;
        for binding in &bindings {
            if let Some(p) = binding.shared_principal()? {
                if principal.as_ref().is_some_and(|old| old != p) {
                    return Err(Error::new(
                        ErrorCode::InvalidRequest,
                        "shared execution principals differ",
                    ));
                }
                principal = Some(p.clone());
            }
        }
        Some(workflow_effect_http::HttpEffects::new(bindings)?)
    } else {
        None
    };
    let binding: ClientBinding = read(binding)?;
    let client = RemoteClient::new(binding.clone())?;
    let _signals = Signals::install()?;
    let session = WorkerSession::start(
        binding,
        config.runtime_version,
        config.heartbeat_interval_ms,
    )?;
    let _watch = Watch::new(session.drain_handle());
    let mut completed = 0;
    let mut failed = 0;
    let mut fenced = 0;
    let mut once = || -> Result<()> {
        session.check()?;
        if let Some(effects) = &effects {
            let report = work_effects_once_bound(&client, effects, 1, principal.as_ref())?;
            completed += report.completed;
            fenced += report.fenced;
            failed += report.failed;
        }
        session.check()?;
        let report = work_once_bound(&client, &worker, 1, principal.as_ref())?;
        completed += report.completed;
        fenced += report.fenced;
        failed += report.failed;
        Ok(())
    };
    for i in 0..iterations {
        if session.draining() {
            break;
        }
        once()?;
        if i + 1 < iterations {
            pause(poll, &session);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(config.drain_timeout_ms);
    loop {
        let status = session.drain()?;
        if status.active_assignments == 0 {
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::new(
                ErrorCode::Busy,
                "worker drain deadline exceeded; remaining assignments require reconciliation",
            ));
        }
        once()?;
        std::thread::sleep(poll.min(deadline.saturating_duration_since(Instant::now())));
    }
    Ok(json!({"completed":completed,"failed":failed,"fenced":fenced,"drained":true}))
}
