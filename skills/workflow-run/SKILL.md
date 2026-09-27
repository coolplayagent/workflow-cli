---
name: workflow-run
description: Create and inspect durable workflow-cli runs, submit trusted events, cancel with an expected revision, inspect the command outbox and verify storage recovery, and drive local read-only tasks with durable leases. Use for persistent workflow progress and explicit local execution; no background daemon remains.
metadata:
  version: "1.2.0"
---

# Workflow run

Resolve `workflow` and read `workflow help`. Use `cargo run --locked --` in this
source checkout or the Bazel binary; `bazel run` needs absolute file paths. Consult
`docs/run-store.md` and `docs/local-execution.md` for transaction, lease and failure semantics.

Choose the explicit run database from the user's task. `run init <db>` alone
creates a store. Do not point it at a definition-registry database or silently
initialize a different database after a query fails. Inspect the error and path.
For schema 1 or 2, use the explicit `run migrate <db>` transaction when upgrading is
within the task scope. A future/foreign schema must not be overwritten to make it open.

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

For authorized local read-only execution, use `run drive <db> <id> <owner>
<max-commands>` with a budget of 1–100. This persists the lease/attempt, invokes
built-in adapters and commits checked results. Inspect task counts and business
status. It starts no background daemon: timers advance on the next explicit drive.
Use `run execution-history <db> <id> 0 <limit>` for lease, request, result and error
observations. Do not infer task execution from an outbox command alone.
`examples/execution` exercises real built-in validation and business decisions.
A worker error stops the drive; a later explicit drive may retry read-only work,
up to three attempts per command. Lease conflicts require inspecting ownership
and waiting for release/expiry. Never edit epochs, reset budgets or fabricate a
result. Write effects, uncertain reconciliation need their separate verified host mechanisms. Artifact
evidence requires a configured store: use `run --artifacts <store> <operation> ...`
and the workflow-artifact Skill. That configuration is needed again for recovery
queries; never remove evidence to make a missing dependency look successful. A release error can follow a committed task;
read status/history before retrying.

Pending work persists when the process exits. Repository examples under `examples/runs`
use simulated approvals, results and receipts; label them as simulation.

For administrative host events, obtain authenticated, validated observations, then read
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
work or manually bypass a managed task result. Receipts follow sequence order and do not mean task success. The consumer
must deduplicate stable command IDs after lost acknowledgements.

If storage is full/busy/read-only or the reply is lost, do not claim acceptance.
Inspect `run status` and retry the exact event/start/receipt identity when justified;
never change IDs to hide uncertainty. Exact duplicates produce no new transition
commands. `run verify <db> <id>` checks full replay, checkpoint plus tail, outbox
receipts and execution authority. Corruption must be investigated using the original records, not by
editing hashes or deleting recovery dependencies.

Report the database, run ID/digest, revision, business status, pending intents and
verification result. Distinguish committed progress from actual external work.
Pause/resume, effect execution and backup/restore are not provided by this release.


For an explicitly managed worker flow, acquire a lease with `run acquire`, persist
the returned lease object, and obtain the request/grant through `run claim`.
Dispatch the actual worker, publish any report, then pass the exact attempt/result
to `run --artifacts <store> finish`. Use `run renew` before expiry when needed and
replace the local lease token with its returned value. Release the current lease
afterward. `run tick-due` observes due waits under the lease; `run attempt-failed`
records an actual worker protocol error, never an invented business outcome.
