//! CLI application layer, also callable by tests and future service adapters.
use std::io::{Read, Write};
use workflow_ir::{Diagnostic, Format, MAX_DOCUMENT_BYTES, Workflow};
use workflow_validator::ValidationReport as Report;

mod artifacts;
mod backups;
mod daemon;
mod gates;
mod kernel;
mod models;
mod registry;
mod remote;
mod runs;
mod worker;
mod workspaces;

const HELP: &str = "workflow — portable SOP definition compiler\n\nUSAGE\n  workflow validate <file.json|file.yaml>\n  workflow export <file.json|file.yaml> <json|yaml>\n  workflow schema\n  workflow help\n\nvalidate emits JSON with valid, digest and diagnostics.\nExit codes: 0 success, 1 invalid definition, 2 usage or I/O error.\nRelative files resolve against the caller's current directory.\n";

fn load_source(path: &str) -> Result<(String, Format), Box<Diagnostic>> {
    let fail = |code: &str, message: String| {
        Box::new(Diagnostic {
            code: code.into(),
            file: path.into(),
            path: "$".into(),
            node: None,
            edge: None,
            message,
        })
    };
    let format = match std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
    {
        Some("json") => Format::Json,
        Some("yaml" | "yml") => Format::Yaml,
        _ => {
            return Err(fail(
                "format_error",
                "file extension must be .json, .yaml or .yml".into(),
            ));
        }
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|e| fail("io_error", e.to_string()))?;
    if !file
        .metadata()
        .map_err(|e| fail("io_error", e.to_string()))?
        .is_file()
    {
        return Err(fail("io_error", "input must be a regular file".into()));
    }
    let mut bytes = vec![];
    file.take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| fail("io_error", e.to_string()))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(fail(
            "document_too_large",
            "definition exceeds 1 MiB".into(),
        ));
    }
    let input =
        String::from_utf8(bytes).map_err(|_| fail("io_error", "input must be UTF-8".into()))?;
    Ok((input, format))
}

fn load(path: &str) -> Result<Workflow, Box<Diagnostic>> {
    let (source, format) = load_source(path)?;
    workflow_ir::parse(&source, format, path)
}
fn validation_output(report: &Report, stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let code = if report
        .diagnostics
        .iter()
        .any(|d| d.code == "io_error" || d.code == "format_error")
    {
        2
    } else if report.valid {
        0
    } else {
        1
    };
    match serde_json::to_string_pretty(report) {
        Ok(json) => write(stdout, &json, code),
        Err(_) => write(stderr, "validation report serialization failed", 2),
    }
}

