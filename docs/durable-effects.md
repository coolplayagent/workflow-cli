# Durable write effects

`workflow-effects` defines a deterministic effect ledger and host adapter port.
`workflow-runstore-sqlite` records its actions in the same fenced execution
journal used by the run. `workflow-effect-http` invokes an explicitly configured
JSON gateway. Ordinary `Worker` invocation still accepts read-only capabilities.

## Frozen contracts and identity

A write task opts in through `BundleSpec.effect_bindings`: exact workflow version,
node ID and an `EffectPolicy`. The policy freezes its own version, logical target,
service principal binding and retry budget. Policy versions lock immutable
content within the run store. An empty binding list is omitted from serialization,
so previous bundles retain their digests. A binding requires a direct write task;
a declared query reference must resolve to a read-only capability in the bundle.

A primary operation key hashes the immutable run digest, concrete node instance
and primary operation slot. Attempt IDs and lease epochs are separate. Retries
reuse the key; loop iterations and separate runs have different instance/run
identities. The persisted intent binds command digest, exact descriptor, inputs,
input digest, target, calling identity and first admission time. Prepared records
also retain the exact request digest. These digests are integrity bindings, not
signatures or authentication credentials.

## Transaction boundaries

1. Acquire a live run lease. Select the first pending command under a transaction.
2. Commit the intent and prepared call before invoking any adapter.
3. Invoke the provider outside the run database transaction.
4. Under the current lease, validate the observation and commit the effect record,
   task event, resulting commands and delivery receipt together.

Recovery replays the journal without calling providers. It rechecks the frozen
intent, request digest, lease, historical pause/node admission state and the
one-to-one relationship between managed task events, receipts and execution
proofs. Raw task outcomes and manual delivery acknowledgements cannot bypass a
managed effect. A duplicated identical observation or manual resolution returns
the committed result; changed content is a conflict.

Lease fencing protects authoritative state commits. The gateway/provider must
implement its own key uniqueness and retention. A paused or expired worker can
still affect a remote system that does not enforce fencing. There is no global
exactly-once guarantee. Cancelling a workflow neither deletes a created resource
nor executes compensation automatically.

## Unknown outcomes, retries and cancellation

| Observation | Admission decision |
| --- | --- |
| First eligible task | Persist intent, then issue one write. |
| Previous owner disappeared or write response is unknown | Query the provider first when lookup is declared. Without lookup, retry only within a declared idempotency guarantee. Otherwise stop as uncertain. |
| Query found the effect | Validate the original intent/target/output receipt and settle it. |
| Query currently found nothing | Retry only with target-side idempotency. Without it, absence cannot fence an old writer: require manual reconciliation. |
| Input, permission, business or permanent rejection | Settle a declared known-no-effect error without retry, unless an older write remains unknown. A new rejection cannot erase that older uncertainty. |
| Known-no-effect transient rejection | Retry only with idempotency, bounded calls and the original write window. |
| Query fails or returns unknown | Bounded query retry; exhaustion leaves uncertain state. |
| Cancel arrives before first intent | Acknowledge cancellation without a write. |
| Cancel arrives after write admission | Admit queries, retain truthful receipts and settle cancellation. No new writes are admitted. |
| Pause | Stop new write/query admission. Already prepared outcomes may settle. |

The retry policy bounds **all calls**, including queries, at 1–32. Exponential
backoff uses deterministic 50–100% jitter, capped by `max_backoff_ms`. Backoff and
call counts survive restarts and lease changes. The write admission deadline is
the earlier of `created_at + total_write_ms` and the declared idempotency retention
window. Query recovery can occur after that write deadline, within the remaining
call count. A provider's no-effect assertion must be authoritative; a timeout,
ambiguous HTTP status or network error cannot be classified as such.

An actual receipt that arrives after a call deadline can still settle under a
live, renewed lease. Rejecting that fact would lose evidence of a real effect.
The second host clock check still fences lease expiry and clock reversal before
commit. Stale owners cannot commit; a new owner obtains the provider receipt by
query. New write requests are also bounded by the earliest pending workflow timer;
when a timer is due, advance it before attempting a new write. The host must
stop initiating I/O at the request deadline; the provider
must document how its idempotency window covers delayed requests.

The driver returns `effect_backoff` or `effect_uncertain`; it does not sleep or
start a daemon. Inspect the next admissible time through `run effects` and invoke
again deliberately. Neither process restart nor manual database edits may reset
budgets.

## CLI and gateway protocol

`examples/runs/effect-release.json` is a typed sandbox release workflow, with a
historical logical start time. Give each real run its intended ID and start time.
It does not supply approval or prove a release happened.

