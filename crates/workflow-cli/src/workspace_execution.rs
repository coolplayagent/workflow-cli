use serde::Deserialize;
use std::{cell::RefCell, collections::BTreeMap, path::PathBuf};
use workflow_artifacts::{ArtifactLink, ArtifactStore, ArtifactType, SourceRevision};
use workflow_worker::{
    AdapterOutcome, Clock, Error, ErrorCode, ExecutionGrant, WorkRequest, WorkResult, Worker,
};
use workflow_workspace_local::{GitSource, LocalWorkspaceStore};
use workflow_workspaces::{CheckoutSpec, MergePolicy, OutputFile, WorkspaceStore};

#[derive(Clone, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub workspace_store: PathBuf,
    pub repository_path: PathBuf,
    pub source_revision: SourceRevision,
    pub capabilities: Vec<TaskBinding>,
}
#[derive(Clone, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskBinding {
    capability: workflow_ir::VersionRef,
    /// Inline UTF-8 request value -> exact committed workspace path.
    inline_files: BTreeMap<String, String>,
    #[serde(default)]
    input_artifacts: BTreeMap<String, ArtifactType>,
    reports: Vec<ReportBinding>,
}
#[derive(Clone, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportBinding {
    path: String,
    artifact_type: ArtifactType,
    /// Exact fields copied from accepted worker outputs into a typed JSON report.
    fields: Vec<String>,
}
pub(crate) struct Executor {
    binding: Binding,
    worker: Worker,
    workspaces: RefCell<LocalWorkspaceStore>,
    artifacts: RefCell<Box<dyn ArtifactStore>>,
    binary_digest: String,
}
struct ExecutionSource<'a> {
    source: GitSource,
    tools: BTreeMap<String, String>,
    binding: &'a SourceRevision,
}
impl workflow_workspaces::WorkspaceSource for ExecutionSource<'_> {
    fn read(
        &self,
        reference: &SourceRevision,
    ) -> workflow_workspaces::Result<workflow_workspaces::SourceTree> {
        if reference != self.binding {
            return Err(workflow_workspaces::Error::new(
                workflow_workspaces::ErrorCode::InvalidReference,
                "workspace source binding changed",
            ));
        }
        let mut tree = self.source.read(reference)?;
        tree.environment.tools = self.tools.clone();
        Ok(tree)
    }
}
fn rejected(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidBinding, message)
}
impl Executor {
    pub fn new(
        binding: Binding,
        artifacts: Box<dyn ArtifactStore>,
    ) -> workflow_worker::Result<Self> {
        if binding.capabilities.is_empty() || binding.capabilities.len() > 64 {
            return Err(rejected(
                "workspace task bindings require 1..64 capabilities",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for task in &binding.capabilities {
            if !seen.insert((&task.capability.id, &task.capability.version))
                || task.inline_files.is_empty()
                || task.inline_files.len() > 64
                || task.input_artifacts.len() > 64
                || task.reports.is_empty()
                || task.reports.len() > 64
            {
                return Err(rejected(
                    "workspace capability bindings must be unique and bounded",
                ));
            }
            for path in task.inline_files.values() {
                workflow_workspaces::validate_path(path).map_err(|e| rejected(e.message))?;
            }
            for report in &task.reports {
                workflow_workspaces::validate_path(&report.path)
                    .map_err(|e| rejected(e.message))?;
                if task.inline_files.values().any(|path| path == &report.path)
                    || report.fields.is_empty()
                    || report.fields.len() > 128
                    || report
                        .fields
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        != report.fields.len()
                {
                    return Err(rejected(
                        "report fields must be unique and cannot overwrite source inputs",
                    ));
                }
            }
        }
        let workspaces =
            LocalWorkspaceStore::open(&binding.workspace_store).map_err(|e| rejected(e.message))?;
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut binary = std::fs::File::open(
            std::env::current_exe().map_err(|_| rejected("executable identity unavailable"))?,
        )
        .map_err(|_| rejected("executable identity unavailable"))?;
        if binary
            .metadata()
            .map_err(|_| rejected("executable metadata unavailable"))?
            .len()
            > 512 * 1024 * 1024
        {
            return Err(rejected("executable exceeds observed tool budget"));
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut size = 0u64;
        loop {
            let count = binary
                .read(&mut buffer)
                .map_err(|_| rejected("executable read failed"))?;
            if count == 0 {
                break;
            }
            size += count as u64;
            if size > 512 * 1024 * 1024 {
                return Err(rejected("executable changed beyond observed tool budget"));
            }
            hash.update(&buffer[..count]);
        }
        let binary_digest = format!("sha256:{:x}", hash.finalize());
        Ok(Self {
            binding,
            worker: workflow_builtin_capabilities::worker()?,
            workspaces: RefCell::new(workspaces),
            artifacts: RefCell::new(artifacts),
            binary_digest,
        })
    }
}
impl workflow_runtime::TaskExecutor for Executor {
    fn execute_task(
        &self,
        request: &WorkRequest,
        grant: &ExecutionGrant,
        clock: &dyn Clock,
    ) -> workflow_worker::Result<WorkResult> {
        request.validate_shape()?;
        if grant.request_digest != workflow_worker::digest(request)?
            || clock.now_unix_ms()? >= grant.expires_at_unix_ms
        {
            return Err(rejected("workspace request grant is not current"));
        }
        let task = self
            .binding
            .capabilities
            .iter()
            .find(|t| t.capability == request.capability)
            .ok_or_else(|| rejected("task has no frozen host workspace binding"))?;
        let mut artifacts = self.artifacts.borrow_mut();
        let mut inputs = vec![];
        for (field, ty) in &task.input_artifacts {
            let link: ArtifactLink = workflow_artifacts::parse_message(
                &serde_json::to_vec(
                    request
                        .inputs
                        .get(field)
                        .ok_or_else(|| rejected("artifact input missing"))?,
                )
                .map_err(|_| rejected("artifact input encoding"))?,
            )
            .map_err(|e| rejected(e.message))?;
            workflow_artifacts::verify_expected(artifacts.as_ref(), &link, ty)
                .map_err(|e| rejected(e.message))?;
            inputs.push(link);
        }
        inputs.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
        inputs.dedup();
        let spec = CheckoutSpec {
            schema_version: 1,
            producer: workflow_runstore::artifact_producer(request)
                .map_err(|e| rejected(e.message))?,
            source_revision: self.binding.source_revision.clone(),
            merge_policy: MergePolicy::Explicit,
            inputs,
            outputs: task
                .reports
                .iter()
                .map(|r| OutputFile {
                    path: r.path.clone(),
                    artifact_type: r.artifact_type.clone(),
                })
                .collect(),
        };
        let mut workspaces = self.workspaces.borrow_mut();
        let source = GitSource::open(
            &spec.source_revision.repository,
            &self.binding.repository_path,
        )
        .map_err(|e| rejected(e.message))?;
        let source = ExecutionSource {
            source,
            binding: &self.binding.source_revision,
            tools: BTreeMap::from([
                ("workflow-binary".into(), self.binary_digest.clone()),
                ("workflow-version".into(), env!("CARGO_PKG_VERSION").into()),
                (
                    "capability".into(),
                    format!("{}@{}", request.capability.id, request.capability.version),
                ),
                (
                    "capability-contract".into(),
                    request.contract_digest.clone(),
                ),
            ]),
        };
        let reference = workspaces
            .checkout(&spec, &source)
            .map_err(|e| rejected(e.message))?;
        if reference.manifest.environment.tools != source.tools {
            return Err(rejected(
                "attempt was allocated under a different executable/tool binding",
            ));
        }
        let before = workspaces
            .observe(&reference.link())
            .map_err(|e| rejected(e.message))?;
        if before.changes.iter().any(|c| !c.declared_output) {
            return Err(rejected("attempt source differs from allocated revision"));
        }
        for (field, path) in &task.inline_files {
            let value = request
                .inputs
                .get(field)
                .and_then(|v| v.as_str())
                .ok_or_else(|| rejected("workspace input must be an inline UTF-8 string"))?;
            if workspaces
                .read_file(&reference.link(), path)
                .map_err(|e| rejected(e.message))?
                != value.as_bytes()
            {
                return Err(rejected(
                    "actual request input differs from committed workspace bytes",
                ));
            }
        }
        let mut result = self
            .worker
            .execute_with_clock(request, grant, clock)?
            .into_result();
        if let AdapterOutcome::Succeeded { outputs, evidence } = &mut result.outcome {
            for report in &task.reports {
                let mut fields = BTreeMap::new();
                for name in &report.fields {
                    fields.insert(
                        name,
                        outputs
                            .get(name)
                            .ok_or_else(|| rejected("accepted output field missing from report"))?,
                    );
                }
                workspaces
                    .write_output(
                        &reference.link(),
                        &report.path,
                        &workflow_artifacts::to_message(&fields)
                            .map_err(|e| rejected(e.message))?,
                    )
                    .map_err(|e| rejected(e.message))?;
            }
            let captured = workspaces
                .capture(&reference.link(), artifacts.as_mut())
                .map_err(|e| rejected(e.message))?;
            if captured
                .observation
                .changes
                .iter()
                .any(|c| !c.declared_output)
            {
                return Err(rejected("source changed during execution"));
            }
            evidence.extend(captured.files.iter().chain([&captured.manifest]).map(|r| {
                workflow_worker::EvidenceRef {
                    artifact_id: r.artifact_id.clone(),
                    digest: r.digest.clone(),
                }
            }));
        }
        Ok(result)
    }
}