/// No process exits or global output redirection inside the reusable application layer.
pub fn run(
    args: impl IntoIterator<Item = String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let args: Vec<_> = args.into_iter().collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["help" | "--help" | "-h"] => write(
            stdout,
            &format!(
                "{HELP}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
                registry::HELP,
                worker::HELP,
                kernel::HELP,
                runs::HELP,
                artifacts::HELP,
                gates::HELP,
                workspaces::HELP,
                models::HELP,
                backups::HELP,
                daemon::HELP,
                remote::HELP
            ),
            0,
        ),
        args @ ["service" | "remote", ..] => remote::run(args, stdout, stderr),
        args @ ["daemon", ..] | args @ ["schema", "daemon-config"] => {
            daemon::run(args, stdout, stderr)
        }
        args @ ["backup", ..]
        | args @ [
            "schema",
            "backup-index" | "backup-sources" | "backup-restore-request",
        ] => backups::run(args, stdout, stderr),
        args @ ["model", ..]
        | args @ [
            "schema",
            "model-policy" | "model-proposal" | "model-record" | "model-http-binding",
        ] => models::run(args, stdout, stderr),
        args @ ["workspace", ..]
        | args @ [
            "schema",
            "workspace-checkout" | "workspace-ref" | "workspace-observation" | "workspace-output",
        ] => workspaces::run(args, stdout, stderr),
        args @ ["gate", ..] | args @ ["schema", "gate-request" | "gate-decision"] => {
            gates::run(args, stdout, stderr)
        }
        args @ ["artifact", ..]
        | args @ [
            "schema",
            "artifact-publish" | "artifact-ref" | "artifact-type",
        ] => artifacts::run(args, stdout, stderr),
        args @ ["run", ..]
        | args @ [
            "schema",
            "run-start"
            | "run-receipt"
            | "run-lease"
            | "run-execution-record"
            | "run-signal"
            | "run-effect-http-binding"
            | "run-effect-attempt"
            | "run-effect-reply"
            | "run-effect-observation"
            | "run-effect-resolution"
            | "run-recovery-acknowledgement"
            | "run-restored-effect",
        ] => runs::run(args, stdout, stderr),
        args @ ["kernel", ..]
        | args @ [
            "schema",
            "kernel-bundle" | "kernel-event" | "kernel-scenario" | "kernel-checkpoint"
            | "kernel-signal",
        ] => kernel::run(args, stdout, stderr),
        args @ ["capability" | "worker", ..]
        | args @ ["schema", "capability" | "request" | "grant" | "result"] => {
            worker::run(args, stdout, stderr)
        }
        args @ ["draft" | "release" | "diff", ..] | args @ ["schema", "patch"] => {
            registry::run(args, stdout, stderr)
        }
        ["schema"] => match workflow_ir::schema() {
            Ok(schema) => write(stdout, &schema, 0),
            Err(e) => write(stderr, &e.to_string(), 2),
        },
        ["validate", path] => {
            let report = match load_source(path) {
                Ok((source, format)) => workflow_validator::validate_source(&source, format, path),
                Err(diagnostic) => Report::rejected(*diagnostic),
            };
            validation_output(&report, stdout, stderr)
        }
        ["export", path, format @ ("json" | "yaml")] => {
            let (workflow, diagnostics) = compile(path);
            let Some(workflow) = workflow else {
                let code = if diagnostics
                    .iter()
                    .any(|d| d.code == "io_error" || d.code == "format_error")
                {
                    2
                } else {
                    1
                };
                return match serde_json::to_string_pretty(&Report {
                    valid: false,
                    digest: None,
                    diagnostics,
                }) {
                    Ok(json) => write(stderr, &json, code),
                    Err(e) => write(stderr, &e.to_string(), 2),
                };
            };
            let output = if *format == "json" {
                workflow.canonical_json().map_err(|e| e.to_string())
            } else {
                workflow.to_yaml().map_err(|e| e.to_string())
            };
            match output {
                Ok(output) => write(stdout, &output, 0),
                Err(error) => write(stderr, &error, 2),
            }
        }
        _ => write(stderr, HELP, 2),
    }
}

