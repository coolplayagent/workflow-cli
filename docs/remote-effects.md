# Authenticated remote effects and R05 acceptance

Remote write tasks use the same frozen policies, operation keys, retry budgets,
effect ledger and compensation rules as local execution. The HTTPS service commits
an intent and worker assignment together in PostgreSQL. The worker invokes its
explicitly configured gateway outside that transaction, then submits an observation.
Provider credentials remain in the worker host binding; they are never assignment
data. A write-capability allowlist without an exact effect policy grants no writes.

## Provisioning and execution

New scopes initialize the additive `workflow_effect_dispatch` schema during trusted
bootstrap. An existing deployment explicitly runs:

```sh
workflow service init-effects server-binding.json
```

Initialization is transactional and idempotent. An unknown schema version is
rejected. Existing read-only schedulers do not need the new schema. These tables
retain assignments, delivery state and receipts; run images continue to use their
existing schema and effect journal.

For each permitted write descriptor, a worker credential's `CapabilityRule`
contains the exact `id`, `version`, `contract_digest`, and this additional field:

```json
{
  "effect": {
    "policy": {
      "identity": {"id":"sandbox-release-policy","version":"1.0.0"},
      "target": {"id":"sandbox-releases","version":"1.0.0"},
      "call_identity": {"id":"sandbox-publisher","version":"1.0.0"},
      "retry": {"max_calls":6,"initial_backoff_ms":10,"max_backoff_ms":100,"total_write_ms":60000}
    }
  }
}
```

This is a policy fragment, not a complete provisioning request. The descriptor
digest comes from `workflow_worker::Capability::new(descriptor)?.digest()`;
the policy must equal the corresponding frozen bundle binding, including its
retry budget. The worker credential must remain valid through the call deadline.
Wrong targets, principals, versions or budgets roll back both intent and assignment.
Provision separate rules for each declared compensator. The configured gateway
must enforce its own principal, resource and operation-key permissions.

Enable `"effects": true` in the scheduler JSON, alongside `instance_id`,
`worker_ids`, `lease_ms` and `scan_limit`. It defaults to false. Start an effect
worker with the same HTTP gateway binding format as local `run drive-effects`:

```sh
workflow remote schedule scheduler-client.json scheduler.json 1000 100
workflow remote work-effects worker-client.json effect-bindings.json 1000 100
```

The worker also handles builtin read-only assignments. Other adapters may compose
`work_once` and `work_effects_once` through the library. `observed_effect_calls`
counts durably accepted observations, including UNKNOWN; it is not a count of
successful business effects. Inspect the run and effect ledger for the outcome.
Scheduler reports distinguish retry waits from manual reconciliation.

## Delivery, completion and recovery

| Operation | Authorized role and behavior |
| --- | --- |
| `dispatch_effect` | Scheduler owns the exact lease; validates a live worker in the same tenant/project and its exact capability/effect policy, then atomically prepares an assignment. |
| `pending_effects` | Worker pages its own undelivered assignments before their delivery deadlines. Lost notifications can be rebuilt from this scan. |
| `effect_assignment` | Worker atomically consumes one delivery. The returned attempt must equal the durable journal, with a live lease and current node admission. |
| `observe_effect` | Assigned worker submits an observation. Scope, attempt and lease come from the server row. Identical retries are idempotent; conflicting receipts fail. |
| `effects` | Run-reading roles page the verified ledger, including stable operation keys, calls and truthful receipts. |
| `outstanding_effects` | Administrator/recovery roles inspect unresolved assignments, including delivered work and revoked workers. |
| `resolve_effect` | Recovery identity holds its own live lease and supplies actual reconciliation evidence. The authenticated actor replaces any caller annotation. |
| `control` | Runner/recovery sends a typed pause, resume or cancel with an event ID and expected revision. There is no arbitrary event/state endpoint. |

All operations use the existing protocol-1 request/response envelope. The new
operation names are additive: old servers reject them, and old worker commands
continue using ordinary assignments. Gateway requests retain their existing
effect-attempt schema. A response request ID never substitutes for the durable
operation key or assignment ID.

