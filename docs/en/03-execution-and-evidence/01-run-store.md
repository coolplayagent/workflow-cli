# Durable run storage

`workflow-runstore` defines the storage port. `workflow-runstore-sqlite` implements
it with SQLite and an explicit Bazel library. The kernel still computes control
flow without I/O; the store commits its state, accepted events and command intents.
This port supplies persistent progress and an inspectable outbox. The separate
[local executor](02-local-execution.md) adds read-only dispatch through durable leases
and attempts. The [effect host](../04-effects-and-recovery/02-durable-effects.md) separately executes managed
writes and [declared compensation](../04-effects-and-recovery/03-ordered-compensation.md). The storage port itself starts no background daemon; unattended execution uses
the separately configured [local daemon](03-local-daemon.md).

## Local CLI example

Use a new explicit database path. Only `run init` creates a database; all other
commands require an existing compatible store. CLI queries open the existing
file with write access so SQLite can recover an interrupted rollback journal;
their SQL transaction only reads logical run data. The Rust adapter also exposes
`open_readonly` for already recovered read-only storage. Definition-registry databases use
a different application ID and cannot be passed to these commands.

```sh
cargo run --locked -- run init /tmp/review-runs.db
cargo run --locked -- run start /tmp/review-runs.db examples/runs/review-start.json
cargo run --locked -- run status /tmp/review-runs.db demo-review-approved
cargo run --locked -- run outbox /tmp/review-runs.db demo-review-approved 0 100 pending
cargo run --locked -- run acknowledge /tmp/review-runs.db examples/runs/timer-delivered.json
cargo run --locked -- run event /tmp/review-runs.db examples/runs/review-approved.json
cargo run --locked -- run event /tmp/review-runs.db examples/runs/implementation-completed.json
cargo run --locked -- run history /tmp/review-runs.db demo-review-approved 0 100
cargo run --locked -- run verify /tmp/review-runs.db demo-review-approved
```

These example contracts, approvals, task results and delivery receipts are
**simulated**. No timer service is registered and no coding adapter is called.
Each CLI invocation opens a new connection. Closing the process preserves committed
state; no background process advances it. `run status` can show running/waiting
while all dispatchers are absent. Submit a validated `advance_time` event through
`run event` when a real host observes a due deadline.

`run cancel <db> <run-id> <event-id> <expected-revision> <at-unix-ms>` records a
global cancel event using the actual run fingerprint. It remains subject to CAS;
issued work may leave the run cancelling until definite task results arrive.
Cancellation never undoes an external write. For a new event shape use
`workflow schema kernel-event`; start and receipt inputs use `run-start` and
`run-receipt`. JSON inputs are strictly decoded and limited to 2 MiB.

The [run guide](../../../skills/workflow-cli/references/run.md) covers durable operations.
The [replay guide](../../../skills/workflow-cli/references/replay.md) covers simulations
and file checkpoints. The [kernel guide](../02-process-definition/04-kernel-semantics.md) defines business
status, branch, cancellation, loop and deadline behavior.

## Transaction and identity contract

| Operation | Persistent behavior |
| --- | --- |
| Start | Validate/canonicalize the complete bundle, register immutable workflow/capability version digests, write the run seed, revision 1 state, initial checkpoint, delivery head and initial commands in one immediate transaction. |
| Apply event | Read and verify the authoritative run under the write transaction, apply the kernel's expected-revision check, append the accepted event, update the state, optionally checkpoint and append commands, then commit. |
| Exact start retry | Same run ID and identical semantic start return the current state and an empty duplicate transition. Changed bundle/inputs/start time/limits reject with `start_conflict`. |
| Exact event retry | Same event ID/content returns the current state and no new commands, even after termination. Changed content or stale revision rejects without partial writes. |
| Delivery acknowledgement | Atomically append an immutable receipt and advance its separate ordered digest chain. It does not alter run revision or mark a task completed. |
| Reads/verify | Use one consistent read transaction and fail on missing/damaged recovery data. They neither advance time nor call external adapters. |

The same workflow or capability ID/version cannot acquire a different digest in
this store, even through another run. Publish a new version for changed content.
This local catalog records supplied, checked content; it does not authenticate an
external registry or authorize the declared capabilities. Bundle/definition locks
remain available for all historical runs. There is no delete or retention command.

SQLite writers use `BEGIN IMMEDIATE`, foreign keys and `synchronous=FULL`; lock
waits are bounded at five seconds. Every mutation returns success only after
`commit()` succeeds. Busy, read-only, full-disk and other storage failures produce
an error. Run state revisions advance one at a time under an explicit conditional
update. SQL triggers prevent mutation/deletion of run seeds, bundles, binding
locks, events, checkpoints, commands and receipts through ordinary SQL writes.

If the process dies after commit or its output cannot be delivered, the caller may
not know whether it committed. Read the run and retry the **exact** start/event or
receipt identity; never fabricate a replacement ID to conceal an uncertain reply.
CLI exit 0 means the storage operation succeeded, not that the business workflow
succeeded. Inspect `result.snapshot.status` for mutations or `result.status` for
status. Rejected requests/transactions exit 1; usage/input I/O/output failure exits
2. A response exceeding 2 MiB reports an error and possible prior commit. Use
paginated history/outbox and re-read the state after an ambiguous response.

