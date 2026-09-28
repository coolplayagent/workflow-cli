use crate::codec::valid_digest;
use crate::{
    Capability, Error, ErrorCode, FailureClass, PROTOCOL_VERSION, Result, Values, digest,
    to_message,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow_ir::{NodeKind, VersionRef, Workflow};
use workflow_validator::identifier;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationScope {
    Standalone,
    Workflow {
        definition_digest: String,
        run_id: String,
        node_id: String,
        node_instance_id: String,
        attempt_id: String,
        lease_epoch: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub trace_id: String,
    pub scope: InvocationScope,
    pub capability: VersionRef,
    pub contract_digest: String,
    pub inputs: Values,
    pub input_digest: String,
    pub issued_at_unix_ms: u64,
    pub deadline_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_policy: Option<ModelPolicyBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPolicyBinding {
    pub policy: VersionRef,
    pub digest: String,
}
impl ModelPolicyBinding {
    pub fn validate(&self) -> Result<()> {
        if !identifier(&self.policy.id)
            || !workflow_validator::pinned_version(&self.policy.version)
            || !valid_digest(&self.digest)
        {
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "exact model policy identity/digest required",
            ));
        }
        Ok(())
    }
}

/// Issued by a trusted host after authorization. A digest is a binding, not a signature.
/// Keep this on a separate authenticated control channel from model/worker messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionGrant {
    pub protocol_version: u32,
    pub request_digest: String,
    pub expires_at_unix_ms: u64,
}
impl ExecutionGrant {
    /// This helper does not perform authentication or acquire a lease.
    pub fn bind(request: &WorkRequest) -> Result<Self> {
        request.validate_shape()?;
        Ok(Self {
            protocol_version: request.protocol_version,
            request_digest: digest(request)?,
            expires_at_unix_ms: request.deadline_unix_ms,
        })
    }
}

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub request_id: String,
    pub trace_id: String,
    pub issued_at_unix_ms: u64,
    pub deadline_unix_ms: u64,
}
#[derive(Clone, Debug)]
pub struct NodeAttempt {
    pub run_id: String,
    pub node_instance_id: String,
    pub attempt_id: String,
    pub lease_epoch: u64,
}
impl WorkRequest {
    pub fn standalone(
        capability: &Capability,
        inputs: Values,
        context: RequestContext,
    ) -> Result<Self> {
        Self::prepare(capability, inputs, context, InvocationScope::Standalone)
    }
    pub fn for_node(
        workflow: &Workflow,
        node_id: &str,
        capability: &Capability,
        inputs: Values,
        context: RequestContext,
        attempt: NodeAttempt,
    ) -> Result<Self> {
        Self::for_node_with_policy(
            workflow, node_id, capability, inputs, context, attempt, None,
        )
    }
    pub fn for_node_with_policy(
        workflow: &Workflow,
        node_id: &str,
        capability: &Capability,
        inputs: Values,
        context: RequestContext,
        attempt: NodeAttempt,
        model_policy: Option<ModelPolicyBinding>,
    ) -> Result<Self> {
        let document = workflow
            .canonical_json()
            .map_err(|e| Error::new(ErrorCode::InvalidBinding, e.to_string()))?;
        workflow_ir::parse(&document, workflow_ir::Format::Json, "definition")
            .map_err(|e| Error::new(ErrorCode::InvalidBinding, e.message))?;
        if !workflow_validator::validate(workflow, "definition").is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "workflow must pass static validation",
            ));
        }
        let node = workflow
            .nodes
            .iter()
            .find(|n| n.id == node_id)
            .ok_or_else(|| Error::new(ErrorCode::InvalidBinding, "unknown workflow node"))?;
        let NodeKind::Task {
            capability: reference,
            policy,
        } = &node.kind
        else {
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "only task nodes invoke capabilities",
            ));
        };
        if policy.as_ref() != model_policy.as_ref().map(|p| &p.policy) {
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "task policy needs a model adapter; direct capability execution cannot bypass it",
            ));
        }
        let descriptor = capability.descriptor();
        if reference != &descriptor.capability
            || node.inputs != descriptor.inputs
            || node.outputs != descriptor.outputs
        {
            return Err(Error::new(
                ErrorCode::InvalidBinding,
                "node capability and input/output contracts must exactly match the resolved descriptor",
            ));
        }
        workflow_validator::validate_values(&node.inputs, &inputs)
            .map_err(|e| Error::new(ErrorCode::InvalidInput, e.message))?;
        for condition in &node.preconditions {
            if !workflow_validator::evaluate(condition, &inputs)
                .map_err(|e| Error::new(ErrorCode::PreconditionFailed, e.message))?
            {
                return Err(Error::new(
                    ErrorCode::PreconditionFailed,
                    "task precondition is false",
                ));
            }
        }
        let scope = InvocationScope::Workflow {
            definition_digest: workflow
                .digest()
                .map_err(|e| Error::new(ErrorCode::InvalidBinding, e.to_string()))?,
            run_id: attempt.run_id,
            node_id: node_id.into(),
            node_instance_id: attempt.node_instance_id,
            attempt_id: attempt.attempt_id,
            lease_epoch: attempt.lease_epoch,
        };
        let request = Self::prepare(capability, inputs, context, scope)?;
        if let Some(policy) = model_policy {
            request.with_model_policy(policy)
        } else {
            Ok(request)
        }
    }
    pub fn with_model_policy(mut self, policy: ModelPolicyBinding) -> Result<Self> {
        policy.validate()?;
        self.protocol_version = 2;
        self.model_policy = Some(policy);
        self.validate_shape()?;
        Ok(self)
    }
    fn prepare(
        capability: &Capability,
        inputs: Values,
        context: RequestContext,
        scope: InvocationScope,
    ) -> Result<Self> {
        let result = Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: context.request_id,
            trace_id: context.trace_id,
            scope,
            capability: capability.descriptor().capability.clone(),
            contract_digest: capability.digest().into(),
            input_digest: digest(&inputs)?,
            inputs,
            issued_at_unix_ms: context.issued_at_unix_ms,
            deadline_unix_ms: context.deadline_unix_ms,
            model_policy: None,
        };
        result.validate_shape()?;
        workflow_validator::validate_values(&capability.descriptor().inputs, &result.inputs)
            .map_err(|e| Error::new(ErrorCode::InvalidInput, e.message))?;
        Ok(result)
    }
    pub fn validate_shape(&self) -> Result<()> {
        to_message(self)?;
        protocol(self.protocol_version)?;
        if self.protocol_version != if self.model_policy.is_some() { 2 } else { 1 } {
            return Err(Error::new(
                ErrorCode::UnsupportedProtocol,
                "model policy requires protocol 2; direct calls require protocol 1",
            ));
        }
        if let Some(policy) = &self.model_policy {
            policy.validate()?;
        }
        if !identifier(&self.request_id)
            || !identifier(&self.trace_id)
            || !identifier(&self.capability.id)
            || !workflow_validator::pinned_version(&self.capability.version)
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "invalid request, trace or capability identity",
            ));
        }
        if !valid_digest(&self.contract_digest) || !valid_digest(&self.input_digest) {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "digests must be lowercase sha256 values",
            ));
        }
        if self.issued_at_unix_ms == 0 || self.deadline_unix_ms <= self.issued_at_unix_ms {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "deadline must follow positive issue time",
            ));
        }
        if let InvocationScope::Workflow {
            definition_digest,
            run_id,
            node_id,
            node_instance_id,
            attempt_id,
            lease_epoch,
        } = &self.scope
            && (!valid_digest(definition_digest)
                || [run_id, node_id, node_instance_id, attempt_id]
                    .iter()
                    .any(|id| !identifier(id))
                || *lease_epoch == 0)
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "invalid workflow attempt identity or lease epoch",
            ));
        }
        if digest(&self.inputs)? != self.input_digest {
            return Err(Error::new(
                ErrorCode::DigestMismatch,
                "input digest mismatch",
            ));
        }
        Ok(())
    }
}
pub(crate) fn protocol(version: u32) -> Result<()> {
    if !matches!(version, 1 | 2) {
        return Err(Error::new(
            ErrorCode::UnsupportedProtocol,
            "worker protocol 1 or 2 required; negotiate before dispatch",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    pub artifact_id: String,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdapterOutcome {
    Succeeded {
        outputs: Values,
        evidence: Vec<EvidenceRef>,
    },
    Failed {
        code: String,
        class: FailureClass,
        message: String,
        evidence: Vec<EvidenceRef>,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkResult {
    pub protocol_version: u32,
    pub request_digest: String,
    pub completed_at_unix_ms: u64,
    pub outcome: AdapterOutcome,
    /// Explicit model/tool decisions. The policy executor and durable host validate its schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_record: Option<serde_json::Value>,
}

/// Result bytes are only observations until validated against a current host grant.
#[derive(Clone, Debug)]
pub struct AcceptedResult(pub(crate) WorkResult);
impl AcceptedResult {
    pub fn result(&self) -> &WorkResult {
        &self.0
    }
    pub fn into_result(self) -> WorkResult {
        self.0
    }
}

pub fn validate_request(
    request: &WorkRequest,
    grant: &ExecutionGrant,
    capability: &Capability,
    now: u64,
) -> Result<()> {
    request.validate_shape()?;
    protocol(grant.protocol_version)?;
    if grant.protocol_version != request.protocol_version
        || grant.request_digest != digest(request)?
        || grant.expires_at_unix_ms < request.deadline_unix_ms
    {
        return Err(Error::new(
            ErrorCode::Unauthorized,
            "host grant does not authorize this exact request and deadline",
        ));
    }
    if now < request.issued_at_unix_ms
        || now >= request.deadline_unix_ms
        || now >= grant.expires_at_unix_ms
    {
        return Err(Error::new(
            ErrorCode::Expired,
            "request/grant expired or request is from the future",
        ));
    }
    if request.capability != capability.descriptor().capability
        || request.contract_digest != capability.digest()
    {
        return Err(Error::new(
            ErrorCode::DigestMismatch,
            "capability contract identity or digest mismatch",
        ));
    }
    if capability.descriptor().effects != crate::EffectContract::ReadOnly {
        return Err(Error::new(
            ErrorCode::UnsupportedEffect,
            "writes require a durable effect-aware executor; this worker protocol implementation only accepts read-only invocation",
        ));
    }
    workflow_validator::validate_values(&capability.descriptor().inputs, &request.inputs)
        .map_err(|e| Error::new(ErrorCode::InvalidInput, e.message))
}
pub fn accept_result(
    request: &WorkRequest,
    current_grant: &ExecutionGrant,
    capability: &Capability,
    result: WorkResult,
    now: u64,
) -> Result<AcceptedResult> {
    validate_request(request, current_grant, capability, now)?;
    to_message(&result)?;
    protocol(result.protocol_version)?;
    if result.protocol_version != request.protocol_version
        || result.request_digest != digest(request)?
    {
        return Err(Error::new(
            ErrorCode::DigestMismatch,
            "result belongs to another request",
        ));
    }
    if result.model_record.is_some() != request.model_policy.is_some() {
        return Err(Error::new(
            ErrorCode::InvalidResult,
            "model requests require an explicit execution record; direct results cannot contain one",
        ));
    }
    if result.completed_at_unix_ms < request.issued_at_unix_ms || result.completed_at_unix_ms > now
    {
        return Err(Error::new(
            ErrorCode::InvalidResult,
            "invalid completion timestamp",
        ));
    }
    let evidence = match &result.outcome {
        AdapterOutcome::Succeeded { outputs, evidence } => {
            workflow_validator::validate_values(&capability.descriptor().outputs, outputs)
                .map_err(|e| Error::new(ErrorCode::InvalidOutput, e.message))?;
            evidence
        }
        AdapterOutcome::Failed {
            code,
            class,
            message,
            evidence,
        } => {
            if capability.descriptor().error_codes.get(code) != Some(class)
                || message.is_empty()
                || message.len() > 8192
            {
                return Err(Error::new(
                    ErrorCode::InvalidResult,
                    "failure must match a declared code/class and bounded message",
                ));
            }
            evidence
        }
    };
    let mut ids = std::collections::BTreeSet::new();
    if evidence.len() > 128
        || evidence.iter().any(|e| {
            !identifier(&e.artifact_id) || !valid_digest(&e.digest) || !ids.insert(&e.artifact_id)
        })
    {
        return Err(Error::new(
            ErrorCode::InvalidResult,
            "evidence requires unique stable IDs and sha256 digests, at most 128 references",
        ));
    }
    Ok(AcceptedResult(result))
}