fn compile(path: &str) -> (Option<Workflow>, Vec<Diagnostic>) {
    match load(path) {
        Err(diagnostic) => (None, vec![*diagnostic]),
        Ok(workflow) => {
            let diagnostics = workflow_validator::validate(&workflow, path);
            if diagnostics.is_empty() {
                (Some(workflow), diagnostics)
            } else {
                (None, diagnostics)
            }
        }
    }
}
fn write(output: &mut impl Write, text: &str, code: i32) -> i32 {
    let result = output.write_all(text.as_bytes()).and_then(|()| {
        if text.ends_with('\n') {
            Ok(())
        } else {
            output.write_all(b"\n")
        }
    });
    if result.is_err() { 2 } else { code }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_and_missing_files_have_stable_exit_codes() {
        let mut out = vec![];
        let mut err = vec![];
        assert_eq!(run(["unknown".into()], &mut out, &mut err), 2);
        assert!(out.is_empty());
        assert_eq!(
            run(
                ["validate".into(), "nonexistent-workflow.json".into()],
                &mut out,
                &mut err
            ),
            2
        );
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(report["valid"], false);
        assert_eq!(report["diagnostics"][0]["code"], "io_error");
        assert!(report["digest"].is_null());
    }
    #[test]
    fn schema_is_machine_readable() {
        let mut out = vec![];
        assert_eq!(run(["schema".into()], &mut out, &mut vec![]), 0);
        let schema: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(schema["title"], "Workflow");
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(path: &str) -> PathBuf {
        if let Ok(runfiles) = std::env::var("TEST_SRCDIR") {
            return PathBuf::from(runfiles)
                .join(std::env::var("TEST_WORKSPACE").unwrap())
                .join(path);
        }
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }

    #[test]
    fn published_examples_compile_and_export_without_changing_identity() {
        for name in [
            "review.json",
            "review.yaml",
            "parallel-tests.json",
            "bounded-repair.json",
            "repair-round.json",
        ] {
            let path = fixture(&format!("examples/{name}"))
                .to_string_lossy()
                .into_owned();
            let mut out = vec![];
            let mut err = vec![];
            assert_eq!(
                run(["validate".into(), path.clone()], &mut out, &mut err),
                0,
                "{name}: {}",
                String::from_utf8_lossy(&out)
            );
            assert!(err.is_empty());
            let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(report["valid"], true);
            let expected = report["digest"].as_str().unwrap();
            for format in ["json", "yaml"] {
                out.clear();
                assert_eq!(
                    run(
                        ["export".into(), path.clone(), format.into()],
                        &mut out,
                        &mut err
                    ),
                    0
                );
                let restored = workflow_ir::parse(
                    std::str::from_utf8(&out).unwrap(),
                    if format == "json" {
                        Format::Json
                    } else {
                        Format::Yaml
                    },
                    "export",
                )
                .unwrap();
                assert_eq!(restored.digest().unwrap(), expected);
            }
        }
    }

    #[test]
    fn compiler_file_admission_keeps_size_and_fifo_failures_explicit() {
        let dir = std::env::temp_dir().join(format!(
            "workflow-file-admission-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let large = dir.join("large.json");
        // Capacity is diagnosed before decoding a possibly truncated UTF-8 unit.
        std::fs::write(&large, vec![0xe9; MAX_DOCUMENT_BYTES + 32]).unwrap();
        for prefix in [
            vec!["validate".to_string()],
            vec![
                "remote".into(),
                "validate".into(),
                "unused-binding.json".into(),
            ],
        ] {
            let mut args = prefix;
            args.push(large.display().to_string());
            let mut out = vec![];
            let mut err = vec![];
            assert_eq!(run(args, &mut out, &mut err), 1);
            let report: Report = serde_json::from_slice(&out).unwrap();
            assert!(!report.valid);
            assert!(report.digest.is_none());
            assert_eq!(report.diagnostics[0].code, "document_too_large");
        }
        #[cfg(unix)]
        {
            let fifo = dir.join("pipe.json");
            let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: this fixture owns a valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            for prefix in [
                vec!["validate".to_string()],
                vec![
                    "remote".into(),
                    "validate".into(),
                    "unused-binding.json".into(),
                ],
            ] {
                let mut args = prefix;
                args.push(fifo.display().to_string());
                let mut out = vec![];
                let mut err = vec![];
                assert_eq!(run(args, &mut out, &mut err), 2);
                let report: Report = serde_json::from_slice(&out).unwrap();
                assert_eq!(report.diagnostics[0].code, "io_error");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn committed_schema_matches_the_compiler() {
        let committed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture("schemas/workflow-v1.schema.json")).unwrap(),
        )
        .unwrap();
        let generated: serde_json::Value =
            serde_json::from_str(&workflow_ir::schema().unwrap()).unwrap();
        assert_eq!(committed, generated);
    }

    #[test]
    fn cli_rejects_a_definition_without_emitting_a_success_digest_or_export() {
        let path =
            std::env::temp_dir().join(format!("workflow-invalid-{}.json", std::process::id()));
        // A structurally well-formed graph with a dangling endpoint exercises semantic validation.
        let mut w = load(fixture("examples/review.json").to_str().unwrap()).unwrap();
        w.edges[0].to = "missing".into();
        std::fs::write(&path, w.canonical_json().unwrap()).unwrap();
        let mut out = vec![];
        let mut err = vec![];
        assert_eq!(
            run(
                ["validate".into(), path.to_string_lossy().into_owned()],
                &mut out,
                &mut err
            ),
            1
        );
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(report["valid"], false);
        assert!(report["digest"].is_null());
        out.clear();
        assert_eq!(
            run(
                [
                    "export".into(),
                    path.to_string_lossy().into_owned(),
                    "yaml".into()
                ],
                &mut out,
                &mut err
            ),
            1
        );
        assert!(out.is_empty());
        std::fs::remove_file(path).unwrap();
    }
}
