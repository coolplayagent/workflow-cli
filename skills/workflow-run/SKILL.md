---
name: workflow-run
description: Create and inspect durable workflow-cli runs, submit trusted events, pause, resume or cancel with an expected revision, inspect the command outbox and verify storage recovery, and drive read-only tasks or managed write effects with durable leases. Use for persistent workflow progress, explicit local execution and optional unattended local daemon operation.
metadata:
  version: "1.11.0"
---

# Workflow run

Resolve `workflow` and read `workflow help`. Use `cargo run --locked --` in this
source checkout or the Bazel binary; `bazel run` needs absolute file paths. Consult
`docs/run-store.md` and `docs/local-execution.md` for transaction, lease and failure semantics.

Choose the explicit run database from the user's task. `run init <db>` alone
creates a store. Do not point it at a definition-registry database or silently
initialize a different database after a query fails. Inspect the error and path.
For schema 1–9, use the explicit `run --artifacts <store> migrate <db>`
transaction to upgrade to schema 10 when within the task scope. Omit the reader only
when existing runs have no artifact dependencies. A future/foreign schema must not be overwritten to make it open.

Read `workflow schema run-start`, obtain exact definitions and descriptors from
the task's catalog, and validate the bundle with `kernel check`. Use the intended
run ID, typed inputs, logical start time and limits. `run start <db> <start.json>`
atomically commits the seed/state/initial commands. Reusing the same run ID with a
changed start is a conflict; workflow/capability/gate-policy/model-policy/effect-policy versions also bind immutable
content within the store. Publish a new version for a legitimate content change.

Use `run status <db> <id>`, `run history <db> <id> <after-revision> <limit>` and
`run outbox <db> <id> <after-sequence> <limit> pending`. Start numeric cursors at 0,
use limits 1–100 and follow `next_cursor`. `run list <db> - <limit>` discovers runs.
Read `result.status` for status or `result.snapshot.status` for mutations; exit 0
means the storage command succeeded, including a failed/cancelled business run.

For authorized local read-only execution, use `run drive <db> <id> <owner>
<max-commands>` with a budget of 1–100. This persists the lease/attempt, invokes
built-in adapters and commits checked results. Inspect task counts and business
status. A foreground drive starts no background process: timers advance on the next
explicit drive or through an explicitly started local daemon.
Use `run execution-history <db> <id> 0 <limit>` for lease, request, result and error
observations. Do not infer task execution from an outbox command alone.
`examples/execution` exercises real built-in validation and business decisions.
A worker error stops the drive; a later explicit drive may retry read-only work,
up to three attempts per command. Lease conflicts require inspecting ownership
and waiting for release/expiry. Never edit epochs, reset budgets or fabricate a
result. Managed write effects use the separate effect host described below. Artifact
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

For requested pause/resume, use `run pause` or `run resume` with `<db> <id>
<event-id> <revision> <at-unix-ms> <reason>`. Read current status/history first.
Retain the exact arguments for safe retries after a lost reply. Pause stops new
admission and successor activation while allowing existing prepared work to settle;
it does not terminate running tools or extend any deadline. Inspect `result.pause`
or `result.snapshot.pause` as well as status. `drive` reports `paused` and releases
its lease. Resume observes expired wait/loop deadlines immediately. Raw signals while
paused are refused; the durable Inbox below retains eligible callbacks.

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
Use the verified backup/recovery flow below when requested.


For an explicitly managed worker flow, acquire a lease with `run acquire`, persist
the returned lease object, and obtain the request/grant through `run claim`.
Dispatch the actual worker, publish any report, then pass the exact attempt/result
to `run --artifacts <store> finish`. Use `run renew` before expiry when needed and
replace the local lease token with its returned value. Release the current lease
afterward. `run tick-due` observes due waits under the lease; `run attempt-failed`
records an actual worker protocol error, never an invented business outcome.

For a bundle with mandatory postconditions, read `docs/runtime-postconditions.md`.
A settled task can still await its gate; inspect node decisions and run status.
`claim`/`drive` computes gates from settled evidence. UNKNOWN is idle until an
authorized explicit `run retry-gate <db> <run-id> <instance-id> <event-id>
<expected-revision>`; retry retains the target and never reruns a checker. Inspect
its cause first. Missing attachments on settled results cannot be added later.
Raw successful task events for these runs, all raw gate events, and manual gate
receipts are rejected. Use declared repair bounds and retain failure history.

For host-managed files from a fixed Git commit, use the workflow-workspace Skill
with this actual prepared request. Its independent directory and typed capture
are separate from run execution authority. Bind the real capability inputs to
those files, then attach the capture references before settlement. The built-in
driver does not automatically allocate workspaces or inspect them at gate time.

For model-policy tasks, follow `skills/workflow-model/SKILL.md` and use
`run drive-models` with exact host bindings. Model result settlement checks explicit
records; raw task successes cannot bypass it. Recovery replays without model calls.

