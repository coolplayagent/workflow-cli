use crate::write;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::{Read, Write};
use workflow_kernel::*;

pub const HELP: &str = "DETERMINISTIC KERNEL\n  workflow kernel check <bundle.json>\n  workflow kernel replay <scenario.json>\n  workflow kernel restore <bundle.json> <checkpoint.json>\n  workflow kernel apply <bundle.json> <checkpoint.json> <event.json>\n  workflow schema <kernel-bundle|kernel-event|kernel-scenario|kernel-checkpoint|kernel-signal>\n\nReplay/apply only calculate state and commands; they never dispatch a task or persist a run.\nEvent results are trusted host facts, not an untrusted worker ingress.\nWrite returned checkpoints to a new path; output redirection must not overwrite inputs.\nExit 0: command evaluated (inspect snapshot.status); 1: rejected input/transition; 2: usage/I/O.\n";
fn read<T: DeserializeOwned>(file: &str) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(file)
        .and_then(|f| {
            f.take((workflow_worker::MAX_MESSAGE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| Error::new(ErrorCode::InvalidRequest, format!("I/O {file}: {e}")))?;
    workflow_worker::parse_message(&bytes).map_err(Into::into)
}
pub fn run(args: &[&str], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    match execute(args) {
        Ok(value) => match workflow_worker::to_message(&value) {
            Ok(bytes) => write(
                stdout,
                std::str::from_utf8(&bytes).expect("JSON is UTF-8"),
                0,
            ),
            Err(e) => write(stdout, &json!({"ok":false,"error":e}).to_string(), 1),
        },
        Err(e) if e.message == "usage" => write(stderr, HELP, 2),
        Err(e) => {
            let code = if e.message.starts_with("I/O ") { 2 } else { 1 };
            write(stdout, &json!({"ok":false,"error":e}).to_string(), code)
        }
    }
}
fn execute(args: &[&str]) -> Result<Value> {
    match args {
        ["schema", kind] if kind.starts_with("kernel-") => {
            workflow_worker::parse_message(schema(&kind[7..])?.as_bytes()).map_err(Into::into)
        }
        ["kernel", "check", file] => {
            let bundle = CompiledBundle::compile(read(file)?)?;
            Ok(
                json!({"ok":true,"bundle_digest":bundle.digest(),"workflows":bundle.spec().workflows.iter().map(|w|json!({"id":w.id,"version":w.version,"digest":w.digest().expect("compiled workflow serializes")})).collect::<Vec<_>>()}),
            )
        }
        ["kernel", "replay", file] => {
            let scenario: Scenario = read(file)?;
            let (mut engine, first) = Engine::start(
                CompiledBundle::compile(scenario.bundle)?,
                &scenario.run_id,
                scenario.inputs,
                scenario.started_at_unix_ms,
                scenario.limits,
            )?;
            let mut transitions = vec![first];
            for event in scenario.events {
                transitions.push(engine.apply(event)?);
            }
            report(&engine, transitions)
        }
        ["kernel", "restore", bundle, checkpoint] => {
            let engine =
                Engine::restore(CompiledBundle::compile(read(bundle)?)?, read(checkpoint)?)?;
            report(&engine, vec![])
        }
        ["kernel", "apply", bundle, checkpoint, event] => {
            let mut engine =
                Engine::restore(CompiledBundle::compile(read(bundle)?)?, read(checkpoint)?)?;
            let transition = engine.apply(read(event)?)?;
            report(&engine, vec![transition])
        }
        _ => Err(Error::new(ErrorCode::InvalidRequest, "usage")),
    }
}
fn report(engine: &Engine, transitions: Vec<Transition>) -> Result<Value> {
    Ok(
        json!({"ok":true,"snapshot":engine.snapshot(),"transitions":transitions,"checkpoint":engine.checkpoint()?}),
    )
}

#[cfg(test)]
mod tests;
