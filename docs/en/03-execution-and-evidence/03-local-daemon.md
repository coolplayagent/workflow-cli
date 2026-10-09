# Local unattended execution

`workflow daemon serve` is an optional Linux foreground service. It repeatedly
scans the same durable SQLite runs used by foreground `run drive`, invokes the
registered adapters and advances due absolute wait/loop deadlines. No cloud
account, queue or external database is required for built-in local capabilities.
Model and HTTP effect nodes still need their configured network services.

## Start, inspect and stop

Initialize the intended run database, then save this configuration, replacing
the paths with your existing database and a new private control directory:

```json
{
  "schema_version": 1,
  "database": "/path/to/runs.db",
  "control_directory": "/tmp/my-workflow-control",
  "artifacts": null,
  "model_bindings": null,
  "effect_bindings": null,
  "poll_interval_ms": 250,
  "error_backoff_ms": 1000
}
```

Relative paths resolve from the starting process's current directory. Set
`artifacts` for evidence-bearing runs. Model/effect files use the same bindings as
`run drive-models`/`drive-effects`; all bindings are loaded and validated once
before the daemon starts, and their contents bind its configuration digest.
Unrelated model policies may share a host binding list, but every policy in an
executed run must match an exact configured binding. Changing binding files
requires a restart. Secret values remain environment lookups inside adapters.

```sh
workflow daemon serve daemon.json
# In another terminal:
workflow daemon status /tmp/my-workflow-control
workflow daemon stop /tmp/my-workflow-control
workflow daemon status /tmp/my-workflow-control
```

`serve` stays in the foreground. Its first JSON line reports startup; its final
line reports graceful termination. Use an OS service manager for persistence
across login/logout. A foreground `run drive` still exits after its bounded work;
exiting that command installs no timer service. Durable run status is independent
of service availability: query the exact configured control directory to learn
whether unattended scheduling is available.

| Observation | Meaning |
| --- | --- |
| `responsive`, phase `polling` | The control listener answered. Inspect `last_scan_unix_ms` and diagnostics for scheduler progress. |
| `responsive`, phase `busy` | One drive was admitted; `active_run` identifies it. Other timers may wait for that synchronous call. |
| `responsive`, phase `draining` | Stop was accepted; no further drive is admitted. The already admitted drive may finish. |
| `unreachable` | The ownership lock is held but control did not answer within its deadline. Scheduling progress is unknown. |
| `stopped` | No responsive service or held ownership lock was observed. No scheduling guarantee is made. |

These are observations at query time, not promises of future liveness. A live
control listener does not prove database health. The bounded diagnostic ring
contains error codes and run IDs, not provider response bodies, inputs or secrets.
`completed_drives` counts attempts to drive, including rejected attempts; it is
not a business delivery metric. `last_completed_run` means that attempt returned.

Stop first queries the instance and sends a generation-bound request. A stale
request cannot stop a replacement service. `stop_requested: true, stopped: false`
acknowledges draining; poll until `stopped` to confirm termination. One previously
admitted drive processes at most one command/provider call. Adapters must honor
their deadlines; a stuck synchronous adapter is not forcibly killed or reported
drained. An OS supervisor may terminate it, after which leases and effect query
recovery handle the orphan. Do not signal a PID copied from a stale status file.

## Ownership and scheduling

The control directory must be owned by the local user and exclude group/other
access (0700). The lock and socket are private; real directory components and a
short Unix socket path are required. An exclusive OS lock prevents two listeners
from sharing a control directory. Only its new lock holder removes a stale socket;
regular files and symlinks at the endpoint are rejected. These controls are a
trusted local host boundary, not remote authentication or tenant isolation.

Two services configured with different control directories can target the same
database. The existing transactional run lease remains the execution authority;
only one lease is valid. A busy lease or stale result is a diagnostic, never an
invocation grant. Pause stops new work and timer advancement; resume retains the
original deadlines. The daemon never clears a restore reconciliation barrier.

