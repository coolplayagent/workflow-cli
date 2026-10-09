//! Cancellable local activities. The parent alone renews and commits ownership.
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};
use workflow_runtime::{RunningTask, TaskExecutor};
use workflow_worker::{Clock, Error, ErrorCode, ExecutionGrant, Result, WorkRequest, WorkResult};

static NEXT: AtomicU64 = AtomicU64::new(0);
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub bundle: Option<workflow_kernel::BundleSpec>,
    pub models: Vec<crate::models::Binding>,
    pub allow_unused: bool,
    pub journal: Option<JournalBinding>,
    pub remote: Option<workflow_service::ClientBinding>,
    pub workspace: Option<WorkspaceJob>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Invocation {
    config: Config,
    request: WorkRequest,
    grant: ExecutionGrant,
}
fn io_error(_: std::io::Error) -> Error {
    Error::new(ErrorCode::InvalidMessage, "activity process I/O failed")
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(io_error)
}
pub(crate) fn execute_file(path: &str) -> Result<()> {
    let path = Path::new(path);
    let mut bytes = vec![];
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .and_then(|f| {
            f.take(workflow_worker::MAX_MESSAGE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(io_error)?;
    let input: Invocation = workflow_worker::parse_message(&bytes)?;
    let result =
        input
            .config
            .execute_task(&input.request, &input.grant, &workflow_worker::SystemClock);
    private_write(
        &path.with_file_name("result.json"),
        &workflow_worker::to_message(&result)?,
    )
}
impl Config {
    fn worker(&self) -> Result<workflow_worker::Worker> {
        let journal = self
            .journal
            .clone()
            .map(|j| std::sync::Arc::new(j) as std::sync::Arc<dyn workflow_models::SessionJournal>);
        if let Some(bundle) = &self.bundle {
            crate::models::bound_worker_with_journal(
                bundle,
                &self.models,
                self.allow_unused,
                journal,
            )
        } else if self.models.is_empty() {
            workflow_builtin_capabilities::worker()
        } else {
            Err(Error::new(
                ErrorCode::InvalidBinding,
                "model worker requires a frozen bundle",
            ))
        }
    }
    pub fn remote(
        bundle: Option<workflow_kernel::BundleSpec>,
        models: Option<&str>,
        binding: workflow_service::ClientBinding,
    ) -> Result<(Self, Option<workflow_credentials::Principal>)> {
        let (bindings, principal) = match (bundle.as_ref(), models) {
            (Some(bundle), Some(path)) => {
                let (_, principal) = crate::models::shared_worker(bundle, path)?;
                (crate::models::bindings(path)?, principal)
            }
            (None, None) => (vec![], None),
            _ => {
                return Err(Error::new(
                    ErrorCode::InvalidBinding,
                    "model bundle and bindings must be supplied together",
                ));
            }
        };
        Ok((
            Self {
                bundle,
                models: bindings,
                allow_unused: false,
                journal: None,
                remote: Some(binding),
                workspace: None,
            },
            principal,
        ))
    }
}
impl TaskExecutor for Config {
    fn execute_task(
        &self,
        request: &WorkRequest,
        grant: &ExecutionGrant,
        clock: &dyn Clock,
    ) -> Result<WorkResult> {
        if let Some(job) = &self.workspace {
            job.executor()?.execute_task(request, grant, clock)
        } else {
            self.worker()?
                .execute_with_clock(request, grant, clock)
                .map(|v| v.into_result())
        }
    }
    fn start_assigned_task(
        &self,
        assignment_id: &str,
        request: &WorkRequest,
        grant: &ExecutionGrant,
    ) -> Result<Option<Box<dyn RunningTask>>> {
        let mut assigned = self.clone();
        if let Some(binding) = assigned.remote.take() {
            assigned.journal = Some(JournalBinding::Remote {
                binding,
                assignment_id: assignment_id.into(),
            });
        }
        assigned.start_task(request, grant)
    }

    fn start_task(
        &self,
        request: &WorkRequest,
        grant: &ExecutionGrant,
    ) -> Result<Option<Box<dyn RunningTask>>> {
        request.validate_shape()?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::new(ErrorCode::ClockError, "clock unavailable"))?
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "workflow-activity-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(io_error)?;
        let mut operation = Process { child: None, dir };
        let path = operation.dir.join("request.json");
        private_write(
            &path,
            &workflow_worker::to_message(&Invocation {
                config: self.clone(),
                request: request.clone(),
                grant: grant.clone(),
            })?,
        )?;
        let mut command = Command::new(std::env::current_exe().map_err(io_error)?);
        #[cfg(not(test))]
        command.arg("worker").arg("execute-file").arg(&path);
        #[cfg(test)]
        command
            .args(["--exact", "activity::tests::child", "--nocapture"])
            .env("WORKFLOW_ACTIVITY_TEST_INPUT", &path);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let parent = std::process::id() as libc::pid_t;
        // Only async-signal-safe syscalls run between fork and exec. Check the
        // parent again after installing the death signal to close the fork race.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(125);
                }
                Ok(())
            });
        }
        operation.child = Some(command.spawn().map_err(io_error)?);
        Ok(Some(Box::new(operation)))
    }
}
struct Process {
    child: Option<Child>,
    dir: PathBuf,
}
impl RunningTask for Process {
    fn poll(&mut self) -> Result<Option<WorkResult>> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| Error::new(ErrorCode::Expired, "activity already stopped"))?;
        let Some(status) = child.try_wait().map_err(io_error)? else {
            return Ok(None);
        };
        // Reap any descendants even when the direct child exits normally.
        self.cancel()?;
        if !status.success() {
            return Err(Error::new(
                ErrorCode::AdapterPanicked,
                "activity process exited without completion",
            ));
        }
        let mut bytes = vec![];
        std::fs::File::open(self.dir.join("result.json"))
            .and_then(|f| {
                f.take(workflow_worker::MAX_MESSAGE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(io_error)?;
        let result: Result<WorkResult> = workflow_worker::parse_message(&bytes)?;
        result.map(Some)
    }
    fn cancel(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            // The fresh process group belongs solely to this invocation.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            child.wait().map_err(io_error)?;
        }
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.cancel();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn child() {
        if let Ok(path) = std::env::var("WORKFLOW_ACTIVITY_TEST_INPUT") {
            super::execute_file(&path).unwrap();
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ArtifactBinding {
    Local { root: String },
    S3 { file: String },
    InvalidatedLocal { root: String, plan: String },
    InvalidatedS3 { file: String, plan: String },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalJournal {
    database: String,
    artifacts: Option<ArtifactBinding>,
}
impl LocalJournal {
    pub fn new(database: &str, artifacts: Option<crate::runs::ArtifactLocation<'_>>) -> Self {
        use crate::runs::ArtifactLocation as A;
        Self {
            database: database.into(),
            artifacts: artifacts.map(|a| match a {
                A::Local(root) => ArtifactBinding::Local { root: root.into() },
                A::S3(file) => ArtifactBinding::S3 { file: file.into() },
                A::InvalidatedLocal(root, plan) => ArtifactBinding::InvalidatedLocal {
                    root: root.into(),
                    plan: plan.into(),
                },
                A::InvalidatedS3(file, plan) => ArtifactBinding::InvalidatedS3 {
                    file: file.into(),
                    plan: plan.into(),
                },
            }),
        }
    }
    fn open(&self) -> Result<workflow_runstore_sqlite::SqliteRunStore> {
        use crate::runs::ArtifactLocation as A;
        let artifacts = self.artifacts.as_ref().map(|a| match a {
            ArtifactBinding::Local { root } => A::Local(root),
            ArtifactBinding::S3 { file } => A::S3(file),
            ArtifactBinding::InvalidatedLocal { root, plan } => A::InvalidatedLocal(root, plan),
            ArtifactBinding::InvalidatedS3 { file, plan } => A::InvalidatedS3(file, plan),
        });
        crate::runs::open_with(&self.database, artifacts).map_err(journal_error)
    }
}
fn journal_error(_: workflow_runstore::Error) -> Error {
    Error::new(
        ErrorCode::InvalidResult,
        "session checkpoint storage or ownership rejected",
    )
}
impl workflow_models::SessionJournal for LocalJournal {
    fn load(&self, request: &WorkRequest) -> Result<Option<workflow_models::ModelCheckpoint>> {
        use workflow_runstore::ExecutionStore;
        self.open()?
            .model_checkpoint(request, &workflow_worker::SystemClock)
            .map_err(journal_error)
    }
    fn save(
        &self,
        request: &WorkRequest,
        previous: Option<&workflow_models::ModelCheckpoint>,
        next: &workflow_models::ModelCheckpoint,
    ) -> Result<()> {
        use workflow_runstore::ExecutionStore;
        let previous_digest = previous.map(workflow_worker::digest).transpose()?;
        self.open()?
            .save_model_checkpoint(
                request,
                previous_digest.as_deref(),
                next,
                &workflow_worker::SystemClock,
            )
            .map_err(journal_error)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum JournalBinding {
    Local {
        journal: LocalJournal,
    },
    Remote {
        binding: workflow_service::ClientBinding,
        assignment_id: String,
    },
}
impl JournalBinding {
    fn remote_call(
        binding: &workflow_service::ClientBinding,
        operation: workflow_service::Operation,
    ) -> Result<workflow_service::Response> {
        workflow_service::RemoteClient::new(binding.clone())
            .and_then(|c| {
                c.call(&workflow_service::Request {
                    protocol_version: workflow_service::PROTOCOL_VERSION,
                    request_id: "activity-checkpoint".into(),
                    operation,
                })
            })
            .map_err(journal_error)
    }
}
impl workflow_models::SessionJournal for JournalBinding {
    fn load(&self, request: &WorkRequest) -> Result<Option<workflow_models::ModelCheckpoint>> {
        match self {
            Self::Local { journal } => journal.load(request),
            Self::Remote {
                binding,
                assignment_id,
            } => {
                let workflow_service::Response::ModelCheckpoint(checkpoint) = Self::remote_call(
                    binding,
                    workflow_service::Operation::LoadModelCheckpoint {
                        assignment_id: assignment_id.clone(),
                    },
                )?
                else {
                    return Err(Error::new(
                        ErrorCode::InvalidResult,
                        "checkpoint response mismatch",
                    ));
                };
                Ok(checkpoint.map(|c| *c))
            }
        }
    }
    fn save(
        &self,
        request: &WorkRequest,
        previous: Option<&workflow_models::ModelCheckpoint>,
        next: &workflow_models::ModelCheckpoint,
    ) -> Result<()> {
        match self {
            Self::Local { journal } => journal.save(request, previous, next),
            Self::Remote {
                binding,
                assignment_id,
            } => {
                let workflow_service::Response::Unit = Self::remote_call(
                    binding,
                    workflow_service::Operation::SaveModelCheckpoint {
                        assignment_id: assignment_id.clone(),
                        previous_digest: previous.map(workflow_worker::digest).transpose()?,
                        checkpoint: Box::new(next.clone()),
                    },
                )?
                else {
                    return Err(Error::new(
                        ErrorCode::InvalidResult,
                        "checkpoint acknowledgement mismatch",
                    ));
                };
                Ok(())
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceJob {
    binding: crate::workspace_execution::Binding,
    artifacts: ArtifactBinding,
}
impl WorkspaceJob {
    fn executor(&self) -> Result<crate::workspace_execution::Executor> {
        let backing: Box<dyn workflow_artifacts::ArtifactStore> = match &self.artifacts {
            ArtifactBinding::Local { root } | ArtifactBinding::InvalidatedLocal { root, .. } => {
                Box::new(
                    workflow_artifact_local::LocalArtifactStore::open(root).map_err(|_| {
                        Error::new(
                            ErrorCode::InvalidBinding,
                            "workspace artifact store unavailable",
                        )
                    })?,
                )
            }
            ArtifactBinding::S3 { file } | ArtifactBinding::InvalidatedS3 { file, .. } => {
                Box::new(crate::artifact_objects::open(file, false).map_err(|_| {
                    Error::new(
                        ErrorCode::InvalidBinding,
                        "workspace object store unavailable",
                    )
                })?)
            }
        };
        crate::workspace_execution::Executor::new(self.binding.clone(), backing)
    }
}
impl Config {
    pub fn workspace(
        binding: crate::workspace_execution::Binding,
        location: Option<crate::runs::ArtifactLocation<'_>>,
    ) -> Result<Self> {
        let artifacts = LocalJournal::new("", location).artifacts.ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidBinding,
                "workspace execution requires an artifact store binding",
            )
        })?;
        let job = WorkspaceJob { binding, artifacts };
        job.executor()?;
        Ok(Self {
            bundle: None,
            models: vec![],
            allow_unused: false,
            journal: None,
            remote: None,
            workspace: Some(job),
        })
    }
}
