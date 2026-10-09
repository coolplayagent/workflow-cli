# Local read-only execution

`workflow-runtime` drives the kernel's durable outbox through `ExecutionStore`
and the checked `Worker`. It depends on their ports, with no SQLite or provider
SDK in its production dependencies. `workflow-runstore-sqlite` implements the
transactional execution port; the CLI selects the built-in compiler adapters.
Each module has its own Bazel `rust_library`.

## Execute a real contract

```sh
cargo run --locked -- run init /tmp/inspect-runs.db
cargo run --locked -- run start /tmp/inspect-runs.db examples/execution/valid-start.json
cargo run --locked -- run drive /tmp/inspect-runs.db inspect-valid local-agent 10
cargo run --locked -- run status /tmp/inspect-runs.db inspect-valid
cargo run --locked -- run execution-history /tmp/inspect-runs.db inspect-valid 0 100
cargo run --locked -- run verify /tmp/inspect-runs.db inspect-valid
cargo run --locked -- run drive /tmp/inspect-runs.db inspect-valid local-agent 10
```

This calls `workflow.validate` on the actual embedded definition text, validates
its result and lets the SOP decision select the declared business terminal. The
first drive executes one task; the second executes zero. The adapter is local,
read-only and requires no model or network service. `invalid-start.json` exercises
the real invalid-definition branch: use run ID `inspect-invalid`. Its inspection
succeeds with `valid=false`, and the workflow ends `failed`. CLI exit 0 means the
drive operation completed; always inspect `result.snapshot.status`.

Start files bind exact workflows and capability descriptors. Obtain descriptors
with `capability describe <id> <version>`. A matching name alone is insufficient:
contract digests must match the registered adapter. Example start time 1000 is
safe here because these definitions start with an immediate task. For real waits
or loops, use the actual start time or their deadlines may already be due.

`run drive <db> <id> <owner> <max-commands>` processes 1–100 ordered commands and
returns the snapshot, task/command/timer counts and stop reason `idle` or `budget`.
The CLI creates a fresh acquisition ID and uses a 120-second lease. `idle` can
mean a waiting run; it does not mean success. No background process remains.
A subsequent explicit drive advances due wait/loop deadlines and resumes pending
work. For unattended polling and live service status, use the optional
[local daemon](03-local-daemon.md). `run execution-history` uses an exclusive
sequence cursor starting at 0, page size 1–100, and `next_cursor`.

## Ownership and invocation sequence

1. Acquire the run's lease in an immediate SQLite transaction. There is one owner
   per run, a unique acquisition ID and a monotonically increasing epoch.
2. Verify all run and execution history, select the first unacknowledged command,
   and persist the exact request and grant before returning an attempt to invoke.
3. Call the registered adapter outside the database transaction. The request binds
   run, node instance, attempt, epoch, definition, capability contract and inputs.
4. Under a new immediate transaction, verify live ownership, request identity,
   deadline, output/error contract and current node eligibility. Commit the worker
   result, kernel event, state/checkpoint/successor outbox, original command receipt
   and execution journal together. Acknowledge only after commit.
5. Release the lease on normal completion or a reported execution error. A crash
   leaves ownership until expiry. A new owner then receives a higher epoch.

The trusted host clock is sampled after the transaction lock and recovery checks,
and again before commit admission. Zero/backward time rejects; lease expiry is
exclusive (`now >= expires_at` rejects). A queued caller cannot submit a timestamp
sampled before waiting for the write lock. All admitted mutations are serialized
by SQLite. Renewals extend a bounded lease and invalidate the old lease token;
they preserve the epoch and do not extend an already prepared request's deadline.
Leases are at most five minutes and are renewed during CLI activity execution. The frozen capability timeout determines the task deadline independently of the lease. CLI builtin, workspace and model work runs in a cancellable child process; ownership loss, cancellation and expiry stop the child. Custom synchronous Rust adapters must still cooperate with deadlines; no process cancellation undoes effects. See [long-running execution](10-long-running-agents.md). This is a trusted local clock/process/storage boundary,
not a distributed clock or remote authentication protocol.

An old owner cannot finish an unsettled attempt after release, expiry or takeover.
An exact result retry after a successful commit returns a duplicate without new
commands, even after lease expiry. A changed result for that attempt conflicts.
This read of an already committed fact does not grant new ownership.

## Failure and retry behavior