Each scan reads a bounded page of up to 100 runs and continues its cursor on the
next scan. Each eligible run gets at most one command per visit. An idle wait does
not acquire leases repeatedly; its persisted deadline is checked on later scans.
Callbacks enter through the durable trusted Inbox, so their resulting work is
found without an in-memory notification. Errors and uncertain effects receive a
bounded retry delay, with changed run revisions reconsidered immediately. This is
local round-robin scanning, not cluster quota/fairness or parallel worker capacity.

Database unavailability prevents admission/commit and is exposed as an error.
After process suspension, the next scan observes actual wall time and due timers;
deadlines are not extended. If suspension happens while SQLite holds a transaction
lock, other local queries can return `Busy`. Recovery still verifies state and
does not pretend a failed query succeeded. Keep database/artifact files in place
while the service is active; use stop and verified backup/restore when relocating.

## Reproducible offline example

Build once, then run either explicit local operator choice:

```sh
cargo build --locked -p workflow-cli
python3 examples/execution/offline-demo.py --workflow target/debug/workflow --decision approve
python3 examples/execution/offline-demo.py --workflow target/debug/workflow --decision reject
```

The example creates a fresh temporary database and starts the actual daemon. Its
immutable bundle uses real `workflow.validate` calls, exclusive branching, a
bounded loop, a subworkflow, parallel control-flow with an all-join and an explicit
human wait. Local adapter calls are sequential. The loop succeeds in its first
iteration; bounded retry/exhaustion is separately covered by kernel tests.
The script submits only the chosen local operator response and checks the two
actual committed task results, run terminal, replay proof and stopped service.
It invokes no model and reports no model quality or production throughput claim.

## Export

`run --artifacts <store> export <db> <new-directory>` exports the entire selected
run database and verified artifact dependencies as the existing backup format.
Omit `--artifacts` for runs without dependencies. Immutable run bundles, versions,
waits, effects and provenance are included; draft registry history requires
`backup create` with its explicit registry source. Export does not authorize a
second active owner. Restoring this format still fences leases and applies the
external-effect recovery barrier; controlled live migration remains R10.

## Failure boundary and retention

The process tests force kill/restart, suspend/resume across a deadline, two CLI
resume/acquire races, stale stop requests and listener contention. Paused callback
recovery, transaction kill points, SQLite-full rollback, missing/corrupt backup
files and restored provider-effect reconciliation have their own existing tests.
See [backup recovery](../04-effects-and-recovery/04-backup-recovery.md) for path relocation, RPO and the explicit
external-effect audit; process restart does not recover an unbacked lost disk.

The local retention policy currently keeps all accepted run/event/effect/Inbox
history, immutable definitions, committed artifacts and backups. There is no
automatic deletion, checkpoint truncation or compression command. Existing size
and event limits reject further admission rather than silently discarding active
recovery data. Stop the owning process before removing abandoned private backup
staging; never infer abandonment from elapsed wall time alone. Lifecycle retention
and compression beyond this conservative policy remain separate R04 work.

Validated host: Ubuntu 24.04.4, Linux 7.0.0-31-generic x86_64, ext4. A separate
Ubuntu 24.04 container with networking disabled, a read-only root filesystem,
all Linux capabilities dropped and no-new-privileges also passed the actual
CLI/daemon/approval example. It used a fresh writable bind directory on host ext4
and shares the host kernel; it is not a separate physical machine. Its image
and raw measurements are retained with the MR evidence. The supported local target is Linux x86_64 with a compatible glibc;
the host-built binary is not compatible with Debian 12's older glibc. Local process
failure tests do not establish power-loss durability, network-filesystem safety,
hardware disaster recovery, or a multi-machine SQLite deployment.

See the [R08 acceptance matrix](../06-acceptance-and-maintenance/05-local-acceptance.md) for exact checkers and platform limits.

<!-- book-navigation -->

3.3 Local daemon

[Book contents](../README.md) · [3. Execute and verify](README.md) · [中文](../../zh/03-execution-and-evidence/03-local-daemon.md) · [Previous: 3.2 Local execution](02-local-execution.md) · [Next: 3.4 Bounded model execution](04-model-execution.md)

<!-- /book-navigation -->