Delivery is **single-use**, including concurrent processes sharing one credential.
The server commits `delivered` before returning a write attempt. If that response
is lost, or the worker dies before submitting its observation, do not fetch or
execute the same assignment again. After ownership expires or is explicitly
released, a successor lease consults the existing ledger: query first where
supported, retry only within a declared idempotency guarantee, otherwise require
manual reconciliation. Intent, policy, input digest and operation key survive;
attempt ID and epoch change. A successful provider write is not inferred from a
missing local response, and a failed query cannot erase an older unknown write.

Call deadlines stop new I/O. A truthful late receipt may settle while the worker
credential and owning lease remain live. Expired/replaced leases cannot submit
even duplicate observations. Revocation is ordered with operations using the
credential lock and is checked through commit. A cancelled or paused node cannot
receive an undelivered write; a previously delivered call can still return its
real receipt. Cancellation does not automatically undo external resources.

Manual reconciliation requires releasing or waiting out scheduler ownership,
acquiring a lease with the recovery credential, and submitting `resolve_effect`.
Its fields are `lease`, `operation_key`, and the existing `ManualResolution`.
Use an actual provider receipt or confirm non-application only after checking
the provider and quiescing old writers. An exact resolution retry is idempotent
under a live lease. Fixed provider-error descriptions replace arbitrary exception
text; structured receipts/outputs remain scoped application data, not sanitized
free-form telemetry.

## Acceptance evidence

The tests use isolated provider systems and disposable PostgreSQL. They do not
contact production releases, PRs or deployment credentials.

| R05 acceptance | Local and shared evidence |
| --- | --- |
| Kill after provider write, before receipt commit; one logical effect | SQLite's `real_http_write_then_killed_worker_is_queried_and_duplicate_delivery_creates_one_release`; HTTPS `https_effect_worker_crash_recovers_provider_receipt_without_duplicate_write` kills real worker/scheduler processes after an independent PostgreSQL provider commit, then verifies exactly one write, one query and one provider resource. |
| Redelivery/races preserve keys; distinct loop instances differ | Local `two_process_claim_race_dispatches_only_one_durable_attempt`, `loop_instances_use_distinct_effect_keys_and_restart_preserves_each_receipt`; shared `effect_authority_policy_rollback_and_single_delivery_race` races independent connections, plus revoked-worker and HTTPS takeover checks. |
| Unsupported lookup/deduplication stops for audited reconciliation | Local `unknown_without_guarantees_and_query_absence_require_audited_manual_resolution`; shared `effect_unknown_without_lookup_requires_authenticated_manual_resolution` rejects wrong roles/owners, stamps the actual actor and verifies durable, idempotent reconciliation. |
| Permanent failures stop; transient retries retain bounds | Local `permanent_failures_settle_once_and_transient_calls_keep_backoff_and_budget`; shared `effect_shared_retry_budget_and_permanent_errors_survive_reopen` reopens the service between calls and checks exact call counts and final states. |
| Ordered compensation survives failure and never redoes completed undo | Local `killed_compensator_after_provider_commit_recovers_by_query_without_repeating_earlier_undo`; shared `effect_shared_compensation_order_and_manual_takeover` verifies reverse resource order, reopens between calls, retains a failed undo for authenticated recovery, and checks each completed compensation has one call. |
| Cancellation races retain true receipts | Local `pause_drains_real_receipts_and_cancel_recovers_without_reissuing_write`; shared `effect_control_cancel_and_late_receipt_preserve_truth` checks before/after delivery, CAS/idempotency and a late truthful receipt under a live lease. |

Run the normal repository checks plus the mandatory database suites:

```sh
cargo test --workspace --locked
# WORKFLOW_TEST_POSTGRES must name a disposable database.
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo test -p workflow-service --locked -- --ignored --nocapture --test-threads=1
bazel test //...
```

Independent database fault experiments run serially so unrelated tests do not
consume each other's real lease/deadline windows. Each race or takeover test
retains its competing connections/processes and unchanged timing assertions.

The HTTPS fault test prints measured recovery time with an eight-second lease.
Its 60-second observation ceiling is a test bound, not a production RTO. The
failure model is process loss and a late stale worker, not whole-database loss
or a partitioned provider. Provider key retention and authoritative query semantics
remain required; state fencing cannot physically prevent an already-running
old worker from contacting an external system. No global exactly-once guarantee
is made. Action-specific gate/approval consumption and workspace measurements
remain R03 work; provider secret grants, sandboxing and retention remain R14 work.