| Observation | Behavior |
| --- | --- |
| Missing adapter or rejected worker protocol | Preserve the worker error as an attempt observation; leave the task and command pending. Drive exits 1 and releases when possible. |
| Crash after request persistence, before result commit | Preserve the prepared attempt. After takeover, a new read-only attempt may run; the old epoch cannot commit. |
| Crash during result transaction | Recover the old complete state or the new complete result/event/receipt; never a partial success. |
| Crash after result commit, before reply | Reopen and inspect execution history; the completed command is not invoked again. |
| Expiry or clock reversal before commit admission | Roll back the result, event, receipt and execution journal together. |
| Cancellation before invocation | Settle unstarted read-only task cancellation and acknowledge its intent without invoking it. |
| Cancellation while invocation is in progress | Retain a validated late result, while kernel cancellation determines business status. |
| Uncertain effect/reconciliation command | Stop for a verified host reconciliation result; do not fabricate settlement. |
| Nonempty artifact evidence | Require the configured ArtifactReader to verify manifest, bytes, types, ancestors and exact producer/request/input identity; missing or corrupt dependencies reject. |

Retries are bounded to three prepared attempts per command, including orphans and
failed invocations. Drive stops on a worker/protocol error; another explicit drive
may retry read-only work under a new acquisition. There is no retry-budget reset,
automatic backoff or write-effect retry. The execution journal is bounded at 10,000
records per run; acquisitions, renewals, releases and observations all count.
Kernel budgets also apply. Exhaustion is an error, not silently dropped history.

Storage/worker/release errors remain separate fields in the CLI error envelope.
A release error can follow an already committed result. Inspect status and history
before retrying; never infer rollback solely from exit 1 or a lost reply. Acquisition
IDs are single-use, not idempotent request IDs. If acquiring a lease had an ambiguous
reply, inspect execution history and wait for expiry before obtaining a fresh
lease. Keep the original request/result identities when investigating a task.

Only capabilities declaring `read_only` are accepted by this executor. It trusts
the registered adapter to honor that declaration. At-least-once invocation after
an orphan is possible; only accepted result settlement is deduplicated. This does
not implement exactly-once external effects, tenant isolation or authorization.

## Durable control intents and recovery

Wait/deadline registration is the persisted kernel snapshot. The local driver
acknowledges those intents and scans that snapshot on each drive. It does not claim
that an OS timer or external service was installed. Cancellation preserves intent
order, so an execute/cancel pair cannot be reordered into a new invocation.

Every normal store read and mutation replays the execution journal, validates its
separate count/digest chain, reconstructs lease/attempt authority, regenerates
prepared requests from immutable commands, and checks finished results against
the committed events and receipts. Missing tails, stale ownership and conflicting
bindings fail verification. Database-owner tampering that rewrites all facts and
hashes is outside this integrity boundary. Recovery performs no adapter calls.

`run event` and `run acknowledge` remain trusted administrative host interfaces.
They do not acquire an execution lease or authenticate external observations. Do
not expose them to untrusted workers or manually acknowledge managed task commands
to bypass result checks. An external service must route worker results through the
fenced execution port and authenticate its callers. Human signals/cancellation
can race execution through the existing kernel revision checks.

## Durable pause and resume

Use the current revision from `run status` and a stable control identity:

```sh
workflow run pause runs.db run-id pause-1 7 1790559000000 'maintenance'
workflow run status runs.db run-id
workflow run resume runs.db run-id resume-1 8 1790559060000 'maintenance finished'
```

`pause` and `resume` commit kernel events and the snapshot in one transaction.
The nonempty reason is at most 1024 bytes; do not place secrets in this audit
field. `status` and `list` expose `pause.reason` and `pause.at_unix_ms` while paused;
`status` remains `running`. Duplicate identical events return the current snapshot
without repeating the control. A changed identity payload, stale revision, second
pause, resume without a pause, or control after cancellation/termination is refused.
On a lost response retry the exact ID, revision, time and reason.

A pause stops new task and gate claims, timer advancement and automatic successor
activation. `drive` releases its lease and returns `stop_reason: paused`, with no
new capability/model calls. Existing prepared work can still execute and settle
under its original lease and deadline. A pause is therefore an admission boundary,
not a process suspension or rollback. Settled results and uncertainty remain in
history; they cannot trigger new downstream work until resume. Pending commands
and instance IDs are retained; completed work is never reissued by resume.

