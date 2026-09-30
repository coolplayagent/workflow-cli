# Shared database recovery and R04 acceptance

PostgreSQL remains the shared authority. Its run images contain frozen definitions,
state/events/checkpoints, Outbox/receipts, Inbox/waits, execution attempts, model
records and effect history. The same database also holds shared artifacts and
their bytes, immutable version locks, publication records, scoped credentials,
assignments and audit. A database restore must preserve these together.

Restoring a database alone is insufficient: it can resurrect old credentials,
leases and dispatched work while losing a later external receipt. The trusted
offline `service fence-restored` operation verifies every run and artifact
dependency and creates a held recovery generation before the destination is
opened to clients.

## Consistent archive and isolated restoration

The acceptance example uses a full custom-format PostgreSQL archive and restores
it into a fresh, isolated database. `pg_dump` produces a consistent snapshot of
one database; it does not include cluster-wide roles or tablespaces. Its custom
archives are consumed by `pg_restore`. See the official [pg_dump documentation](https://www.postgresql.org/docs/current/app-pgdump.html).

Use a trusted source, compatible PostgreSQL tools, host-managed connection
secrets, and a new destination. An example with preconfigured libpq services is:

```sh
umask 077
pg_dump --format=custom --file=workflow.dump --dbname='service=workflow-source'
sha256sum workflow.dump
pg_restore --single-transaction --exit-on-error --no-owner --no-privileges \
  --dbname='service=workflow-isolated-restore' workflow.dump
```

The destination database and its owner must already exist. The restore options
make the application archive load one transaction and stop on errors; they do
not recreate deployment users/privileges. See [pg_restore](https://www.postgresql.org/docs/current/app-pgrestore.html).
Retain the archive, its verified checksum, snapshot/capture time and compatible
application release using the host's durable backup storage. This example is a
bounded logical recovery drill, not a replacement for a deployment's physical
backup, WAL/PITR, replication or archive-retention configuration.

Do not start the destination service yet. Its host-owned server binding must
point at the isolated database. Prepare a request with the destination's exact
database name, the archive's actual SHA-256 digest, operator and reason:

```json
{
  "database": "workflow_isolated_restore",
  "backup_digest": "sha256:ACTUAL_ARCHIVE_DIGEST",
  "actor": "recovery-operator",
  "reason": "Restore isolated destination after source storage loss"
}
```

```sh
workflow service fence-restored restored-server.json restore-request.json new-administrators.json
```

This is a trusted database-host operation, absent from the public RPC protocol.
The supplied name must equal `current_database()`. The archive digest is a host
provenance annotation; full replay validates the restored application state, not
the identity or authenticity of an arbitrary SQL archive. The caller must have
selected the intended trusted archive and isolated destination.

## Atomic recovery transaction

The operation locks the authority, credential, assignment and artifact tables,
uses synchronous commit, and performs these steps in one database transaction:

1. Verify supported schema versions and bounded inventory (at most 1000 runs and
   1000 tenant/project scopes). Verify each image digest, complete replay,
   checkpoint/history agreement, immutable bindings, artifact bytes and lineage.
2. Revoke every retained credential, including administrators. Fence retained
   unfinished task/effect assignments without deleting their history.
3. Append a fresh random ownership generation and backup-bound recovery barrier
   to every run. Release old ownership. Running runs become paused; existing
   pauses, wait deadlines, loop rounds, retry budgets and terminal states remain.
4. Issue a fresh administrator for each retained scope and append restoration
   audit entries. Confirm the transaction before returning the report.

A corrupt dependency or failed write rolls back the whole operation, including
changes to earlier runs and credential replacement. Successful finished runs keep
their business state and evidence without executing their tools again. Restored
images retain their original history plus explicit recovery journal entries.

The CLI delivers the new administrators in one exclusive 0600 file; stdout has
only counts, destination, backup and generation metadata. Each administrator
retains the existing one-hour bootstrap lifetime. Use the existing scoped
provisioning mechanism to issue replacement runner, viewer, recovery, scheduler
and worker credentials, then start the destination service. Never reuse source
worker credentials. Tokens are absent from the ordinary report and Debug output.

If private-file delivery fails after the transaction commits, the command returns
an error and the database may already be held. Keep it isolated. Repeating the
operation with a new output path creates another held generation and fresh
administrators, revoking the previous recovery credentials. It does not reset
history or execution budgets. This is intentionally a fresh recovery operation,
not an automatic replay after an ambiguous result.

## External effects and resumption

Use `remote call` with `{"type":"recovery_barrier","run_id":"..."}` to inspect
the exact barrier. Resuming a run permits reconciliation and read-only progress;
it does not authorize a new external write. Old credentials and old leases cannot
complete work in the restored destination.

| Retained evidence | Recovery action |
| --- | --- |
| Original intent retained; outcome uncertain | Query the provider under a successor lease using the existing effect protocol. Missing observations cannot imply success or permit blind repetition. |
| A source admitted a write after the backup | Obtain the actual source intent and provider receipt. A recovery credential acquires its own live lease and calls `import_restored_effect` with `lease` and `request: {intent, resolution}`. |
| Original source/provider history unavailable | Keep the barrier. An absent database receipt is not evidence that the provider did nothing. |

The import uses the existing `run-restored-effect` schema. The service replaces
the submitted resolution actor with the authenticated recovery actor. The reducer
checks the frozen task/input/policy/key and exact provider receipt; a changed
intent is rejected without state change. Duplicate imports return the original
completion, after current credential and lease checks. Importing never calls the
provider or invents the missing original attempt history.

After actual source retirement and provider audit, `acknowledge_recovery` accepts
the existing `run-recovery-acknowledgement` document, bound to the exact backup and
generation. Its `no_missing_effect_intents` assertion belongs to the recovery
operator; it is not inferred by the model or service. Known unresolved effects
still block acknowledgement. Clearing this barrier does not resume a paused run.

The new generation protects the restored database. It cannot stop a surviving
source service or revoke that source's provider credentials. Source retirement
and provider fencing remain deployment responsibilities; controlled migration
and ownership handoff are R10 work.

## Executable evidence

```sh
# WORKFLOW_TEST_POSTGRES must name the disposable database used by the container.
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo build -p workflow-cli --locked
python3 examples/remote/database-recovery.py target/debug/workflow --container POSTGRES_CONTAINER_ID
# On a Podman host, also pass --container-engine podman.
```

The CLI drill creates two unique databases and a provider gateway outside both.
It publishes an assignment-derived artifact, takes a full database snapshot,
applies one real fixture-provider write after the snapshot, stops the source,
restores/fences the destination, and verifies the original completed state and
artifact bytes. Old credentials fail. New write admission remains blocked until
the actual post-backup intent/receipt is imported. Wrong-role and tampered imports
fail; duplicate imports preserve one completion and **one provider write**. The
authenticated actor replaces payload actor claims. The final acknowledgement
occurs only after the fixture source is stopped and its sole write is accounted
for. Both databases and all child services are cleaned up.

The Rust restore contract additionally corrupts a late artifact and proves that
earlier run changes and all credential revocations roll back. It checks old
lease/result rejection, cross-tenant credential replacement, completed state,
artifact retrieval and repeated restoration without changed deadlines/pauses.

| R04 criterion | Evidence |
| --- | --- |
| Kill before/during/after commit; retain successor Outbox | SQLite `killed_processes_never_leave_partial_state_or_lose_committed_outbox`; PostgreSQL `real_postgres_parity_fencing_atomicity_and_outage_contract` and HTTPS scheduler/worker kill contracts |
| CAS, duplicate events/completion, cancel race | SQLite independent-process and final-completion/cancel tests; PostgreSQL assignment settlement and fencing tests |
| Waits, timers and bounded loops survive restart | SQLite scenario/checkpoint tests; R06 PostgreSQL/HTTPS service restart contract preserves wait identity, subjects and original deadlines |
| No re-execution of committed work; unknown effects reconciled | R02 model record restart matrix; R05 query-first HTTP crash fixture; post-backup provider receipt import in this drill |
| Checkpoint/tail equals full replay; corrupt/missing records fail | SQLite periodic-checkpoint, journal corruption and full replay tests; PostgreSQL run-image/binding checks; restored artifact corruption rollback |
| Full/unwritable storage cannot confirm success | SQLite real full-disk/read-only tests; PostgreSQL transaction failure/connection-loss contract; synchronous authority writes |
| Backup validates active runs, history and artifacts; RPO/RTO defined | [Local backup recovery](backup-recovery.md), the complete PostgreSQL archive drill, and the atomic restore contract above |

These contracts run in the repository's Rust, Bazel and disposable PostgreSQL CI
jobs. Process crash/restart is distinct from loss of all storage. The backup's
snapshot defines its RPO: later committed database state may be absent and later
external effects require reconciliation. There is no RPO=0 assertion for a
restored archive. The drill reports archive bytes, dump time, time to verified
held state and the binary digest. Human/source/provider reconciliation time is
additional business RTO; the small fixture timings are not production guarantees.

Committed artifacts and active recovery dependencies are retained. Existing
cleanup removes only eligible incomplete transfers, never committed artifacts or
run history. Object-storage lifecycle policy and controlled live migration are
separate R07/R10/R14 concerns.
