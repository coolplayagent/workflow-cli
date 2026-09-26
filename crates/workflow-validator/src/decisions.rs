use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use workflow_ir::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DecisionError {
    pub code: String,
    pub field: Option<String>,
    pub message: String,
}
impl DecisionError {
    fn new(code: &str, field: Option<&str>, message: &str) -> Self {
        Self {
            code: code.into(),
            field: field.map(str::to_owned),
            message: message.into(),
        }
    }
}

pub fn validate_values(
    contract: &Contract,
    values: &BTreeMap<String, Value>,
) -> Result<(), DecisionError> {
    for (name, field) in contract {
        match values.get(name) {
            None if field.required => {
                return Err(DecisionError::new(
                    "missing_value",
                    Some(name),
                    "required value is missing",
                ));
            }
            Some(value) if !field.value_type.accepts(value) => {
                return Err(DecisionError::new(
                    "value_type",
                    Some(name),
                    "value does not match declared type",
                ));
            }
            _ => {}
        }
    }
    for name in values.keys() {
        if !contract.contains_key(name) {
            return Err(DecisionError::new(
                "unknown_value",
                Some(name),
                "value is not declared in the contract",
            ));
        }
    }
    Ok(())
}

/// Logical groups short-circuit left to right. Use `exists` before comparing optional fields.
pub fn evaluate(
    condition: &Condition,
    values: &BTreeMap<String, Value>,
) -> Result<bool, DecisionError> {
    match condition {
        Condition::Exists { field } => Ok(values.contains_key(field)),
        Condition::Eq { field, value } | Condition::NotEq { field, value } => {
            let actual = values.get(field).ok_or_else(|| {
                DecisionError::new(
                    "missing_condition_value",
                    Some(field),
                    "comparison value is missing; use exists to guard optional values",
                )
            })?;
            Ok(if matches!(condition, Condition::Eq { .. }) {
                actual == value
            } else {
                actual != value
            })
        }
        Condition::Not { condition } => Ok(!evaluate(condition, values)?),
        Condition::All { conditions } => {
            if conditions.is_empty() {
                return Err(DecisionError::new(
                    "empty_condition",
                    None,
                    "all requires at least one operand",
                ));
            }
            for c in conditions {
                if !evaluate(c, values)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Condition::Any { conditions } => {
            if conditions.is_empty() {
                return Err(DecisionError::new(
                    "empty_condition",
                    None,
                    "any requires at least one operand",
                ));
            }
            for c in conditions {
                if evaluate(c, values)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// All case expressions are evaluated before selection; a bad case never disappears behind a hit.
pub fn select_branch<'a>(
    node: &Node,
    outgoing: &[&'a Edge],
    values: &BTreeMap<String, Value>,
) -> Result<&'a Edge, DecisionError> {
    let NodeKind::Decision { mode } = &node.kind else {
        return Err(DecisionError::new(
            "not_decision",
            None,
            "node is not a decision",
        ));
    };
    validate_values(&node.inputs, values)?;
    let mut hits = vec![];
    let mut fallback = None;
    let mut cases = 0;
    for edge in outgoing {
        if edge.from != node.id {
            return Err(DecisionError::new(
                "invalid_route",
                None,
                "edge belongs to a different node",
            ));
        }
        match &edge.route {
            Route::Case { when } => {
                cases += 1;
                if evaluate(when, values)? {
                    hits.push(*edge);
                }
            }
            Route::Otherwise => {
                if fallback.replace(*edge).is_some() {
                    return Err(DecisionError::new(
                        "invalid_route",
                        None,
                        "multiple default routes",
                    ));
                }
            }
            _ => {
                return Err(DecisionError::new(
                    "invalid_route",
                    None,
                    "unsupported decision route",
                ));
            }
        }
    }
    let fallback = fallback.ok_or_else(|| {
        DecisionError::new("no_default", None, "a decision requires a default route")
    })?;
    if cases == 0 {
        return Err(DecisionError::new(
            "invalid_route",
            None,
            "a decision requires at least one case",
        ));
    }
    if hits.len() > 1 && *mode == DecisionMode::Exclusive {
        return Err(DecisionError::new(
            "multiple_matches",
            None,
            "exclusive decision matched more than one case",
        ));
    }
    Ok(hits.first().copied().unwrap_or(fallback))
}
