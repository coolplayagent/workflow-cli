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
work. Polling is a host responsibility. `run execution-history` uses an exclusive
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
Leases are at most five minutes. Worker code must cooperate with its deadline:
expiry fences result admission, but cannot terminate arbitrary synchronous Rust
code or undo effects. This is a trusted local clock/process/storage boundary,
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

## Explicit storage migration

New databases use storage schema 5. Schema 1, 2, 3 or 4 databases from prior increments
must be upgraded explicitly:

```sh
cargo run --locked -- run --artifacts /path/to/artifacts migrate /path/to/existing-runs.db
```

Migration creates the execution journal/empty authority heads for schema 1,
preserves existing leases/attempts for schema 2, validates all existing runs and
updates the version in one transaction. Schema 3 introduced required artifact
verification; schema 4 adds protected gate transitions and schema 5 protects model
policy records and immutable policy bindings. Evidence-bearing runs need
`--artifacts` during migration; omit it only for runs without dependencies. Missing
or corrupt dependencies roll back the version. Corrupt old runs roll back the
tables and version together. Running migrate again on schema 5
is harmless. Ordinary open/create never silently upgrades old data; foreign or
future schemas are refused. Logical StartRun schema remains v1. Direct worker calls retain protocol 1;
model-policy calls require protocol 2. See [model execution](model-execution.md).
Take the repository's normal database backup before operational migration; this
release does not provide backup/restore commands or disk-loss recovery guarantees.

## Evidence and remaining scope

Tests include two independent processes racing for a lease, fencing after takeover,
renewal/release/time boundaries, forged results, bounded retries, real built-in
business decisions, no invocation after storage rejection, and repeat drives after
commit. OS-process termination is forced before the result transaction, after
event/state/execution writes, and before/after commit. Reopening recovers the entire
transaction and retries only an orphan. Migration preserves v1 runs and rolls back
on corruption. Cargo and Bazel run the same tests.

R02/R04/R08/R09 remain open for model/remote adapters, write-effect
ledgers, isolated workspaces, remote artifact adapters, pause/resume, autonomous
dispatch, node parallelism, scheduling/fairness,
cluster ownership, authenticated tenants, retention and backup/restore. These
Linux process-crash checks establish no production throughput, power-loss, shared
network filesystem, business-benefit or RPO/RTO claim.

For a host-managed worker/report flow use `run acquire/claim/finish/release`. Runs
with evidence require `run --artifacts <store>` on subsequent reads and mutations.
The artifact location is host configuration, and missing dependencies prevent a
healthy recovery response. The built-in `run drive` does not automatically fabricate
reports; it verifies evidence supplied by registered adapters through its store.

## Mandatory postconditions

`claim` and `drive` consume frozen gate commands under the current lease. Task
settlement can leave the run awaiting a gate; inspect business state and decisions.
A gate PASS commits its transition, receipt and proof atomically; UNKNOWN consumes
one intent and waits for explicit `retry-gate`. See [runtime postconditions](runtime-postconditions.md)
for bindings, evidence selection, repair bounds and protected event ingress.
