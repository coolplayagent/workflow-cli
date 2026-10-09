# Shared scheduling and R09 acceptance

Cluster mode separates the TLS control API, project-scoped schedulers and execution
workers. PostgreSQL owns all run leases, node attempts, assignment delivery,
admission permits, worker liveness and dead letters. Any API replica observes the
same authority. A scheduler's scope is its authenticated tenant/project; ownership
inside that partition is the run lease, not a process-local leadership flag.
Multiple schedulers may compete in one partition. Every acquired lease has an
owner, acquisition identity, database-time expiry and increasing epoch.

## Enable and operate

Upgrade API replicas first, drain existing execution grants, and install a trusted
host configuration. `examples/cluster/policy.json` is a bounded example, not a
production sizing recommendation. Wrap it as:

```json
{
  "tenant": "team",
  "expected_revision": null,
  "policy": { "...": "contents of examples/cluster/policy.json" }
}
```

```sh
workflow service configure-scheduling server.json cluster-configuration.json
workflow remote managed-work worker-client.json examples/cluster/worker.json 100000 40
workflow remote schedule scheduler-client.json scheduler.json 100000 40
```

The configuration operation is local to the trusted database host and absent from
RPC. Initial activation rejects unsettled, unexpired task or effect grants; drain
them first. Subsequent updates require the returned exact `expected_revision`.
Retain the returned revision/policy digest with the deployment change record.
Activation creates scheduling schema 1 and upgrades access schema to 3. Older API
binaries reject this schema on request; they cannot serve quota-free traffic.
`migrate-access` preserves schema 3. Tenants without a policy retain legacy
scheduling behavior under a compatible API. Direct database-owner SDKs are trusted
host code and must be upgraded with the API.

Example scheduler configuration:

```json
{
  "instance_id": "scheduler-a",
  "cluster": true,
  "effects": false,
  "worker_ids": ["issued-worker-credential-id"],
  "lease_ms": 120000,
  "scan_limit": 100
}
```

Run a scheduler pool for each project that requires progress; credentials do not
silently expand across project boundaries. Size worker pools and tenant/project
limits together. This implementation does not allocate hosts or promise a minimum
CPU share between project deployments. Avoid sharing one worker credential between
processes: liveness, drain and per-worker limits belong to that identity.

## Admission, fairness and transport

Tenant, project, exact capability ID/version, model pool and worker each have an
explicit concurrent-grant ceiling and sliding sixty-second admission limit.
Project/capability/model overrides are bounded; model pools map frozen policy
ID/version to an operator-chosen provider pool. Unmapped policies use their exact
identity. Limits count **execution assignments**, including model-task assignments,
not individual model HTTP calls, tokens or money. R02's frozen model policy bounds
calls/tools/tokens within each admitted task. Provider-level billing budgets are a
separate concern. Effect writes and reconciliation queries both consume permits.

All API replicas serialize each tenant's quota decision through its policy row.
The execution claim, durable assignment and admission permit commit together.
Completion releases concurrency but retains the minute's rate token. Expiry releases
the grant slot; revoked/released work may conservatively occupy its slot until its
original deadline. Lowering a policy blocks new grants until usage fits. It does
not retroactively cancel already authorized external I/O. Legacy single-worker
dispatch operations obey the same installed quotas.

The bounded active-run queue rejects excess starts/resumes transactionally. Existing
active runs discovered when enabling the policy are grandfathered. Within a project,
selection orders last selection time plus `(9 - priority) * priority_step_ms`, then
run ID. Priorities are 0–9; older low-priority candidates eventually outrank freshly
selected high-priority candidates. Selection is only a hint; exact current leases
still decide ownership. Separate tenant quota locks keep one tenant's grants from
consuming another's limit. This is bounded rotation with priority aging, not a
weighted global scheduler or a production latency guarantee.

The durable assignment tables are the transport. Workers repeatedly scan their
scoped pending records, so there is no required external queue notification to
lose. State/Outbox changes and assignment admission use the same transaction;
results and durable Inbox/receipt deduplication also commit together. A disconnected
poller resumes by scanning; a lost post-commit dispatch reply is not permission to
repeat an external write. Effect delivery is single-use and its successor queries
the R05 ledger/provider before considering a write.

## Heartbeats, renewal and rolling drain

Managed workers register a pinned runtime version and heartbeat independently of
synchronous capability execution. Exact capability, model policy and effect
contracts still authorize each assignment. Runtime version is a host declaration,
not binary attestation. Missing/stale heartbeats, draining identities or versions
removed from the allowlist defer new work. A heartbeat failure stops the managed
worker from accepting more work. Successful heartbeats cannot resurrect an expired
execution grant.

Cluster schedulers renew a live lease when less than half its configured lifetime
remains. Renewal preserves its epoch and atomically updates stored assignment lease
identities. Frozen task/call deadlines and admission expiry never increase. Expired
node attempts are reclaimed under a later attempt/lease epoch; late completions
cannot alter the run. Persistent timers and Inbox reconciliation remain on every
leased scheduler scan, including after process restart.

`managed-work` composes builtin read-only work, optional `models` (bundle and host
bindings paths) and optional `effects` (host bindings path). All shared provider
bindings must agree on the authenticated execution principal. See the model and
effect CLI examples for their binding formats.

