# Version locking and explicit migration (R11)

Publication never changes an existing run. The original StartRun, canonical
bundle, workflow/subworkflow versions, capability contracts, model policies and
templates, postcondition policies and approval policies remain retained. A new
version receives a new immutable binding. Reusing a version with different
content is rejected, including conflicts with other runs in the same shared
tenant/project. Hosts select an explicit published bundle for each new run.

Worker dispatch requires the exact capability version, descriptor digest and
model-policy binding. An incompatible worker cannot consume the task. Keep a
compatible worker and its required artifacts, credentials and environment until
all dependent runs finish, or drain those runs before retiring it. If the old
implementation is unavailable, pause and handle the run explicitly. A version
label is a host contract, not an attestation of an arbitrary executable. The
workspace executor additionally records and checks the actual executable digest
for an existing attempt; see [workspaces](workspaces.md).

History replay reduces recorded results; it does not call a model, worker or
provider. Running a tool again requires a new instance/attempt and the applicable
effect policy. Retain the original and all migration target bundles, artifact
dependencies, registry releases and compatible interpreters for the run's entire
lifecycle. Current storage has no automatic deletion of these dependencies.

## Definition migration

`MigrationStore` exposes a preview and a protected administrative commit. The
only implemented execution policy is `restart_with_fresh_evidence`: restart the
target root from explicit target inputs, allocate fresh monotonically increasing
node instance IDs, and recompute every task, gate and approval. Completed results
have **no automatic reuse condition** under this policy. The node mapping is an
impact review, not a request to resume at a mapped node. Unsupported policies and
unknown fields are rejected.

The source must be paused and still running. The reviewed plan contains:

- source revision, full state digest and original run identity;
- source/target bundle digests and the complete canonical target bundle;
- suggested mappings with explicit overrides, removed/added nodes, definition,
  gate and approval-policy changes;
- target inputs, input-change flag and every old instance's input/output and
  gate-decision digest;
- invalidated callback IDs and old timer deadlines mapped to target durations;
- explicit execution/timer policies, stable migration ID and bounded rationale.

Mappings must reference existing nodes and cannot merge two old nodes into one
target. Every commit recomputes the complete plan against the current source;
editing the impact list or applying a stale plan is rejected. All old results
are invalidated even if inputs or node definitions happen to compare equal.
Gate/approval binding identity changes are conservatively reported as changes.

`cancel_and_rearm_on_resume` cancels old timer intents. The target remains paused
until an explicit resume; each reached target wait/loop starts its declared
duration at that time. This is a reviewed deadline reset. Ordinary pause/resume
without migration still preserves the original deadlines.

Pending old Inbox messages become `definition_mismatch` rejections in the same
event. Accepted/rejected old records remain historical facts. Old instances are
never reused, so old approvals/evidence cannot authorize the target. The shared
callback endpoint refuses a target with no current authenticated wait policy;
local durable ingress can record the old target as a rejected message.

The migration transaction retains target definitions/bindings, commits the kernel
event, retires every prior pending outbox command with migration-specific
receipts, and appends a fenced execution proof with the actor and plan digest.
It changes the ownership generation and releases the lease. A new owner must
acquire a new lease before executing target work. Live current attempts must
drain; stale readonly attempts are fenced. Their historical proofs are verified
against their original definition and state, including after a target removes
the old node or capability. `history-at` reconstructs the locked historical state.

Any recorded business-effect ledger entry or outstanding restore barrier blocks
this restart policy. This includes reconciled/failed effects: do not clear the
ledger to force migration. Finish the old run, or explicitly reconcile a separate
new run and its external-effect policy. Changing a definition or restoring a
database does not reverse a release, payment or other external write.

Repeated `(migration_id, complete plan, actor)` returns the prior logical commit;
changed content or actor conflicts. Lease expiry during local or final shared
commit admission rolls back the operation. A replayed migration event without
exactly one matching execution proof is corrupt. Raw `run event` cannot submit a
definition migration.

## Local commands

Pause the run using its current revision, then write a request matching
`workflow schema run-migration-request`. Use an immutable target bundle and
explicit input values. CLI JSON responses contain their value in `result`.

```sh
workflow run migration-plan runs.sqlite run-id request.json
# Save and review result as plan.json. Acquire a current administrative lease:
workflow run acquire runs.sqlite lease-request.json
# Save result as lease.json; filesystem/host access is the local trust boundary.
workflow run migration-apply runs.sqlite lease.json plan.json operator
workflow run history-at runs.sqlite run-id 7
workflow run verify runs.sqlite run-id
```

The run is still paused after commit. Inspect the target, acquire a fresh lease
for execution and explicitly resume using its new revision. The same immutable
plan may be retried after an uncertain response. Do not regenerate a different
plan under an already used migration ID. Supply `--artifacts` (or the configured
object reader) whenever retained history contains artifact dependencies.

## Shared administrative operations

The HTTPS protocol exposes `plan_migration`, `migrate_definition` and
`historical_snapshot` through `workflow remote call`. A definition maintainer
must publish the target bundle first. Only an administrator in the same scope
may plan/apply a migration. Administrators can acquire/release their own run
lease; this permission does not grant task dispatch or start authority.
The service verifies the lease owner against the authenticated credential,
stamps the authenticated actor and uses PostgreSQL time. No claimed actor or
client clock is accepted. Run image, global bindings, old assignment retirement
and bounded audit entries commit together. Regular readers can request a
historical snapshot; administrators can also use that operation for review.

