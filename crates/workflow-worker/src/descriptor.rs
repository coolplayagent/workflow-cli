use crate::{Error, ErrorCode, Result, digest, to_message};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use workflow_ir::{Contract, ValueType, VersionRef};
use workflow_validator::{identifier, pinned_version};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    InvalidInput,
    Permanent,
    PermissionDenied,
    BusinessRejected,
    Transient,
    Cancelled,
    UnknownEffect,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Idempotency {
    None,
    Key { scope: String, retention_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectContract {
    ReadOnly,
    Write {
        #[serde(default, skip_serializing_if = "is_false")]
        irreversible: bool,
        idempotency: Idempotency,
        query: Option<VersionRef>,
        compensation: Option<VersionRef>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilityDescriptor {
    pub schema_version: u32,
    pub capability: VersionRef,
    pub inputs: Contract,
    pub outputs: Contract,
    pub timeout_ms: u64,
    pub error_codes: BTreeMap<String, FailureClass>,
    pub effects: EffectContract,
    pub usage: String,
    pub skill: Option<VersionRef>,
}

/// A validated, immutable descriptor snapshot. No setter can invalidate its digest.
#[derive(Clone, Debug)]
pub struct Capability {
    descriptor: CapabilityDescriptor,
    digest: String,
}
impl Capability {
    pub fn new(descriptor: CapabilityDescriptor) -> Result<Self> {
        let invalid = |message| Error::new(ErrorCode::InvalidDescriptor, message);
        if to_message(&descriptor)?.len() > 131_072 {
            return Err(invalid("descriptor exceeds 128 KiB"));
        }
        if descriptor.schema_version != 1 {
            return Err(invalid("only capability schema 1 is supported"));
        }
        reference(&descriptor.capability)?;
        if let Some(skill) = &descriptor.skill {
            reference(skill)?;
        }
        if descriptor.timeout_ms == 0 || descriptor.timeout_ms > 86_400_000 {
            return Err(invalid("timeout must be 1..86400000 ms"));
        }
        if descriptor.usage.trim().is_empty() || descriptor.usage.len() > 8192 {
            return Err(invalid("usage must contain 1..8192 bytes"));
        }
        if descriptor.error_codes.len() > 128
            || descriptor.error_codes.keys().any(|k| !identifier(k))
        {
            return Err(invalid(
                "error codes must be at most 128 stable identifiers",
            ));
        }
        contract(&descriptor.inputs)?;
        contract(&descriptor.outputs)?;
        match &descriptor.effects {
            EffectContract::ReadOnly => {
                if descriptor
                    .error_codes
                    .values()
                    .any(|c| *c == FailureClass::UnknownEffect)
                {
                    return Err(invalid(
                        "read-only capabilities cannot declare unknown write effects",
                    ));
                }
            }
            EffectContract::Write {
                irreversible,
                idempotency,
                query,
                compensation,
            } => {
                if *irreversible && compensation.is_some() {
                    return Err(invalid(
                        "irreversible effect cannot declare an automatic compensator",
                    ));
                }
                if let Idempotency::Key {
                    scope,
                    retention_ms,
                } = idempotency
                    && (!identifier(scope) || *retention_ms == 0)
                {
                    return Err(invalid(
                        "idempotency requires a stable scope and positive retention",
                    ));
                }
                for r in [query, compensation].into_iter().flatten() {
                    reference(r)?;
                }
            }
        }
        let digest = digest(&descriptor)?;
        Ok(Self { descriptor, digest })
    }
    pub fn descriptor(&self) -> &CapabilityDescriptor {
        &self.descriptor
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
}
fn reference(r: &VersionRef) -> Result<()> {
    if !identifier(&r.id) || !pinned_version(&r.version) {
        return Err(Error::new(
            ErrorCode::InvalidDescriptor,
            "references require a stable ID and pinned version",
        ));
    }
    Ok(())
}
fn contract(contract: &Contract) -> Result<()> {
    if contract.len() > 128 || contract.keys().any(|k| !identifier(k)) {
        return Err(Error::new(
            ErrorCode::InvalidDescriptor,
            "contract must have at most 128 stable field names",
        ));
    }
    let mut pending: Vec<_> = contract.values().map(|f| (&f.value_type, 0)).collect();
    let mut count = 0;
    while let Some((ty, depth)) = pending.pop() {
        count += 1;
        if depth > 16 || count > 4096 {
            return Err(Error::new(
                ErrorCode::InvalidDescriptor,
                "contract exceeds depth 16 or 4096 type members",
            ));
        }
        match ty {
            ValueType::Array { items } => pending.push((items, depth + 1)),
            ValueType::Object { fields } => {
                if fields.len() > 128 || fields.keys().any(|k| k.is_empty() || k.len() > 128) {
                    return Err(Error::new(
                        ErrorCode::InvalidDescriptor,
                        "nested object exceeds member/name limits",
                    ));
                }
                pending.extend(fields.values().map(|f| (f, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}
