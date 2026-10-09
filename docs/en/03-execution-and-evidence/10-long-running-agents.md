# Long-running agent sessions

Version 0.2.0 separates task duration from ownership, adds durable model/tool
sessions and typed loop feedback, and provides explicit run continuation. The
local and shared stores keep the existing approval, artifact, effect and result
proofs. A model never acquires permission to change workflow control state.

## Upgrade and recovery

SQLite storage and PostgreSQL run images now use schema **12**. Stop old writers,
retain the old executable, and use the existing reviewed storage upgrade and
backup procedures. Local schemas 1–11 and shared images 10–11 require explicit
upgrade. Normal open never silently migrates a database. An old executable must
not open schema 12. Upgrade shared services and workers together and update the
scheduling policy's exact worker-version allowlist before admitting new tasks.

New runs save state checkpoints at revision 1 and every 16 revisions. Each contains
the snapshot, event/outbox prefix digests and the previous checkpoint digest.
Ordinary recovery verifies stored identities, chains, receipts and execution
proofs, then applies at most 15 events after the latest checkpoint. It still scans
retained history; it is not constant-time. `run verify` independently replays the
full history once and compares checkpoints and commands. Upgrades retain legacy
journal checkpoints and add a state checkpoint at the verified head.

```sh
workflow run storage-plan runs.db
workflow run migrate runs.db retained-before-v12.db
workflow run history-usage runs.db segment-1
workflow run verify runs.db segment-1
```

`history-usage` reports events, frames, checkpoint bytes, configured ceilings,
checkpoint revision and replayed tail. At 80% of an event/frame/byte ceiling it
recommends continuation. Limits are unchanged. PostgreSQL still stores a bounded
64 MiB transaction image; checkpoints reduce replay work, not image write volume.
Design finite workflow segments instead of increasing limits indefinitely.

## Activities and scheduling

A prepared read-only task's deadline comes from its frozen capability timeout.
The ownership lease can expire earlier and is renewed while the activity runs.
Renewal preserves the epoch and the absolute task deadline. Progress records are
visible in execution history; stale owners cannot append progress or results.
Legacy prepared records retain their original lease-clamped deadline semantics.

CLI builtin, workspace and model tasks run in private subprocesses. The parent
renews ownership and checks cancellation while the child works. Cancellation,
deadline expiry or loss of ownership stops and reaps the process group. Linux
parent-death handling terminates the direct activity child after a driver crash.
This does not undo external effects. Custom synchronous Rust adapters remain
cooperative; the library cannot forcibly stop arbitrary code.

The local daemon admits up to eight independent runs, one command per drive.
A slow model activity does not block another run's timer. `active_runs` lists
admitted runs; `active_run` retains the first for compatibility. Stop prevents new
admission and drains every admitted drive. `lease_ms` in daemon configuration is
100–300000 ms, default 120000; choose enough margin for database admission latency.
Shared schedulers renew assignment expiry and capacity reservations together,
bounded by the original task deadline and worker credential expiry. Shared CLI
workers probe assignment authority during child execution and persist progress.
Effects keep their existing frozen call deadlines and reconciliation rules.

## Frozen retries and durable model sessions

Policies without `retry` retain their old canonical digest and failure behavior.
A new immutable policy version can declare:

```json
"retry": {
  "max_retries": 2,
  "initial_backoff_ms": 500,
  "max_backoff_ms": 30000
}
```

`max_retries` must be positive and below `budget.model_calls`. The task must also
declare permanent final outcomes `model_rate_limited` and `model_authentication`.
Every admitted call, including a call whose result became unknown, consumes the
original model-call budget. Backoff and its next eligible time are derived from
durable observations. Restart never resets the session budget or original deadline.

With retries enabled, HTTP 429 is a rate limit; 408/5xx and transport failures are
temporary; 401/403 are authentication failures. Refusal and malformed output are
permanent. `Retry-After` supports seconds and HTTP dates. The next attempt waits
for both exponential backoff and the provider's delay. A delay beyond the frozen
backoff ceiling or session deadline stops the session instead of retrying early.
The HTTP client itself does not retry or follow redirects.

Before each model/tool call the host durably records admission; after the call it
acknowledges the observation before admitting subsequent work. Checkpoints bind
exact inputs, original request, policy digest and provider binding. Writes require
live ownership and an exact previous-checkpoint digest. New admissions are refused
after workflow cancellation. Acknowledged tools are reused after restart. An
unacknowledged model call is recorded as `uncertain` and counts against the budget;
an unacknowledged tool has an explicit uncertain observation. No result is invented.

A resumed final model record uses schema 2 and includes its original
`source_request`; old schema-1 records still verify. Offline verification alone is
not authorization: durable completion also requires the recorded checkpoint prefix.
Provider usage can remain unknown after a crash. No exact billing or atomic
monetary budget is claimed. Inspect `schema model-checkpoint`, `schema model-record`
and `run execution-history` for the wire contracts.

## Feedback and successor handoff

A loop can declare `"feedback":{"diagnostic":"diagnostic"}`. Keys name next
iteration inputs; values name required inputs of every failed body terminal.
The bundle compiler checks types. A failed iteration copies the actual terminal
values into the next body's inputs; missing/conflicting diagnostics fail the loop.
Empty feedback retains the old loop behavior and definition digest. Existing
iteration and absolute loop deadlines remain unchanged.

A continuation plan contains a version-1 handoff with source run/digest/revision,
objective, plan, verified facts, artifact links, failures and remaining work.
Each verified fact names an instance, output field and exact observed value;
artifacts must already be acknowledged by source execution and still verify.
The successor must declare a typed input containing that exact handoff.

```sh
workflow schema run-handoff
workflow schema run-continuation
workflow run continue runs.db source-lease.json reviewed-continuation.json
workflow run continuation runs.db segment-1
```

Continuation requires the exact successful source revision, settled outbox and
effects, and a current lease. It reserves one immutable successor plan in source
execution history, then starts that successor in a second idempotent commit. A
crash between commits leaves a recoverable reservation: repeat the same command.
A different successor conflicts; a pre-existing destination with different start
content also conflicts. Keep the recorded plan and both runs. Successor history
starts at revision 1. Pending gates, approvals and unknown effects cannot be
carried past this successful segment boundary. This is explicit segmentation,
not automatic truncation of a running workflow.

## Reproduce the failure-path checks

```sh
python3 examples/long-running/acceptance.py target/debug/workflow
python3 examples/long-running/history-benchmark.py target/release/workflow --output history.json
```

The fixture sends a real 503, kills a real daemon after a tool acknowledgement,
and cancels a blocked activity. It checks retry timing, one acknowledged tool
execution, uncertain-call accounting, child termination and offline verification.
It uses a deterministic loopback provider, not production model quality evidence.
Kernel tests prove that the next repair receives the previous diagnostic; shared
integration tests exercise assignment renewal, scoped checkpoint access and
PostgreSQL recovery. The delivery milestones are recorded by
[`long-running-p1.json`](../../../examples/maintenance/long-running-p1.json).

<!-- book-navigation -->

3.10 Long-running agent sessions

[Book contents](../README.md) · [3. Execute and verify](README.md) · [中文](../../zh/03-execution-and-evidence/10-long-running-agents.md) · [Previous: 3.9 Protected delivery](09-release-acceptance.md) · [Next: 4.1 Events and human decisions](../04-effects-and-recovery/01-event-inbox.md)

<!-- /book-navigation -->
