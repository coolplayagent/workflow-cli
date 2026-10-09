//! Portable workflow definitions. This crate has no execution or storage dependency.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_DOCUMENT_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    pub entry: String,
    #[serde(default)]
    pub inputs: Contract,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub type Contract = BTreeMap<String, Field>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub value_type: ValueType,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueType {
    String,
    Boolean,
    Integer,
    Number,
    Array { items: Box<ValueType> },
    Object { fields: BTreeMap<String, ValueType> },
}

impl ValueType {
    /// Objects are closed and all declared members are required; null is not a value.
    pub fn accepts(&self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Boolean => value.is_boolean(),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::Number => value.is_number(),
            Self::Array { items } => value
                .as_array()
                .is_some_and(|a| a.iter().all(|v| items.accepts(v))),
            Self::Object { fields } => value.as_object().is_some_and(|o| {
                o.len() == fields.len()
                    && fields
                        .iter()
                        .all(|(k, t)| o.get(k).is_some_and(|v| t.accepts(v)))
            }),
        }
    }

    pub fn assignable_from(&self, source: &Self) -> bool {
        match (self, source) {
            (Self::Number, Self::Integer) => true,
            (Self::Array { items: a }, Self::Array { items: b }) => a.assignable_from(b),
            (Self::Object { fields: a }, Self::Object { fields: b }) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, t)| b.get(k).is_some_and(|s| t.assignable_from(s)))
            }
            _ => self == source,
        }
    }
}

/// Versions are immutable opaque release identifiers, never ranges or `latest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionRef {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    #[serde(default)]
    pub inputs: Contract,
    #[serde(default)]
    pub outputs: Contract,
    #[serde(default)]
    pub bindings: BTreeMap<String, Binding>,
    #[serde(default)]
    pub preconditions: Vec<Condition>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeKind {
    Task {
        capability: VersionRef,
        policy: Option<VersionRef>,
    },
    Decision {
        mode: DecisionMode,
    },
    Fork,
    Join {
        mode: JoinMode,
        remaining: RemainingPolicy,
    },
    Wait {
        event: String,
        timeout_ms: u64,
    },
    Subworkflow {
        workflow: VersionRef,
    },
    Loop {
        body: VersionRef,
        /// Next-iteration input field -> failed body terminal input field.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        feedback: BTreeMap<String, String>,
        max_iterations: u32,
        deadline_ms: u64,
    },
    Terminal {
        outcome: TerminalOutcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    Exclusive,
    FirstMatch,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    All,
    Any,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemainingPolicy {
    Await,
    CancelAndReconcile,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum Binding {
    WorkflowInput { field: String },
    NodeOutput { node: String, field: String },
    Literal { value: Value },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub route: Route,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Route {
    Next,
    Case { when: Condition },
    Otherwise,
    Completed,
    Exhausted,
    Accepted,
    Rejected,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    Exists { field: String },
    Eq { field: String, value: Value },
    NotEq { field: String, value: Value },
    All { conditions: Vec<Condition> },
    Any { conditions: Vec<Condition> },
    Not { condition: Box<Condition> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Json,
    Yaml,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub file: String,
    pub path: String,
    pub node: Option<String>,
    pub edge: Option<String>,
    pub message: String,
}

pub fn parse(input: &str, format: Format, file: &str) -> Result<Workflow, Box<Diagnostic>> {
    let error = |code: &str, path: String, message: String| {
        Box::new(Diagnostic {
            code: code.into(),
            file: file.into(),
            path,
            node: None,
            edge: None,
            message,
        })
    };
    if input.len() > MAX_DOCUMENT_BYTES {
        return Err(error(
            "document_too_large",
            "$".into(),
            "definition exceeds 1 MiB".into(),
        ));
    }
    match format {
        Format::Json => {
            let mut deserializer = serde_json::Deserializer::from_str(input);
            let workflow = serde_path_to_error::deserialize(&mut deserializer)
                .map_err(|e| error("parse_error", e.path().to_string(), e.inner().to_string()))?;
            deserializer
                .end()
                .map_err(|e| error("parse_error", "$".into(), e.to_string()))?;
            Ok(workflow)
        }
        Format::Yaml => {
            serde_path_to_error::deserialize(serde_yaml_ng::Deserializer::from_str(input))
                .map_err(|e| error("parse_error", e.path().to_string(), e.inner().to_string()))
        }
    }
}

impl Workflow {
    /// Node order is immaterial. Edge order is significant for first-match decisions.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        let mut canonical = self.clone();
        canonical.nodes.sort_by(|a, b| a.id.cmp(&b.id));
        serde_json::to_string(&canonical)
    }

    pub fn digest(&self) -> Result<String, serde_json::Error> {
        Ok(format!(
            "sha256:{:x}",
            Sha256::digest(self.canonical_json()?.as_bytes())
        ))
    }

    pub fn to_yaml(&self) -> Result<String, serde_yaml_ng::Error> {
        serde_yaml_ng::to_string(self)
    }
}

pub fn schema() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&schemars::schema_for!(Workflow))
}

/// Builder and file import produce exactly the same IR; validation stays separate.
pub struct WorkflowBuilder(Workflow);
impl WorkflowBuilder {
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        entry: impl Into<String>,
    ) -> Self {
        Self(Workflow {
            schema_version: SCHEMA_VERSION,
            id: id.into(),
            version: version.into(),
            entry: entry.into(),
            inputs: BTreeMap::new(),
            nodes: vec![],
            edges: vec![],
        })
    }
    pub fn input(mut self, id: impl Into<String>, field: Field) -> Self {
        self.0.inputs.insert(id.into(), field);
        self
    }
    pub fn node(mut self, node: Node) -> Self {
        self.0.nodes.push(node);
        self
    }
    pub fn edge(mut self, edge: Edge) -> Self {
        self.0.edges.push(edge);
        self
    }
    pub fn build(self) -> Workflow {
        self.0
    }
}

#[cfg(test)]
mod tests;
