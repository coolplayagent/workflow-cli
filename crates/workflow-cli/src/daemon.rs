use crate::write;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};
use workflow_daemon_local::{Availability, Control, Info, Server};
use workflow_runstore::{RunStatus, RunStore};
use workflow_worker::Clock;
pub const HELP: &str = "LOCAL DAEMON (Linux)\n  workflow daemon serve <config.json>\n  workflow daemon status <private-control-directory>\n  workflow daemon stop <private-control-directory>\n  workflow schema daemon-config\n\nserve stays in the foreground; an OS service manager may supervise it.\nstatus queries live IPC; stopped/unreachable does not imply timers are being scheduled.\nstop acknowledges draining all admitted drives; query status until stopped.\nDaemon control is trusted local user access, not remote authentication.\n";
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Config {
    schema_version: u32,
    database: String,
    control_directory: String,
    artifacts: Option<String>,
    model_bindings: Option<String>,
    effect_bindings: Option<String>,
    #[serde(default = "default_lease_ms")]
    lease_ms: u64,
    poll_interval_ms: u64,
    error_backoff_ms: u64,
}
fn default_lease_ms() -> u64 {
    120000
}
fn read_config(path: &str) -> Result<Config, String> {
    let mut bytes = vec![];
    std::fs::File::open(path)
        .and_then(|f| f.take(65537).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("daemon configuration exceeds 64 KiB".into());
    }
    let mut c: Config = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if c.schema_version != 1
        || !(100..=300000).contains(&c.lease_ms)
        || !(20..=60000).contains(&c.poll_interval_ms)
        || !(100..=60000).contains(&c.error_backoff_ms)
    {
        return Err(
            "daemon requires schema 1, lease 100..300000 ms, poll 20..60000 ms and error backoff 100..60000 ms".into(),
        );
    }
    for path in [&mut c.database]
        .into_iter()
        .chain(c.artifacts.iter_mut())
        .chain(c.model_bindings.iter_mut())
        .chain(c.effect_bindings.iter_mut())
    {
        *path = std::fs::canonicalize(&*path)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("daemon paths must be UTF-8")?
            .into();
    }
    c.control_directory = std::path::absolute(&c.control_directory)
        .map_err(|e| e.to_string())?
        .to_str()
        .ok_or("control path must be UTF-8")?
        .into();
    // Reject configuration errors before reporting a live scheduler.
    crate::runs::open(&c.database, c.artifacts.as_deref()).map_err(|e| e.to_string())?;
    Ok(c)
}
fn due(snapshot: &workflow_runstore::Snapshot, now: u64) -> bool {
    snapshot
        .frames
        .values()
        .flat_map(|f| f.nodes.values())
        .any(|n| match n.state {
            workflow_kernel::NodeState::Waiting { deadline_unix_ms } => deadline_unix_ms <= now,
            workflow_kernel::NodeState::Child {
                deadline_unix_ms: Some(d),
                exhausting: false,
                ..
            } => d <= now,
            _ => false,
        })
}
struct Poller {
    cursor: Option<String>,
    delayed: BTreeMap<String, (u64, Instant)>,
    models: Vec<crate::models::Binding>,
    effects: Option<std::sync::Arc<workflow_effect_http::HttpEffects>>,
    active: BTreeMap<
        String,
        (
            u64,
            std::thread::JoinHandle<Result<Value, workflow_runtime::Error>>,
        ),
    >,
    sequence: u64,
}
impl Poller {
    fn drive(
        c: &Config,
        id: &str,
        models: &[crate::models::Binding],
        effects: Option<&workflow_effect_http::HttpEffects>,
        instance: &str,
        sequence: u64,
    ) -> Result<Value, workflow_runtime::Error> {
        let mut store = crate::runs::open(&c.database, c.artifacts.as_deref())?;
        let bundle = store.bundle(id)?;
        crate::models::bound_worker(&bundle, models, true)?;
        let worker = crate::activity::Config {
            bundle: Some(bundle),
            models: models.to_vec(),
            allow_unused: true,
            remote: None,
            workspace: None,
            journal: Some(crate::activity::JournalBinding::Local {
                journal: crate::activity::LocalJournal::new(
                    &c.database,
                    c.artifacts
                        .as_deref()
                        .map(crate::runs::ArtifactLocation::Local),
                ),
            }),
        };
        let options = workflow_runtime::DriveOptions {
            owner: format!("daemon-{}", &instance[..16]),
            acquisition_id: format!("daemon-{instance}-{sequence}"),
            lease_ms: c.lease_ms,
            max_commands: 1,
        };
        let report = if let Some(effects) = effects {
            workflow_runtime::drive_with_effects(
                &mut store,
                &worker,
                effects,
                id,
                &options,
                &workflow_worker::SystemClock,
            )?
        } else {
            workflow_runtime::drive(
                &mut store,
                &worker,
                id,
                &options,
                &workflow_worker::SystemClock,
            )?
        };
        Ok(json!(report))
    }
    fn reap(&mut self, c: &Config, control: &Control) {
        let done: Vec<_> = self
            .active
            .iter()
            .filter(|(_, (_, h))| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            let (revision, handle) = self.active.remove(&id).unwrap();
            let code = match handle.join() {
                Ok(Ok(report)) => match report["stop_reason"].as_str() {
                    Some(reason @ ("effect_backoff" | "effect_uncertain")) => {
                        Some(reason.to_string())
                    }
                    _ => None,
                },
                Ok(Err(e)) => Some(
                    e.storage
                        .map(|e| format!("storage_{:?}", e.code))
                        .or_else(|| e.worker.map(|e| format!("worker_{:?}", e.code)))
                        .unwrap_or("drive_failed".into()),
                ),
                Err(_) => Some("drive_panicked".into()),
            };
            control.complete(&id);
            if let Some(code) = code {
                control.diagnose(Some(&id), &code);
                if self.delayed.len() >= 10000 {
                    self.delayed.clear();
                }
                self.delayed.insert(
                    id,
                    (
                        revision,
                        Instant::now() + Duration::from_millis(c.error_backoff_ms),
                    ),
                );
            } else {
                self.delayed.remove(&id);
            }
        }
    }
    fn poll(&mut self, c: &Config, control: &Control) -> Result<(), workflow_runstore::Error> {
        self.reap(c, control);
        let mut store = crate::runs::open(&c.database, c.artifacts.as_deref())?;
        let page = store.list(self.cursor.as_deref(), 100)?;
        self.cursor = page.next_cursor;
        control.scanned();
        // A bounded page gives stop and other runs a scheduling opportunity.
        for run in page.items {
            if control.stopping() || self.active.len() >= 8 {
                break;
            }
            if self.active.contains_key(&run.run_id) {
                continue;
            }
            if !matches!(run.status, RunStatus::Running | RunStatus::Cancelling)
                || run.pause.is_some()
            {
                self.delayed.remove(&run.run_id);
                continue;
            }
            if self
                .delayed
                .get(&run.run_id)
                .is_some_and(|(revision, until)| {
                    *revision == run.revision && Instant::now() < *until
                })
            {
                continue;
            }
            let outcome: workflow_runstore::Result<()> = (|| {
                let snapshot = store.get(&run.run_id)?;
                let now = workflow_worker::SystemClock.now_unix_ms()?;
                if snapshot.pause.is_some()
                    || (!due(&snapshot, now)
                        && store.outbox(&run.run_id, 0, 1, true)?.items.is_empty())
                {
                    return Ok(());
                }
                self.sequence = self.sequence.checked_add(1).ok_or_else(|| {
                    workflow_runstore::Error::new(
                        workflow_runstore::ErrorCode::InvalidRequest,
                        "daemon sequence exhausted",
                    )
                })?;
                if !control.admit(&run.run_id) {
                    return Ok(());
                }
                let (config, id, models, effects, instance, sequence) = (
                    c.clone(),
                    run.run_id.clone(),
                    self.models.clone(),
                    self.effects.clone(),
                    control.snapshot().instance,
                    self.sequence,
                );
                let handle = std::thread::spawn(move || {
                    Self::drive(
                        &config,
                        &id,
                        &models,
                        effects.as_deref(),
                        &instance,
                        sequence,
                    )
                });
                self.active
                    .insert(run.run_id.clone(), (run.revision, handle));
                Ok(())
            })();
            if let Err(e) = outcome {
                control.diagnose(Some(&run.run_id), &format!("storage_{:?}", e.code));
                self.delayed.insert(
                    run.run_id,
                    (
                        run.revision,
                        Instant::now() + Duration::from_millis(c.error_backoff_ms),
                    ),
                );
            }
        }
        Ok(())
    }
}
fn serve(c: Config, stdout: &mut impl Write) -> Result<Value, String> {
    let models = c
        .model_bindings
        .as_deref()
        .map(crate::models::bindings)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let effect_bindings: Option<Vec<workflow_effect_http::HttpEffectBinding>> = c
        .effect_bindings
        .as_deref()
        .map(|path| {
            let mut bytes = vec![];
            std::fs::File::open(path)
                .and_then(|f| {
                    f.take(workflow_worker::MAX_MESSAGE_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)
                })
                .map_err(|e| e.to_string())?;
            workflow_worker::parse_message(&bytes).map_err(|e| e.to_string())
        })
        .transpose()?;
    let digest =
        workflow_worker::digest(&(&c, &models, &effect_bindings)).map_err(|e| e.to_string())?;
    let effects = effect_bindings
        .map(workflow_effect_http::HttpEffects::new)
        .transpose()
        .map_err(|e| e.to_string())?;
    let server = Server::start(
        Path::new(&c.control_directory),
        Info {
            database: c.database.clone(),
            configuration_digest: digest,
            poll_interval_ms: c.poll_interval_ms,
        },
    )
    .map_err(|e| e.to_string())?;
    let control = server.control();
    writeln!(
        stdout,
        "{}",
        json!({"ok":true,"event":"daemon_started","result":control.snapshot()})
    )
    .and_then(|_| stdout.flush())
    .map_err(|e| e.to_string())?;
    let mut poller = Poller {
        cursor: None,
        delayed: BTreeMap::new(),
        models,
        effects: effects.map(std::sync::Arc::new),
        active: BTreeMap::new(),
        sequence: 0,
    };
    while !control.stopping() {
        if let Err(e) = poller.poll(&c, &control) {
            control.diagnose(None, &format!("storage_{:?}", e.code));
        }
        control.sleep(Duration::from_millis(c.poll_interval_ms));
    }
    while !poller.active.is_empty() {
        poller.reap(&c, &control);
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = control.snapshot();
    drop(server);
    Ok(json!({"stopped":true,"last_status":status}))
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let result = match args {
        ["schema", "daemon-config"] => Ok(json!(schemars::schema_for!(Config))),
        ["daemon", "serve", path] => read_config(path).and_then(|c| serve(c, stdout)),
        ["daemon", "status", dir] => workflow_daemon_local::inspect(dir)
            .map(|r| json!(r))
            .map_err(|e| e.to_string()),
        ["daemon", "stop", dir] => workflow_daemon_local::inspect(dir)
            .map_err(|e| e.to_string())
            .and_then(|r| match r.availability {
                Availability::Stopped => Ok(json!({"stopped":true})),
                Availability::Unreachable => {
                    Err("daemon control is unreachable; stop was not acknowledged".into())
                }
                Availability::Responsive => {
                    workflow_daemon_local::request_stop(dir, &r.status.unwrap().instance)
                        .map(|s| json!({"stop_requested":true,"stopped":false,"status":s}))
                        .map_err(|e| e.to_string())
                }
            }),
        _ => return write(stderr, HELP, 2),
    };
    match result {
        Ok(value) => write(stdout, &json!({"ok":true,"result":value}).to_string(), 0),
        Err(message) => write(
            stdout,
            &json!({"ok":false,"error":{"message":message}}).to_string(),
            1,
        ),
    }
}
#[cfg(test)]
mod tests;
