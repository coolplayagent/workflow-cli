use crate::*;
use serde::{Deserialize, Serialize};

pub const MAX_REPORT_BYTES: usize = 1_048_576;
pub const MAX_DIAGNOSTIC_LABEL_BYTES: usize = 1024;

fn label_error(file: &str) -> Option<ValidationReport> {
    (file.len() > MAX_DIAGNOSTIC_LABEL_BYTES).then(|| {
        ValidationReport::rejected(Diagnostic {
            code: "diagnostic_label_limit".into(),
            file: "<oversized diagnostic label>".into(),
            path: "$".into(),
            node: None,
            edge: None,
            message: "diagnostic label exceeds 1024 bytes".into(),
        })
    })
}

/// Portable static compiler output. A diagnostic label is carried through for
/// display; this module never opens it as a filesystem path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationReport {
    pub valid: bool,
    pub digest: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}
impl ValidationReport {
    pub fn rejected(diagnostic: Diagnostic) -> Self {
        Self {
            valid: false,
            digest: None,
            diagnostics: vec![diagnostic],
        }
        .bounded()
    }
    fn bounded(self) -> Self {
        struct Limit(usize);
        impl std::io::Write for Limit {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > self.0 {
                    return Err(std::io::Error::other("report capacity exceeded"));
                }
                self.0 -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        if serde_json::to_writer(Limit(MAX_REPORT_BYTES), &self).is_ok() {
            return self;
        }
        let file = self
            .diagnostics
            .first()
            .map(|d| d.file.as_str())
            .filter(|f| f.len() <= MAX_DIAGNOSTIC_LABEL_BYTES)
            .unwrap_or("<oversized diagnostic label>");
        Self {
            valid: false,
            digest: None,
            diagnostics: vec![Diagnostic {
                code: "diagnostic_limit".into(),
                file: file.into(),
                path: "$".into(),
                node: None,
                edge: None,
                message: "diagnostic report exceeds 1 MiB; definition rejected".into(),
            }],
        }
    }
    pub fn check(workflow: &Workflow, file: &str) -> Self {
        if let Some(error) = label_error(file) {
            return error;
        }
        let diagnostics = validate(workflow, file);
        if !diagnostics.is_empty() {
            return Self {
                valid: false,
                digest: None,
                diagnostics,
            }
            .bounded();
        }
        match workflow.digest() {
            Ok(digest) => Self {
                valid: true,
                digest: Some(digest),
                diagnostics,
            },
            Err(_) => Self::rejected(Diagnostic {
                code: "serialization_error".into(),
                file: file.into(),
                path: "$".into(),
                node: None,
                edge: None,
                message: "validated definition could not be serialized".into(),
            }),
        }
    }
}

/// JSON/YAML parsing, static checking and canonical identity are identical at
/// local and authenticated remote boundaries. This does not resolve a runtime
/// bundle, publish a version, start a run or grant execution authority.
pub fn validate_source(source: &str, format: Format, file: &str) -> ValidationReport {
    if let Some(error) = label_error(file) {
        return error;
    }
    match workflow_ir::parse(source, format, file) {
        Ok(workflow) => ValidationReport::check(&workflow, file),
        Err(diagnostic) => ValidationReport::rejected(*diagnostic),
    }
}
