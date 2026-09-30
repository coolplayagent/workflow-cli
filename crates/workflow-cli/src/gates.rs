use crate::write;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::io::{Read, Write};
use workflow_gates::*;
use workflow_worker::Clock;
mod source;

pub const HELP: &str = "EVIDENCE CHECKS\n  workflow gate review-digest <request.json>\n  workflow gate evaluate <run-db> <artifact-store> <request.json>\n  workflow gate revalidate <run-db> <artifact-store> <request.json> <decision.json>\n  workflow schema <gate-request|gate-decision>\n\nReads verified execution history and artifact bytes; does not change a run or authorize an effect.\nOnly PASS exits 0. FAIL, UNKNOWN and rejected evidence exit 1; usage/I/O exit 2.\nUse a host-controlled policy and freshly observed target. Time comes from the host clock.\n";
fn read<T: DeserializeOwned>(path: &str) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(path)
        .and_then(|f| {
            f.take(workflow_artifacts::MAX_JSON_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| Error::new(ErrorCode::InvalidDocument, format!("I/O: {path}: {e}")))?;
    parse_message(&bytes)
}
fn execute(args: &[&str]) -> Result<(serde_json::Value, i32)> {
    match args {
        ["gate", "review-digest", request] => {
            let request: Request = read(request)?;
            Ok((
                json!({"review_digest":review_digest(&request.policy,&request.target)?}),
                0,
            ))
        }
        ["schema", kind @ ("gate-request" | "gate-decision")] => {
            Ok((parse_message(schema(&kind[5..])?.as_bytes())?, 0))
        }
        [
            "gate",
            mode @ ("evaluate" | "revalidate"),
            db,
            artifacts,
            request,
            rest @ ..,
        ] if (*mode == "evaluate" && rest.is_empty())
            || (*mode == "revalidate" && rest.len() == 1) =>
        {
            let request: Request = read(request)?;
            validate(&request)?;
            let source = source::LocalEvidence::open(db, artifacts, &request.target.run_id)?;
            // Sample after recovery so its cost cannot extend the freshness window.
            let now = workflow_worker::SystemClock
                .now_unix_ms()
                .map_err(|e| Error::new(ErrorCode::InvalidContract, e.message))?;
            let decision = if *mode == "evaluate" {
                evaluate(&request, &source, now)?
            } else {
                revalidate(&request, &read(rest[0])?, &source, now)?
            };
            let exit = if decision.verdict == Verdict::Pass {
                0
            } else {
                1
            };
            Ok((json!(decision), exit))
        }
        _ => Err(Error::new(ErrorCode::InvalidDocument, "usage")),
    }
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    match execute(args) {
        Ok((value, code)) => match to_message(&json!({"ok":true,"result":value})) {
            Ok(bytes) => write(stdout, std::str::from_utf8(&bytes).expect("JSON"), code),
            Err(e) => write(stdout, &json!({"ok":false,"error":e}).to_string(), 1),
        },
        Err(e) if e.message == "usage" => write(stderr, HELP, 2),
        Err(e) => write(
            stdout,
            &json!({"ok":false,"error":e}).to_string(),
            if e.message.starts_with("I/O:") { 2 } else { 1 },
        ),
    }
}
#[cfg(test)]
mod tests;
