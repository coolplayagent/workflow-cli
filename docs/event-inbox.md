# Durable event Inbox

The Inbox retains a trusted host's callback before or during a durable wait.
Receiving a message and applying its decision are separate facts. A committed
receipt can be `pending`, `applied` or `rejected`; only `applied` advances a wait.
The portable kernel owns matching and replay, `InboxStore` owns transactional
ingress/query contracts, and SQLite commits receipt state, event, checkpoint and
new outbox commands together. No worker or model session stays open for a wait.

## Commands and bindings

```sh
workflow run waits runs.db run-id 0 100
workflow schema run-signal
workflow run receive runs.db verified-signal.json
workflow run inbox runs.db run-id 0 100
workflow run history runs.db run-id 0 100
workflow run verify runs.db run-id
```

`waits` returns active waits, their original deadline and whether the run is
paused. Copy the target and correlation ID from the actual wait registration.
The target binds a node instance, immutable definition digest, event name and
actual typed input digest. Correlation is a digest of the immutable run digest
and that target; cloning a run or changing inputs cannot reuse a correlation.
The host must put the artifact/revision being reviewed in the wait's typed inputs
if that content is part of the decision. An empty input contract does not attest
to any external artifact. Automatic artifact-subject approval policy is not yet
provided by this increment.

The submission includes the run ID/digest and a schema-1 message containing a
stable provider message ID, copied target/correlation, host source label,
`approve`, `reject` or `request_changes`, a nonempty reason, typed outputs and an
absolute expiry. Reasons are at most 1024 bytes. Reject/request-changes must have
empty outputs and both follow the workflow's declared `rejected` edge; their
distinct decisions and reasons remain in history. Define that edge as the desired
rework or terminal path. Approve validates outputs and follows `accepted`.

`receive` samples the trusted host clock inside its write transaction. The caller
does not select a run revision or receipt timestamp. Before commit admission the
host samples its clock again and checks the minimum of message expiry and wait
deadline. A clock reversal or newly expired decision rolls back the entire
transaction; retry can then persist an expired receipt. Fenced task, gate and
timer settlement perform the same check when they activate an early callback.
Raw administrative `run event`/pause/resume still represent trusted logical-time
observations and are not an authenticated external ingress. Retry the exact submission
after a lost response: it returns the current receipt with `duplicate: true`
without applying again, even after expiry or run termination. Changing any message
field under the same ID is a conflict. Exit 0 means durable receipt, not approval;
inspect `result.entry.status.status` and its rejection reason. Pagination uses the
last received revision for Inbox and the last instance ID for waits, limits 1–100.
Follow `next_cursor`; a page is not the complete history.

## Ordering, early delivery and recovery

An early callback can target an allocated pending wait instance with the exact
expected input digest. It remains pending until activation supplies actual inputs.
Unallocated future loop/subworkflow instances are refused in a retained
`unknown_instance` receipt; the host must obtain their new identities first.
Multiple messages compete in committed receive order, not message-ID sort order.
The first eligible decision settles the wait; later messages retain
`already_settled` receipts. A rejected or malformed message never grants a route.

While paused, eligible callbacks remain pending. Resume rechecks actual inputs,
message expiry and the original wait deadline. Expired callbacks cannot authorize
work. Timer expiration is reduced before callbacks at the same logical timestamp;
cancellation wins once committed, and subsequent receipts say `run_cancelled`.
If approval commits first, a competing cancellation with an old expected revision
is refused and must be reconsidered against current state. Cancellation after an
applied decision does not rewrite that historical approval.

Pending expiry is observed on the next committed event or explicit timer drive;
there is no background daemon. Queries never silently advance logical time.
No sampled model result is involved in matching or recovery. Checkpoint plus tail
and complete event replay independently rebuild Inbox state; edits or missing
journal entries disagree with the stored head. Atomic commit ensures crash recovery
sees the entire callback transition or none of it. The retained Inbox is bounded
to 256 messages per run, plus existing event, message and checkpoint size limits.
A capacity rejection is not a durable acceptance; no automatic pruning discards
receipts or deduplication identities.

## Host boundary and compatibility

`source` is a host-attested label, not authentication or an actor role. The current
local CLI trusts the administrator with database access. A future HTTP/webhook or
approval service must authenticate the caller, authorize the responder and derive
this label before using the port. Do not expose `run event` or accept model text as
a human decision. This increment does not claim tenant isolation, signed approval
evidence, access-policy enforcement or R06 completion.

Storage schema 7 protects Inbox semantics from old executors. Use explicit
`run [--artifacts store] migrate db` for schema 1–6; existing seeds, events and
empty-Inbox snapshots retain their exact digests. A lost acknowledgement is retried
with the original submission; normal recovery never calls a webhook or model.

Tests cover early callbacks before activation and after checkpoint restore,
paused expiry, input/definition/event/output mismatches, changed duplicate content,
terminal/late delivery, independent-process duplicate and cancellation races,
five killed-writer phases, expiry/clock reversal before commit, early-callback
expiry during task settlement, corrupted Inbox heads and CLI receipt/query behavior.