## Independent storage upgrades

SQLite and shared transaction images now use application schema **11**. Logical
StartRun remains schema 1; worker and service wire protocols retain their existing
versions. Schema 11 protects definition-migration events and storage-upgrade
journals from older interpreters. Ordinary open/create refuses older/future
storage; it never upgrades as a side effect of running work.

Local upgrades accept schemas 1–10. Schema 1 receives empty execution-authority
heads; later versions preserve existing authority. Preflight reconstructs a
consistent snapshot in memory, verifies every run, event, checkpoint, outbox,
receipt, execution proof and artifact dependency, and produces a source digest
and verified-history digest. No tools or effects run during this verification.

```sh
workflow run storage-plan old.sqlite
workflow run migrate old.sqlite before-upgrade.sqlite
workflow run storage-history old.sqlite
```

`migrate` exclusively creates a consistent SQLite backup (0600 on Unix), verifies
its conversion, syncs it, and checks the same source digest inside the upgrade
transaction. A source change after backup/preflight aborts the upgrade. Retain the
backup and retry with a new backup path. The source version, new table, migration
record and conversion commit atomically. The record contains source/target
versions, source digest, verified run count and verified-history digest. Bounds:
256 MiB per local database snapshot and 10,000 runs per upgrade. Dependency
failure or corruption leaves the original schema intact. `run migrate <db>` with
no backup argument only verifies an already-current store.

Save the returned storage-upgrade record as `upgrade.json`. With all owners
stopped, restore to a new path and use the retained compatible binary:

```sh
workflow run storage-restore before-upgrade.sqlite restored.sqlite upgrade.json
/path/to/retained-v10-workflow run verify restored.sqlite run-id
```

Restore verifies the backup and copied result against the reviewed record and
preserves its old schema. It refuses an existing destination. This path is for
offline storage rollback while owners are stopped; it does not create the
[fenced recovery barrier](backup-recovery.md) required after lost external work.
Retain/copy the referenced artifact catalog and content as well as the database.
If execution or external writes occurred after the backup, use the existing
fenced recovery/reconciliation procedure instead of reopening an old owner.

Shared images upgrade **one run at a time**, from image schema 10 to 11. A scoped
administrator calls `plan_storage_upgrade {run_id}` and then
`upgrade_storage {run_id, plan}`. The source-byte digest is checked under the
aggregate row lock. Complete replay, global binding and artifact verification
precede replacement of the image; a mismatched plan leaves it byte-identical.
Exact repeated plans are idempotent. This conversion preserves the logical
definition, current leases and recorded work; it is not a definition restart.
New hosts refuse old images until this explicit operation. Old hosts refuse new
images, so route requests to compatible hosts during rolling deployment.
The outer PostgreSQL authority/access schemas have their own versions and remain
unchanged. Take a verified [shared backup](shared-recovery.md) before the batch;
after lost writes, use its credential/ownership fencing and reconciliation path.

## Executable acceptance

| Issue criterion | Direct evidence |
| --- | --- |
| Publish v2 while v1 waits; old resumes v1, new uses v2 | `published_v2_does_not_move_waiting_v1_and_workers_route_by_exact_contract`; real registry/CLI flow in `examples/migrations/acceptance.py` |
| Reject or route incompatible capability versions | Same PostgreSQL contract rejects a v2 worker for v1, executes the real v1 builtin, and routes the v2 request to the exact v2 credential |
| Review removal, input/gate and timer impacts | Kernel migration tests, local gate test, shared administrator test and saved CLI plan assertions |
| Crash recovery and idempotence | Independent processes killed at eight definition commit boundaries, two-process duplicate race, four storage commit boundaries, lease-expiry rollback and stale-plan rejection |
| Do not reuse evidence/approval | Previously passing gate and completed task replay as history; old WorkResult and old producer evidence fail the new attempt; fresh evidence passes. Kernel/local/shared tests reject old approvals and invalidate paused Inbox messages |
| Failed storage upgrade and locked interpretation | Full journal/dependency validation; local backup/restore and corrupt-upgrade rollback; byte-identical failed shared image upgrade; historical state lookup and retained v10 executable CLI check |

```sh
cargo test -p workflow-kernel migration --locked
cargo test -p workflow-runstore-sqlite migration --locked
WORKFLOW_TEST_POSTGRES='host=127.0.0.1 ...' \
  cargo test -p workflow-runstore-postgres migration --locked -- --ignored --test-threads=1
cargo build -p workflow-cli --locked
python3 examples/migrations/acceptance.py target/debug/workflow
WORKFLOW_TEST_POSTGRES='host=127.0.0.1 ...' \
  python3 examples/migrations/acceptance.py target/debug/workflow --https \
    --legacy-binary /path/to/retained-v10-workflow
```

CI mandates the local CLI drill and the HTTPS/PostgreSQL drill. The optional
retained-binary check was also run locally against the R07 CLI; CI's baseline
builds a schema-10 fixture with the same old table layout. Process-fault tests
run in the workspace/Bazel suites. These are bounded fixture measurements, not
production upgrade reliability or business benefit estimates. Migration events,
execution proofs and shared audits provide durable outcome/actor counts; record
host elapsed time separately when measuring operational migration latency.
