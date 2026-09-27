use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow_artifacts::{ArtifactLink, ArtifactRef, ArtifactType, Producer, SourceRevision};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergePolicy {
    /// Changes are proposals. A separate authorized merge must produce a new verified revision.
    Explicit,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutSpec {
    pub schema_version: u32,
    pub producer: Producer,
    pub source_revision: SourceRevision,
    pub merge_policy: MergePolicy,
    pub inputs: Vec<ArtifactLink>,
    /// Exact portable paths, up to 64. Capturing never guesses which files are deliverables.
    pub outputs: Vec<OutputFile>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputFile {
    pub path: String,
    pub artifact_type: ArtifactType,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub digest: String,
    pub bytes: u64,
    pub executable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub os: String,
    pub architecture: String,
    pub git_version: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    pub spec: CheckoutSpec,
    pub git_tree: String,
    pub baseline: Vec<FileEntry>,
    pub tree_digest: String,
    pub environment: Environment,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLink {
    pub workspace_id: String,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
    pub workspace_id: String,
    pub digest: String,
    pub location: String,
    pub manifest: WorkspaceManifest,
}
impl WorkspaceRef {
    pub fn link(&self) -> WorkspaceLink {
        WorkspaceLink {
            workspace_id: self.workspace_id.clone(),
            digest: self.digest.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    pub declared_output: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub workspace: WorkspaceLink,
    pub source_revision: SourceRevision,
    pub tree_digest: String,
    pub files: Vec<FileEntry>,
    pub clean: bool,
    pub changes: Vec<Change>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapturedFile {
    pub path: String,
    pub executable: bool,
    pub artifact_id: String,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputManifest {
    pub workspace_id: String,
    pub workspace_digest: String,
    pub baseline_tree_digest: String,
    pub observed_tree_digest: String,
    pub clean: bool,
    pub files: Vec<CapturedFile>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub observation: Observation,
    pub manifest: ArtifactRef,
    pub files: Vec<ArtifactRef>,
}
/// Trusted source port output; not accepted from deserialized model messages.
pub struct SourceTree {
    pub source_revision: SourceRevision,
    pub git_tree: String,
    pub files: Vec<SourceFile>,
    pub environment: Environment,
}
pub struct SourceFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}
