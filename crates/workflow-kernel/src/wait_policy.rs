//! Frozen responder and subject rules for durable waits. Identity is attested by
//! the ingress host; a source string is never itself an authentication mechanism.
use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use workflow_ir::{NodeKind, ValueType, VersionRef, Workflow};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    HumanApproval,
    ExternalEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    Digest,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExceptionPolicy {
    pub identity: VersionRef,
    pub responders: BTreeSet<String>,
    pub codes: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitPolicy {
    pub identity: VersionRef,
    pub kind: WaitKind,
    pub responders: BTreeSet<String>,
    /// Names of required wait inputs whose exact values are being reviewed.
    pub subjects: BTreeMap<String, SubjectKind>,
    /// Response expiry must be within this duration of durable receipt.
    pub max_validity_ms: u64,
    pub exception: Option<ExceptionPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitPolicyBinding {
    pub workflow: VersionRef,
    pub node_id: String,
    pub policy: WaitPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExceptionClaim {
    pub policy: VersionRef,
    pub code: String,
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidBundle, message)
}
fn reference(r: &VersionRef) -> bool {
    workflow_validator::identifier(&r.id) && workflow_validator::pinned_version(&r.version)
}
fn names(values: &BTreeSet<String>) -> bool {
    !values.is_empty()
        && values.len() <= 128
        && values.iter().all(|v| workflow_validator::identifier(v))
}
pub(crate) fn validate(spec: &BundleSpec, workflows: &BTreeMap<String, Workflow>) -> Result<()> {
    if spec.wait_policies.len() > 256 {
        return Err(invalid("at most 256 wait policies"));
    }
    let mut nodes = BTreeSet::new();
    let mut identities = BTreeMap::new();
    for binding in &spec.wait_policies {
        let policy = &binding.policy;
        if !reference(&policy.identity)
            || !names(&policy.responders)
            || policy.subjects.len() > 32
            || (policy.kind == WaitKind::HumanApproval && policy.subjects.is_empty())
            || !(1..=2_592_000_000).contains(&policy.max_validity_ms)
        {
            return Err(invalid(
                "wait policy requires a pinned identity, 1..128 responders, at most 32 subjects (at least one for human approval) and validity 1 ms..30 days",
            ));
        }
        let wk = crate::bundle::key(&binding.workflow);
        let node = workflows
            .get(&wk)
            .and_then(|w| w.nodes.iter().find(|n| n.id == binding.node_id))
            .ok_or_else(|| invalid("wait policy target is missing"))?;
        if !matches!(node.kind, NodeKind::Wait { .. })
            || !nodes.insert((wk, binding.node_id.clone()))
        {
            return Err(invalid("wait policy requires a unique wait node"));
        }
        for (input, kind) in &policy.subjects {
            let ty = match kind {
                SubjectKind::Digest => ValueType::String,
                SubjectKind::Artifact => ValueType::Object {
                    fields: BTreeMap::from([
                        ("artifact_id".into(), ValueType::String),
                        ("digest".into(), ValueType::String),
                    ]),
                },
            };
            if !node
                .inputs
                .get(input)
                .is_some_and(|f| f.required && f.value_type == ty)
            {
                return Err(invalid(
                    "approval subject requires an exact required wait input contract",
                ));
            }
        }
        let mut versions = vec![(
            "wait_policy",
            &policy.identity,
            workflow_worker::digest(policy)?,
        )];
        if let Some(exception) = &policy.exception {
            if !reference(&exception.identity)
                || !names(&exception.responders)
                || !names(&exception.codes)
            {
                return Err(invalid(
                    "exception policy requires a pinned identity, responders and allowed codes",
                ));
            }
            versions.push((
                "wait_exception",
                &exception.identity,
                workflow_worker::digest(exception)?,
            ));
        }
        for (kind, identity, digest) in versions {
            let key = (kind, crate::bundle::key(identity));
            if identities
                .insert(key, digest.clone())
                .is_some_and(|old| old != digest)
            {
                return Err(invalid("wait policy version has conflicting contents"));
            }
        }
    }
    Ok(())
}

impl CompiledBundle {
    pub fn wait_policy(&self, workflow: &VersionRef, node: &str) -> Option<&WaitPolicy> {
        self.spec()
            .wait_policies
            .iter()
            .find(|b| &b.workflow == workflow && b.node_id == node)
            .map(|b| &b.policy)
    }
}

fn sha(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

pub(crate) fn rejection(
    policy: Option<&WaitPolicy>,
    message: &SignalMessage,
    received_at: u64,
    inputs: Option<&Values>,
) -> Option<SignalRejection> {
    use SignalRejection::*;
    let Some(policy) = policy else {
        return message.exception.as_ref().map(|_| ExceptionNotAllowed);
    };
    if message.expires_at_unix_ms.saturating_sub(received_at) > policy.max_validity_ms {
        return Some(ResponseValidityExceeded);
    }
    if let Some(claim) = &message.exception {
        let Some(exception) = &policy.exception else {
            return Some(ExceptionNotAllowed);
        };
        if message.decision != SignalDecision::Approve
            || claim.policy != exception.identity
            || !exception.codes.contains(&claim.code)
            || !exception.responders.contains(&message.source)
        {
            return Some(ExceptionNotAllowed);
        }
    } else if !policy.responders.contains(&message.source) {
        return Some(ResponderNotAllowed);
    }
    if let Some(inputs) = inputs {
        for (field, kind) in &policy.subjects {
            let valid = inputs.get(field).is_some_and(|value| match kind {
                SubjectKind::Digest => value.as_str().is_some_and(sha),
                SubjectKind::Artifact => value.as_object().is_some_and(|obj| {
                    obj.len() == 2
                        && obj
                            .get("artifact_id")
                            .and_then(|v| v.as_str())
                            .is_some_and(workflow_validator::identifier)
                        && obj.get("digest").and_then(|v| v.as_str()).is_some_and(sha)
                }),
            });
            if !valid {
                return Some(InvalidSubject);
            }
        }
    }
    None
}
