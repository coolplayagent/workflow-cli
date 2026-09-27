use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow_ir::{ValueType, VersionRef};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentSchema {
    Bytes,
    Utf8,
    Json { value_type: ValueType },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactType {
    pub identity: VersionRef,
    pub content: ContentSchema,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Producer {
    pub run_id: String,
    pub node_instance_id: String,
    pub attempt_id: String,
    pub request_digest: String,
    pub input_digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceRevision {
    pub repository: String,
    pub revision: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    RunDependency,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessScope {
    Run { run_id: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactLink {
    pub artifact_id: String,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishSpec {
    pub schema_version: u32,
    pub artifact_type: ArtifactType,
    pub producer: Producer,
    pub source_revision: SourceRevision,
    pub inputs: Vec<ArtifactLink>,
    pub access: AccessScope,
    pub retention: Retention,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub spec: PublishSpec,
    pub content_digest: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub digest: String,
    pub location: String,
    pub manifest: Manifest,
}
impl ArtifactRef {
    pub fn link(&self) -> ArtifactLink {
        ArtifactLink {
            artifact_id: self.artifact_id.clone(),
            digest: self.digest.clone(),
        }
    }
}
