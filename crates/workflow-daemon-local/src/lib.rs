//! Private local daemon control. Live IPC, not a PID file, proves responsiveness.
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
mod unix;
pub use unix::{Server, inspect, request_stop};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Info {
    pub database: String,
    pub configuration_digest: String,
    pub poll_interval_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Polling,
    Busy,
    Draining,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub run_id: Option<String>,
    pub code: String,
    pub at_unix_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub instance: String,
    pub pid: u32,
    pub started_at_unix_ms: u64,
    pub observed_at_unix_ms: u64,
    pub info: Info,
    pub phase: Phase,
    pub active_run: Option<String>,
    #[serde(default)]
    pub active_runs: Vec<String>,
    pub last_scan_unix_ms: Option<u64>,
    pub last_completed_run: Option<String>,
    pub completed_drives: u64,
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Stopped,
    Responsive,
    Unreachable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub availability: Availability,
    pub status: Option<Status>,
}
#[derive(Clone)]
pub struct Control(Arc<Mutex<Status>>);
impl Control {
    pub fn stopping(&self) -> bool {
        self.0.lock().expect("daemon state").phase == Phase::Draining
    }
    /// Reserve one bounded drive before stop linearizes. An admitted drive drains.
    pub fn admit(&self, run_id: &str) -> bool {
        let mut s = self.0.lock().expect("daemon state");
        if s.phase == Phase::Draining
            || s.active_runs.len() >= 8
            || s.active_runs.iter().any(|id| id == run_id)
        {
            return false;
        }
        s.phase = Phase::Busy;
        s.active_runs.push(run_id.into());
        s.active_run = s.active_runs.first().cloned();
        true
    }
    pub fn complete(&self, run_id: &str) {
        let mut s = self.0.lock().expect("daemon state");
        s.active_runs.retain(|id| id != run_id);
        s.active_run = s.active_runs.first().cloned();
        s.last_completed_run = Some(run_id.into());
        s.completed_drives = s.completed_drives.saturating_add(1);
        if s.phase != Phase::Draining {
            s.phase = if s.active_runs.is_empty() {
                Phase::Polling
            } else {
                Phase::Busy
            };
        }
    }
    pub fn scanned(&self) {
        self.0.lock().expect("daemon state").last_scan_unix_ms = Some(now());
    }
    /// Codes only: provider bodies, inputs, secrets and arbitrary error text stay out.
    pub fn diagnose(&self, run_id: Option<&str>, code: &str) {
        let mut s = self.0.lock().expect("daemon state");
        if s.diagnostics.len() == 32 {
            s.diagnostics.remove(0);
        }
        s.diagnostics.push(Diagnostic {
            run_id: run_id.map(str::to_owned),
            code: code.chars().take(128).collect(),
            at_unix_ms: now(),
        });
    }
    pub fn snapshot(&self) -> Status {
        let mut s = self.0.lock().expect("daemon state").clone();
        s.observed_at_unix_ms = now();
        s
    }
    pub fn sleep(&self, duration: Duration) {
        let end = std::time::Instant::now() + duration;
        while !self.stopping() && std::time::Instant::now() < end {
            std::thread::sleep(
                Duration::from_millis(20)
                    .min(end.saturating_duration_since(std::time::Instant::now())),
            );
        }
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

#[cfg(test)]
mod tests;