For a real host-verified callback, read `docs/event-inbox.md`, `schema run-signal`
and `run waits <db> <id> 0 <limit>`. Copy the actual run digest, target and correlation;
retain a stable source message ID, decision, bounded reason and expiry. Use
`run receive <db> <signal.json>` and inspect `result.entry.status.status`:
`pending` is retained, `applied` advanced a wait, and `rejected` did not authorize it.
A successful receipt is not necessarily approval. Retry an identical submission
on a lost reply; do not change its ID to hide a rejection or expired input binding.
Use `run inbox <db> <id> <after-revision> <limit>` and follow `next_cursor`.
Reject/request-changes follow the declared rejected route. Paused messages are
rechecked on resume. Future unallocated instances require their new target identity.
The source label is not authentication: the local trusted administrator must verify
the actual source/decision first. Never create a human approval from model text.

## Managed write effects

Read `docs/durable-effects.md` before dispatching a write. The workflow needs a
frozen effect binding, exact descriptor and authorized host target/principal
configuration. Use `run drive-effects <db> <id> <owner> <budget> <bindings.json>`
only within the user's authorized effect scope. An optional model binding file
after the effect binding file composes model steps in the same run. Binding IDs and digests are not
authentication. Credentials stay in host environment/secret resolution.

Inspect `run effects <db> <id> 0 100` and execution history. `effect_backoff` means
wait until the retained deadline; `effect_uncertain` requires verified provider
reconciliation. Never reissue a write manually to bypass an uncertain attempt,
change its operation key or reset its budget. Query absence cannot fence an old
writer; idempotency/retention belong to the target system. A cancellation retains
actual effects and does not automatically compensate them.

Low-level `effect-claim`, `effect-observe` and `effect-resolve` are trusted host
operations. Submit only actual observations. Manual resolution needs a stable ID,
actor annotation, reason and evidence; confirm non-application only after checking
the provider and quiescing outstanding writers. This settles the task as cancelled
and does not grant a new attempt. Do not fabricate receipts or authenticated actor
claims.

For compensation, read `docs/ordered-compensation.md`. Declare exact compensator
versions, same-frame effect dependencies and an explicit business branch. The host
binds the original Applied receipt and enforces reverse dependency order.
`irreversible: true` prohibits a compensator. Inspect `needs_attention` separately
from unknown outcomes; neither permits resetting a retry budget. Use actual
authorized cleanup receipts for manual takeover. `confirmed_not_applied` on a
compensator does not mark its original undone. A cancelled run alone is not proof
of rollback; check the original effects and their `compensated_by` links.


## Local backup and recovery

Read `docs/backup-recovery.md` and the exported backup schemas. Name the exact run
store, required artifacts and optional definition registry in the source file;
`backup create` writes only to a new directory. `backup verify` checks bytes and
full application replay. Do not copy live database files or omit required artifacts.
Backup does not stop the source. Retain the captured snapshot boundary and avoid
claiming RPO=0 for commits after it.

`backup restore` needs a new directory and an actual operator/reason annotation.
Inspect the returned generation and `run recovery`; running runs start paused and
old leases are fenced. Resume can permit read-only work and provider queries, but
the recovery barrier blocks new writes. Quiesce the original execution authority
and audit external work since the snapshot before allowing new effect admission.

For a retained intent, query/resolve through the ordinary effect protocol. For a
missing post-backup intent with a confirmed effect, `run effect-import` requires
its actual original intent and provider receipt plus an audited resolution; do
not invent timestamps, missing attempts or business outcomes. Import performs no
provider write. Missing source/provider history leaves recovery unresolved.

Use `run recovery-acknowledge` only with the actual backup/generation and evidence
that every admitted write has been accounted for. Its required
`no_missing_effect_intents: true` is an operator attestation, never a default to
fill automatically. Known unresolved effects still block it. Inspect
`pending_recovery` in the response: an exact duplicate of an older acknowledgement
does not clear a later restore. This local annotation is not remote authentication.
Restoring a database does not retire the source service or authorize two active copies.

## Optional unattended local daemon

Read `docs/local-daemon.md` and `schema daemon-config`. Use `daemon serve` with the
explicit database, artifact path and private control directory; it remains in the
foreground and can be supervised by the OS. Bindings load once at startup. Local
builtins need no network, while configured model/effect adapters still may.

Query `daemon status` at the exact control directory. Responsive control alone
is not evidence of healthy storage: inspect last scan, active run and diagnostics.
Stopped/unreachable service cannot promise timer advancement. A durable waiting
run is not proof that a scheduler is running.

`daemon stop` requests a generation-bound drain of one admitted drive. Continue
checking until status says stopped before relocating files. Do not report the
stop-request acknowledgement as completed shutdown or kill an unrelated/stale PID.
Retain the existing run leases, pause state, effect uncertainty and recovery holds.

For a portable export of the selected run database, use `run export <db> <new-directory>`
with its artifact reader when required. This exports all runs in that database as
the verified backup format; it does not retire the source or authorize a clone.