```sh
workflow run init runs.db
workflow run start runs.db start.json
workflow run drive-effects runs.db my-run local-owner 20 effect-bindings.json
workflow run effects runs.db my-run 0 100
workflow run execution-history runs.db my-run 0 100
workflow run verify runs.db my-run
```

`effect-bindings.json` is an array of `HttpEffectBinding` objects. Export its
contract with `workflow schema run-effect-http-binding`. To compose bounded model
steps and effects, add the optional model binding file after the effect binding
file in `run drive-effects`; both use the same frozen bundle. Example:

```json
[{
  "schema_version": 1,
  "target": {"id": "sandbox-releases", "version": "1.0.0"},
  "call_identity": {"id": "sandbox-publisher", "version": "1.0.0"},
  "capability": "REPLACE WITH THE EXACT WRITE DESCRIPTOR FROM THE BUNDLE",
  "endpoint": "https://your-authorized-gateway.example/operations",
  "api_key_env": "WORKFLOW_EFFECT_TOKEN",
  "allow_loopback_http": false
}]
```

The placeholder must be replaced by a descriptor object, not a string. The host
routes only an exact descriptor/target/principal match. Credentials are resolved
from the named environment variable, or a host secret resolver can use
`execute_with_secret`; secrets are excluded from the ledger and adapter error
messages. HTTP redirects, implicit retries and proxy discovery are disabled.
HTTPS is required except for explicitly allowed literal loopback IP addresses.
Responses are bounded by the worker message limit and request time budget.

The gateway receives `POST <endpoint>/write` or `/query`, a serialized
`EffectAttempt`, bearer authorization, `Idempotency-Key`, `X-Effect-Intent` and
`X-Effect-Request`. A query is a read-only provider lookup even though its wire
method is POST. The capability's query reference names this effect-protocol
lookup; it is not a direct `Worker` invocation with the descriptor's task ports.
The gateway must validate the principal, target and capability against its own
permissions; the supplied identity string does not authenticate them.

A successful HTTP response contains `EffectReply`: the exact attempt's request
digest plus `Observation`. An applied receipt binds the original operation key,
intent digest, target, provider resource and receipt identifiers, and typed task
outputs. Cached provider receipts are valid across attempts, but the enclosing
reply must identify the current request. Non-success status, malformed content or
a mismatched reply becomes unknown; HTTP status alone never proves no effect.
`NotApplied` must use a code/class declared by the frozen write capability.

Export contracts with `workflow schema run-effect-attempt`, `run-effect-reply`,
`run-effect-observation` and `run-effect-resolution`. As with other run schemas,
the CLI returns its normal `ok/result` envelope; the JSON Schema is in `result`.
Low-level host composition uses `run effect-claim <db> <lease.json>`,
`effect-observe <db> <lease.json> <attempt-id> <observation.json>` and
`effect-resolve <db> <lease.json> <operation-key> <resolution.json>`.
These are trusted local administrator interfaces, not remote worker auth APIs.

## Manual reconciliation

An uncertain effect blocks its reconciliation command. A manual resolution
retains a stable resolution ID, actor annotation, reason and evidence reference.
Use an actual provider receipt to confirm application, or `confirmed_not_applied`
only after checking the target and quiescing outstanding writers. Confirmed
non-application settles that task as cancelled; it never authorizes a hidden
retry. Restarting business work needs an explicit workflow decision/new run.
No effect receipt, approval or authenticated actor may be fabricated.

## Storage and verified boundaries

Storage schema 9 protects effect journal/policy and compensation semantics from older executors.
`run migrate` explicitly upgrades schemas 1–8 and revalidates existing runs.
Ordinary open refuses a different version. Migration itself is not a backup.

Tests cover an actual loopback HTTP gateway with a separate durable provider
SQLite database: kill after provider commit but before run receipt, then query
under a new lease; duplicate concurrent gateway deliveries retain one release.
Additional tests cover two-process claim contention, ten killed intent/receipt
transaction phases, loop instance isolation, retry budgets/backoff, old-writer
uncertainty, pause/cancel races, stale commits, late truthful receipts and missing
journal proofs. Real v7→v8 migration retains paused Inbox state and old readers
refuse v8. These are bounded fixtures, not a production reliability measurement.

For declared reverse dependencies, original receipt binding, irreversible effects
and manual takeover after a failed undo, see [ordered compensation](ordered-compensation.md).
Authenticated multi-tenant ingress, distributed RunStore/transport, mandatory
action-specific approval/current-workspace checks, remote artifact access and
backup/restore remain separate roadmap work. R05 remains open.
