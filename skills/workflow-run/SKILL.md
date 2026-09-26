---
name: workflow-run
description: Create and inspect durable workflow-cli runs, submit trusted events, cancel with an expected revision, inspect the command outbox and verify storage recovery. Use for persistent workflow progress; this storage CLI does not dispatch workers or run background timers.
metadata:
  version: "1.0.0"
---

# Workflow run

Resolve `workflow` and read `workflow help`. Use `cargo run --locked --` in this
source checkout or the Bazel binary; `bazel run` needs absolute file paths. Consult
`docs/run-store.md` for transaction and failure semantics.

Choose the explicit run database from the user's task. `run init <db>` alone
creates a store. Do not point it at a definition-registry database or silently
initialize a different database after a query fails. Inspect the error and path.
A future/foreign schema must not be overwritten to make it open.

Read `workflow schema run-start`, obtain exact definitions and descriptors from
the task's catalog, and validate the bundle with `kernel check`. Use the intended
run ID, typed inputs, logical start time and limits. `run start <db> <start.json>`
atomically commits the seed/state/initial commands. Reusing the same run ID with a
changed start is a conflict; workflow/capability versions also bind immutable
content within the store. Publish a new version for a legitimate content change.

Use `run status <db> <id>`, `run history <db> <id> <after-revision> <limit>` and
`run outbox <db> <id> <after-sequence> <limit> pending`. Start numeric cursors at 0,
use limits 1–100 and follow `next_cursor`. `run list <db> - <limit>` discovers runs.
Read `result.status` for status or `result.snapshot.status` for mutations; exit 0
means the storage command succeeded, including a failed/cancelled business run.

The CLI starts no worker or daemon. Pending task/timer intents remain pending
when the process exits. Never claim a timer is actively scheduled or a task ran
because its command is in the outbox. Repository examples under `examples/runs`
use simulated approvals, results and receipts; label them as simulation.

For real progress, obtain authenticated, validated host observations, then read
`workflow schema kernel-event` and submit `run event <db> <event.json>`. Use the
actual run digest, instance ID and expected revision. Do not invent task results,
approvals, effect receipts or reconciliation facts. Use a stable event ID and
nondecreasing observed logical time. Reading the outbox does not grant a lease or
permission to invoke an external capability.

For requested cancellation, use `run cancel <db> <id> <event-id> <revision>
<at-unix-ms>`. A cancelling status means issued work still needs a definite
outcome. Cancellation cannot undo writes, and uncertain effects must be reconciled
before continuing or retrying. On a revision conflict, inspect current state and
history before deciding whether the requested event remains valid.

`run acknowledge <db> <receipt.json>` is only for a real host's confirmed durable
delivery/registration, or an explicitly labeled simulation. Read `schema run-receipt`
and copy the exact command ID/digest. Do not acknowledge merely to clear pending
work. Receipts follow sequence order and do not mean task success. The consumer
must deduplicate stable command IDs after lost acknowledgements.

If storage is full/busy/read-only or the reply is lost, do not claim acceptance.
Inspect `run status` and retry the exact event/start/receipt identity when justified;
never change IDs to hide uncertainty. Exact duplicates produce no new transition
commands. `run verify <db> <id>` checks full replay, checkpoint plus tail, outbox
and receipts. Corruption must be investigated using the original records, not by
editing hashes or deleting recovery dependencies.

Report the database, run ID/digest, revision, business status, pending intents and
verification result. Distinguish committed progress from actual external work.
Pause/resume, effect execution and backup/restore are not provided by this release.
