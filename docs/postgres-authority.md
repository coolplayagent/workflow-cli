# Shared PostgreSQL run authority

`workflow-runstore-postgres` implements RunStore, ExecutionStore, InboxStore,
EffectStore and RestorationStore. The authenticated host supplies a PostgreSQL
client and the server-derived tenant/project identity. This library does not
expose a public HTTP ingress, authenticate workers, or implement the R09 scheduler.
The caller configures database authentication and TLS on `postgres::Client`.
`PostgresRunStore::initialize` explicitly creates the dedicated schema; `open`
requires the exact supported version and never initializes or upgrades silently.

Each run is one bounded transactional aggregate in PostgreSQL. A mutation locks
that run's row, verifies its content digest, reconstructs a volatile SQLite
reducer, and replays state and execution journals. Only the compiled SQLite
schema is installed: the image contains typed rows, not SQL or native pages.
The existing reducer preserves task/gate/effect, receipt, Inbox and recovery
rules. This avoids maintaining two different state transition implementations.
The resulting image becomes authoritative only after a PostgreSQL transaction
commits; an in-memory reducer result is never returned as durable success.

Run state, events, outbox, execution/effect journals and the immutable version
catalog commit together. Same-scope workflow/capability/gate/model/effect versions
cannot bind different content across runs. Primary database time supplies lease,
attempt, timer, effect and callback admission checks; caller clocks cannot extend
authority. Lease/request/gate deadlines are retained from the reducer and checked
again by the final conditional PostgreSQL update, after serialization and catalog
locking. New callback transitions also retain their exclusive admission deadline.
The row lock serializes ownership, and a generation predicate additionally guards
the update. Database errors and unknown commit outcomes return an error. The host
must reconcile using stable operation identities instead of assuming rollback.

Read operations use a repeatable-read snapshot, replay the aggregate and verify
its shared version bindings. Pagination uses deterministic byte ordering and loads
one bounded aggregate at a time. A corrupt or incomplete row fails status/list;
it is not silently skipped. Database errors expose bounded generic diagnostics,
not connection strings, statements, parameter data or server error detail.

## Verification and operational boundary

The `postgres` CI job uses a disposable PostgreSQL 17 service and explicitly runs
the otherwise ignored integration test:

```sh
WORKFLOW_TEST_POSTGRES='<disposable connection string>' \
  cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture
```

The current contract exercises independent client processes contending for a
lease, expired epochs, real builtin execution through the existing runtime,
human approval/deduplication, immutable version conflicts, scope partitioning,
reopen/replay, a synthetic effect receipt, reducer rollback, final-write expiry,
read-only transactions, terminated database connections and corrupt rows. Synthetic
receipts verify storage semantics, not behavior of an actual release provider.
Image tests check journal round trips, removed execution records, mismatched run
identity, malformed columns, size bounds and multiple-run rejection.

Images are limited to 64 MiB per run and use SQLite application schema 11 within
PostgreSQL schema 1. Each mutation rewrites and verifies the aggregate. This has
deliberate memory, CPU and write-amplification costs; throughput has not been
claimed. Large histories require a separately verified storage evolution. The
library bounds SQL statement, lock and idle transaction waits and uses synchronous
commit; database connection establishment timeouts are the caller's responsibility.

The database client is a trusted service credential. Tenant/project keys provide
partitioned data access through this adapter; they do not establish R14 identity,
artifact ACLs, secret rotation or protection from direct database access. Artifact
verification is supplied by the host and must retain immutable referenced content.
Local image export does not revoke an old owner or authorize migration. Full
database recovery must retire the old authority and apply recovery fences before
admitting writes; [R04 recovery](shared-recovery.md) provides that operator protocol.

The [authenticated HTTPS service](remote-service.md) adds task transport and a
two-scheduler/three-worker owner-loss fixture above this storage boundary. R09
remains open for fairness/quotas/backpressure, worker drain, rolling upgrades,
broader queue/network failure, database outage and disaster recovery measurements.
[R14 security acceptance](security-acceptance.md) defines the authenticated admission
boundary above this raw trusted port. Local SQLite and
PostgreSQL storage contracts alone do not prove those deployment properties.

Implementation references: [PostgreSQL row locking](https://www.postgresql.org/docs/17/explicit-locking.html)
and the [Rust PostgreSQL client](https://docs.rs/postgres/0.19.14/postgres/struct.Client.html).

The authenticated host application facade is documented in
[Authenticated PostgreSQL authority](authenticated-authority.md). It derives
scope/actor from database-backed bearer identity and authorizes operations in the
same transaction as run changes. The raw `PostgresRunStore` API remains trusted.

Previous schema-10 images require a source-bound, administrator-authorized
`plan_storage_upgrade` / `upgrade_storage` operation. Definition migration is a
separate paused-run CAS operation; see [R11 migration](version-migration.md).
