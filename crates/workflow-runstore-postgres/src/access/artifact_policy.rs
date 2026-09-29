use super::*;
use std::collections::{BTreeMap, BTreeSet};
use workflow_artifacts::{ArtifactLink, ArtifactType, PublishSpec, SourceRevision};

/// Host-authored artifact authority for one exact capability contract. Empty or
/// absent policy grants no artifact access. Input names refer to the committed
/// task's top-level inputs, never to fields chosen by a worker at execution time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPolicy {
    pub inputs: BTreeMap<String, ArtifactType>,
    pub output: Option<ArtifactOutputPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactOutputPolicy {
    pub types: Vec<ArtifactType>,
    pub repository_input: String,
    pub revision_input: String,
}

impl ArtifactPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.inputs.len() > 32 || (self.inputs.is_empty() && self.output.is_none()) {
            return Err(denied());
        }
        for (name, ty) in &self.inputs {
            validate_id(name)?;
            workflow_artifacts::validate_type(ty)?;
        }
        if let Some(output) = &self.output {
            if output.types.is_empty() || output.types.len() > 16 {
                return Err(denied());
            }
            validate_id(&output.repository_input)?;
            validate_id(&output.revision_input)?;
            if output.repository_input == output.revision_input {
                return Err(denied());
            }
            let mut versions = BTreeSet::new();
            for ty in &output.types {
                workflow_artifacts::validate_type(ty)?;
                if !versions.insert((&ty.identity.id, &ty.identity.version)) {
                    return Err(denied());
                }
            }
        }
        Ok(())
    }

    /// Resolve only input links explicitly named by the trusted capability rule.
    /// The catalog must still verify their scope, exact type, bytes and lineage.
    pub fn input_links(
        &self,
        request: &workflow_worker::WorkRequest,
    ) -> Result<Vec<(ArtifactLink, ArtifactType)>> {
        self.validate()?;
        self.inputs
            .iter()
            .map(|(name, ty)| {
                let value = request.inputs.get(name).ok_or_else(denied)?;
                let link: ArtifactLink =
                    serde_json::from_value(value.clone()).map_err(|_| denied())?;
                workflow_artifacts::validate_link(&link)?;
                Ok((link, ty.clone()))
            })
            .collect()
    }

    /// Derive publication provenance from a server-held prepared task. This
    /// binds the reported source revision to declared inputs; it does not claim
    /// the worker's filesystem was independently measured by this function.
    pub fn publication_spec(
        &self,
        request: &workflow_worker::WorkRequest,
        ty: &ArtifactType,
    ) -> Result<PublishSpec> {
        self.validate()?;
        let output = self.output.as_ref().ok_or_else(denied)?;
        if !output.types.contains(ty) {
            return Err(denied());
        }
        let string = |name: &str| -> Result<String> {
            request
                .inputs
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(denied)
        };
        let producer = artifact_producer(request)?;
        let mut inputs = BTreeMap::new();
        for (link, _) in self.input_links(request)? {
            inputs.insert(link.artifact_id.clone(), link);
        }
        let spec = PublishSpec {
            schema_version: 1,
            artifact_type: ty.clone(),
            access: workflow_artifacts::AccessScope::Run {
                run_id: producer.run_id.clone(),
            },
            producer,
            source_revision: SourceRevision {
                repository: string(&output.repository_input)?,
                revision: string(&output.revision_input)?,
            },
            inputs: inputs.into_values().collect(),
            retention: workflow_artifacts::Retention::RunDependency,
        };
        workflow_artifacts::validate_spec(&spec)?;
        Ok(spec)
    }
}