## Journal, checkpoints and corruption checks

Each run retains its immutable seed/bundle, current state, accepted events and deterministic outbox. Schema 12 saves state checkpoints at revision 1 and every 16 revisions, binding the state to event/outbox prefix digests and the previous checkpoint. Normal recovery validates identities, digest chains, receipts and execution proofs, then replays at most 15 tail events. It does not independently replay every checkpoint prefix. Explicit `run verify` performs one full replay and checks every retained checkpoint and command. Legacy checkpoints remain immutable after upgrade.

Receipts form an ordered prefix bound to exact commands and the delivery head. Missing or changed records fail verification. The integrity boundary excludes a database owner who can rewrite both records and hashes. Explicit `run migrate <db> <new-backup-file>` upgrades schemas 1–11 to 12 with a verified backup. Reads still scan retained records; no constant-time recovery or production throughput guarantee is claimed. Inspect `run history-usage` and use [bounded successor segments](10-long-running-agents.md) before reaching the existing event/frame/byte limits.

## Outbox and future hosts

Each intent has a per-run sequence, run revision, command index, command digest
and stable `command_id = digest(run_digest, revision, command_index)`. Replay,
process restart and duplicate events preserve these identities. Delivery receipts
bind the run, command ID/digest and a stable host delivery ID. Exact receipt retry
is accepted; changed receipts reject. Acknowledgements follow sequence order.

`run outbox ... pending` repeatedly exposes unacknowledged intents. A real host
must durably deliver/register each intent before acknowledging it. Delivery can
repeat after a lost acknowledgement, so the destination must deduplicate stable
command IDs. Reading pending commands acquires **no lease**. Multiple readers
must not be treated as authorized concurrent dispatchers. `run drive` obtains a
run lease, preserves execute/cancel order, persists attempts and validates results
through the execution port. Remote authentication and write-effect identities
remain host work. A delivery receipt is neither a task
result nor proof of an external effect. The local executor supports read-only work and persisted timer registration;
write capabilities use the separate durable effect driver.

`run list` uses a lexical run-ID cursor; history uses exclusive revision cursors;
outbox uses exclusive sequence cursors, with `all` or `pending`. Limits are 1–100.
`next_cursor: null` ends the current page stream. Receipts can change which entries
are pending; these queries are inspection pages, not durable claims.

## Verified failure boundary and remaining R04 work

Tests use independent OS processes and force termination before a transaction,
after event/state writes, before commit and after commit before reply/dispatch.
Recovery observes the old complete state or the new complete state, with no
missing command. Races exercise exact duplicate delivery and completion versus
cancel CAS. Other tests cover SQLite page-limit `SQLITE_FULL`, read-only writes,
busy writers, CLI recovery of a hot rollback journal, malformed/foreign storage,
immutable version drift, deliberately
damaged journals/outbox/receipts, and multi-checkpoint recovery. The five kernel
scenarios reopen storage between events, retaining waits, loop frames, winners
and uncertain reconciliation.

This is Linux process-crash and SQLite fault evidence on the tested filesystem;
it does not prove power-loss durability of arbitrary VFS/filesystems, shared
network-disk multiwriter safety or disk-loss recovery. No RPO/RTO is claimed.
Run leases, fenced result commits and bounded read-only retries are covered by
the local execution guide, including durable pause/resume controls. Managed write retries and the effect ledger are covered by the effect guide.
[Backup/recovery](../04-effects-and-recovery/04-backup-recovery.md) now covers verified local snapshots,
relocation and new ownership generations. [Local daemon](03-local-daemon.md), [shared artifacts](../05-shared-execution/04-shared-artifacts.md) and
[shared recovery](../05-shared-execution/07-shared-recovery.md) document the later timer, dependency and
archive acceptance evidence. Retention and environment limits remain explicit in
those chapters.

Schema 3 adds required artifact dependency verification for execution results.
Supply `run --artifacts <store>` for runs with evidence; unconfigured or corrupt
dependencies fail reads and mutations. See [typed artifacts](05-artifacts.md).

Schema 4 protects [mandatory postconditions](08-runtime-postconditions.md). Recovery
recomputes every gate decision from its historical execution prefix and retained
artifacts. Raw gate events and manual gate receipts are rejected; runs containing
postconditions also reject raw successful task events. Policy versions bind
immutable content. Upgrade with the artifact reader when existing runs carry
evidence, so dependency failures roll back the migration.

Schema 10 adds durable restored ownership and external-effect reconciliation.
Use [local backup/recovery](../04-effects-and-recovery/04-backup-recovery.md) for consistent SQLite images and
retained artifact content; copying live files does not implement that protocol.

Schema 11 adds protected definition migrations and retained storage-upgrade
records. [Version migration](../04-effects-and-recovery/05-version-migration.md) describes reviewable plans,
fresh result/approval policy, historical snapshots and verified storage rollback.

<!-- book-navigation -->

3.1 Durable state

[Book contents](../README.md) · [3. Execute and verify](README.md) · [中文](../../zh/03-execution-and-evidence/01-run-store.md) · [Previous: 2.5 Reviewed SOP templates](../02-process-definition/05-reviewed-templates.md) · [Next: 3.2 Local execution](02-local-execution.md)

<!-- /book-navigation -->