Time stays absolute: pause does not extend wait, loop, lease, task or gate-evidence
deadlines. Resume observes expired wait/loop deadlines before driving successors.
A final task result may leave a run `running` with a pause until resume reduces the
remaining control flow. Cancellation remains available while paused, clears the
pause and performs the usual cancellation/reconciliation flow. Raw signal, gate,
retry-gate and time events are refused while paused. The [durable Inbox](../04-effects-and-recovery/01-event-inbox.md) buffers trusted callbacks during pauses;
authenticated approval ingestion is described in [R06 acceptance](../06-acceptance-and-maintenance/03-approval-acceptance.md).

These controls use the trusted administrative host boundary described above;
caller-supplied reasons are audit context, not authenticated actor identities.
The tests cover checkpoint replay, result draining, no paused invocations, expired
waits, stale resume conflicts and killed pause writers at five commit phases.

## Explicit storage migration

New databases use storage schema 12. Schemas 1–11 require an explicit verified
backup and upgrade; ordinary create/open never migrates a store:

```sh
cargo run --locked -- run --artifacts /path/to/artifacts storage-plan /path/to/existing-runs.db
cargo run --locked -- run --artifacts /path/to/artifacts migrate /path/to/existing-runs.db /path/to/new-before-upgrade.sqlite
```

The source digest is compared inside the upgrade transaction. All retained history,
execution proofs and artifact dependencies must verify before version/journal
commit. Failure keeps the old schema. See [version migration](../04-effects-and-recovery/05-version-migration.md)
for source-bound plans, storage journals, verified rollback to a retained binary,
shared image conversion and the separate explicit definition-migration operation.
Logical StartRun remains schema 1; direct worker calls retain protocol 1 and model
policy calls require protocol 2. A storage upgrade never restarts a definition or
undoes an external business effect.

## Evidence and remaining scope

Tests include two independent processes racing for a lease, fencing after takeover,
renewal/release/time boundaries, forged results, bounded retries, real built-in
business decisions, no invocation after storage rejection, and repeat drives after
commit. OS-process termination is forced before the result transaction, after
event/state/execution writes, and before/after commit. Reopening recovers the entire
transaction and retries only an orphan. Migration preserves v1 runs and rolls back
on corruption. Cargo and Bazel run the same tests.

The [remote service](../05-shared-execution/03-remote-service.md), [workspace acceptance](../06-acceptance-and-maintenance/04-artifact-acceptance.md),
[cluster scheduling](../05-shared-execution/06-cluster-scheduling.md) and [security acceptance](../06-acceptance-and-maintenance/06-security-acceptance.md)
chapters document the separate shared, workspace and authority contracts. These
local Linux process-crash checks establish no production throughput, power-loss,
shared network filesystem, business-benefit or RPO/RTO claim.

For a host-managed worker/report flow use `run acquire/claim/finish/release`. Runs
with evidence require `run --artifacts <store>` on subsequent reads and mutations.
The artifact location is host configuration, and missing dependencies prevent a
healthy recovery response. The built-in `run drive` does not automatically fabricate
reports; it verifies evidence supplied by registered adapters through its store.

## Mandatory postconditions

`claim` and `drive` consume frozen gate commands under the current lease. Task
settlement can leave the run awaiting a gate; inspect business state and decisions.
A gate PASS commits its transition, receipt and proof atomically; UNKNOWN consumes
one intent and waits for explicit `retry-gate`. See [runtime postconditions](08-runtime-postconditions.md)
for bindings, evidence selection, repair bounds and protected event ingress.

## Managed write effects

The separate `EffectStore` port and `drive_with_effects` host driver persist write
intents and settle provider observations under the same run lease. CLI hosts use
`run drive-effects`; ordinary `drive` continues to use read-only workers. See
[durable effects](../04-effects-and-recovery/02-durable-effects.md) for stable keys, query recovery, bounded
retry, manual reconciliation and provider fencing limits. Storage schema 8 adds
these policies and journal records; schema 9 adds [ordered compensation](../04-effects-and-recovery/03-ordered-compensation.md).
Explicit migration to schema 12 accepts schemas 1–11; restored stores use the
[recovery barrier](../04-effects-and-recovery/04-backup-recovery.md) before further write admission.

<!-- book-navigation -->

3.2 Local execution

[Book contents](../README.md) · [3. Execute and verify](README.md) · [中文](../../zh/03-execution-and-evidence/02-local-execution.md) · [Previous: 3.1 Durable state](01-run-store.md) · [Next: 3.3 Local daemon](03-local-daemon.md)

<!-- /book-navigation -->
