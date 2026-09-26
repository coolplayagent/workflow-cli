//! Useful read-only capabilities over the same compiler used by the standalone CLI.
use serde_json::json;
use std::collections::BTreeMap;
use workflow_ir::{Contract, Field, Format, ValueType, VersionRef};
use workflow_worker::*;

pub struct ValidateDefinition;
pub struct CanonicalizeDefinition;

fn field(value_type: ValueType, required: bool) -> Field {
    Field {
        value_type,
        required,
    }
}
fn string() -> Field {
    field(ValueType::String, true)
}
fn inputs() -> Contract {
    BTreeMap::from([("document".into(), string()), ("format".into(), string())])
}
fn descriptor(id: &str, outputs: Contract, usage: &str) -> CapabilityDescriptor {
    CapabilityDescriptor {
        schema_version: 1,
        capability: VersionRef {
            id: id.into(),
            version: "1.0.0".into(),
        },
        inputs: inputs(),
        outputs,
        timeout_ms: 60_000,
        error_codes: BTreeMap::from([
            ("invalid_format".into(), FailureClass::InvalidInput),
            ("invalid_definition".into(), FailureClass::InvalidInput),
        ]),
        effects: EffectContract::ReadOnly,
        usage: usage.into(),
        skill: Some(VersionRef {
            id: "workflow-capability".into(),
            version: "1.0.0".into(),
        }),
    }
}
fn failure(code: &str, message: &str) -> AdapterOutcome {
    AdapterOutcome::Failed {
        code: code.into(),
        class: FailureClass::InvalidInput,
        message: message.into(),
        evidence: vec![],
    }
}
fn compile(
    inputs: &Values,
) -> std::result::Result<
    (Option<workflow_ir::Workflow>, Vec<workflow_ir::Diagnostic>),
    AdapterOutcome,
> {
    let format = match inputs.get("format").and_then(|v| v.as_str()) {
        Some("json") => Format::Json,
        Some("yaml") => Format::Yaml,
        _ => return Err(failure("invalid_format", "format must be json or yaml")),
    };
    let Some(document) = inputs.get("document").and_then(|v| v.as_str()) else {
        return Err(failure("invalid_definition", "document must be a string"));
    };
    match workflow_ir::parse(document, format, "document") {
        Err(d) => Ok((None, vec![*d])),
        Ok(w) => {
            let diagnostics = workflow_validator::validate(&w, "document");
            Ok((diagnostics.is_empty().then_some(w), diagnostics))
        }
    }
}
impl CapabilityAdapter for ValidateDefinition {
    fn descriptor(&self) -> CapabilityDescriptor {
        let diagnostic = ValueType::Object {
            fields: ["code", "file", "path", "node", "edge", "message"]
                .into_iter()
                .map(|n| (n.into(), ValueType::String))
                .collect(),
        };
        descriptor(
            "workflow.validate",
            BTreeMap::from([
                ("valid".into(), field(ValueType::Boolean, true)),
                ("digest".into(), field(ValueType::String, false)),
                (
                    "diagnostics".into(),
                    field(
                        ValueType::Array {
                            items: Box::new(diagnostic),
                        },
                        true,
                    ),
                ),
            ]),
            "Validate an in-memory JSON/YAML workflow. Invalid definitions are a successful inspection with valid=false and diagnostics; digest is absent. No filesystem or model access.",
        )
    }
    fn invoke(&self, invocation: Invocation<'_>) -> AdapterOutcome {
        let (workflow, diagnostics) = match compile(&invocation.request.inputs) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let mut outputs = Values::from([
            ("valid".into(), json!(workflow.is_some())),
            ("diagnostics".into(), json!(diagnostics.into_iter().map(|d| json!({"code":d.code,"file":d.file,"path":d.path,"node":d.node.unwrap_or_default(),"edge":d.edge.unwrap_or_default(),"message":d.message})).collect::<Vec<_>>())),
        ]);
        if let Some(w) = workflow {
            match w.digest() {
                Ok(d) => {
                    outputs.insert("digest".into(), json!(d));
                }
                Err(_) => return failure("invalid_definition", "definition could not be digested"),
            }
        }
        AdapterOutcome::Succeeded {
            outputs,
            evidence: vec![],
        }
    }
}
impl CapabilityAdapter for CanonicalizeDefinition {
    fn descriptor(&self) -> CapabilityDescriptor {
        descriptor(
            "workflow.canonicalize",
            BTreeMap::from([("document".into(), string()), ("digest".into(), string())]),
            "Validate and export canonical workflow-v1 JSON plus its content digest. Invalid definitions fail; inspect them with workflow.validate. No filesystem or model access.",
        )
    }
    fn invoke(&self, invocation: Invocation<'_>) -> AdapterOutcome {
        let workflow = match compile(&invocation.request.inputs) {
            Ok((Some(w), _)) => w,
            Ok(_) => {
                return failure(
                    "invalid_definition",
                    "definition failed validation; use workflow.validate for diagnostics",
                );
            }
            Err(e) => return e,
        };
        match (workflow.canonical_json(), workflow.digest()) {
            (Ok(document), Ok(digest)) => AdapterOutcome::Succeeded {
                outputs: Values::from([
                    ("document".into(), json!(document)),
                    ("digest".into(), json!(digest)),
                ]),
                evidence: vec![],
            },
            _ => failure("invalid_definition", "canonicalization failed"),
        }
    }
}
pub fn worker() -> Result<Worker> {
    let mut worker = Worker::default();
    worker.register(ValidateDefinition)?;
    worker.register(CanonicalizeDefinition)?;
    Ok(worker)
}

#[cfg(test)]
mod tests;