On SIGINT/SIGTERM, an independent monitor requests sticky server-side drain while
the current call finishes. Administrative `control_worker` can also drain an
identity. Once the database acknowledges drain, new assignments are refused;
already accepted assignments are settled until zero live grants or a reported
drain error. Only then does the CLI return `drained: true`. Restarting or sending a
normal heartbeat does not clear drain; explicit administrator resume is required.
Install the new version under a new worker identity, include it in scheduler routing,
then retire the old version after drain. Exact-contract mismatches leave no
speculative attempt when another compatible worker is tried.

The drain timeout starts after the current synchronous adapter returns. Rust
callbacks cannot be forcibly preempted by this library. Builtin/model/HTTP bindings
enforce their existing deadlines; noncooperative host adapters require process
isolation and service-manager termination. Killing a process still requires lease
expiry and, for uncertain writes, effect reconciliation. Scheduler termination
leaves its durable grants recoverable after expiry rather than reporting them
drained.

## Dead letters and recovery

Permanently incompatible routing or worker execution errors park the run with a
stable reason, run revision and snapshot digest. Busy quotas and temporarily stale
workers defer instead. `dead_letters` lists active records; `dead_letter` retrieves
one record plus its immutable resolution receipt. Only scoped recovery credentials
may call `resolve_dead_letter` with the exact current reviewed revision/digest,
retry/archive choice and bounded reason. A changed snapshot requires a new review.
The server binds the receipt to the authenticated actor and time; identical retries
are idempotent. Archiving an active run is rejected, and retrying a terminal run is
rejected. Replay resumes normal authoritative dispatch, including R05 reconciliation;
it does not execute a stale serialized task or bypass an effect check.

| Failure model | Recovery boundary and RTO | RPO / side effects |
| --- | --- | --- |
| Scheduler/worker process loss with healthy PostgreSQL | Remaining ownership/attempt/permit expiry, then a successful scan, claim and execution; scan/I/O latency adds to configured lease time | Zero loss of acknowledged database transactions; read-only execution may repeat |
| Worker/API network partition | No successful heartbeat/claim/commit, so no new confirmed authority; after connectivity returns, scan and lease rules apply | Committed assignments remain durable; unknown external writes require R05 query/manual reconciliation |
| Database temporarily unavailable or read-only | No transition is acknowledged; restoration of writable service plus scan/lease reconciliation determines recovery time | Previously committed state, assignments and timers remain together; no fixed database outage RTO is claimed |
| Whole-database loss/restore | Full archive verification, isolated restore, credential/ownership fencing, operator/provider reconciliation, then explicit resume | RPO is the backup/WAL boundary, not zero; missing post-backup effects need independent reconciliation |

`fence-restored` also marks scheduling permits finished and all retained workers
draining in the same recovery transaction. Re-provision fresh identities and resume
held runs only after the [database recovery procedure](07-shared-recovery.md). Commit
fencing never proves that a stale process stopped external I/O; the
[effect protocol](05-remote-effects.md) defines that separate safety boundary.

## Executable evidence

```sh
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo test -p workflow-service --locked -- --ignored --nocapture --test-threads=1
cargo build -p workflow-cli --locked
python3 examples/cluster/acceptance.py target/debug/workflow
```

These commands require a disposable `WORKFLOW_TEST_POSTGRES`; the mandatory
PostgreSQL CI job runs them. Normal Cargo/Bazel checks also compile every contract.
The R09 process fixture uses two active schedulers and three workers, kills the
owner, SIGSTOPs/resumes its old worker, checks rejected late completion, compares
local/cluster final state, loads two tenants, and verifies version-2 worker drain.
It records total throughput, nearest-rank p95 admission delay, the smaller tenant's
p95, lease/scan settings and machine configuration. The 60/90-second test ceilings
are observation bounds, not production SLOs.

One local run on 2026-09-30, Linux x86_64, Intel i7-1260P, 16 logical CPUs and
16,061,032 KiB reported memory, observed: 8,000 ms lease, 40 ms scan, 10,262 ms
owner-loss recovery, 15 queued jobs in 28,355 ms (0.529 jobs/s), 18,791 ms overall
p95 admission delay and 7,081 ms smaller-tenant p95. This small durable-image fixture
includes real TLS/database work on a shared development host; it is not a capacity
benchmark. Each CI run emits its own measurements.

The CLI fixture holds a real model provider request during SIGTERM, observes drain
while execution is still active, confirms completion, retains the model rate token,
and then rolls from runtime 1.0.0 to 2.0.0. Its optional `--legacy-binary` check
confirms that a pre-R09 binary rejects access schema 3. PostgreSQL contracts cover
concurrent quota races, all admission scopes, renewal/deadline preservation, expired
heartbeat, read-only rollback, reviewed dead-letter retry, and effect query after
worker replacement. Existing multi-process and database recovery fixtures continue
to cover dispatcher loss, disconnected durable scans, timers and restored effects.

<!-- book-navigation -->

5.6 Cluster scheduling

[Book contents](../README.md) · [5. Shared execution](README.md) · [中文](../../zh/05-shared-execution/06-cluster-scheduling.md) · [Previous: 5.5 Remote external effects](05-remote-effects.md) · [Next: 5.7 Shared backup and recovery](07-shared-recovery.md)

<!-- /book-navigation -->
