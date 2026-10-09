# Local backup and fenced recovery

`workflow-backups` defines the portable inventory and reader contract.
`workflow-backup-local` creates verified SQLite images and copies immutable artifact
content. Both are Rust libraries with explicit Cargo/Bazel targets. Publication
currently requires Linux, `/dev/urandom` and filesystem support for atomic
`renameat2(RENAME_NOREPLACE)`.

## Contents and snapshot boundary

A backup contains `runs.sqlite`, optional `registry.sqlite`, optional
`artifacts/catalog.sqlite` plus retained content objects, and `backup.json`.
The run image includes frozen definitions, version locks, events, Inbox entries,
checkpoints, command Outbox/receipts, leases, attempts, gates, model records and the
effect ledger. The optional definition registry also preserves unpublished and
deleted draft history. Artifact manifests retain their original ownership,
producer, source revision, input lineage and content digests.

SQLite's backup API copies a pinned read transaction, including committed WAL
content; copying a live database file is not the protocol. Each database is checked
with SQLite integrity and foreign-key checks, then its application-level verifier.
The run image is captured first. Artifact content and manifests are immutable and
retained, so the later artifact catalog may be a superset while containing every
committed run dependency. Full run replay against the copied artifacts proves
closure before publication. Registry snapshots are separate: each run already
contains its exact frozen executable definitions.

Every listed file has a byte count and SHA-256 digest; the index binds its inventory
and each run's identity, revision and state digest. Extra files, missing bytes,
wrong digests, unsupported versions, inconsistent histories and symlinks reject.
Inventory paths come from a fixed layout. Digests establish integrity, not source
identity or authorization. The host owns the source and destination directories;
hostile replacement by another process with the same OS permissions is outside
this adapter's boundary.

A new private staging directory uses modes 0700/0600. Data and directory entries
are synced before an atomic publication that refuses any existing destination,
including an empty directory created concurrently. Failure before publication
leaves no usable destination. Interrupted private staging directories are never
automatically accepted as backups. Inspect and remove abandoned staging only when
its creating process is known to have stopped; no cleanup deletes a live run,
committed artifact or completed backup.

The bounded format accepts up to 256 MiB per SQLite image, 1 GiB total, 10000 runs,
10000 artifact manifests, 10003 payload files and an 8 MiB index. Whole-registry
verification is bounded to 10000 retained revisions and 10000 publications. These
are explicit local product limits. Backup includes the selected stores; it does
not capture uncommitted workspaces, arbitrary repository files, environment
credentials or host adapter configuration.

## CLI

Create a source file with the intended existing paths; paths resolve from the
current directory, independently of the JSON file's location:

```json
{
  "runs": "/path/to/runs.db",
  "artifacts": "/path/to/artifacts",
  "registry": "/path/to/definitions.db"
}
```

Use `null` for an optional store that is absent. Omitting required artifacts does
not disable dependency verification; it rejects the backup.

```sh
workflow backup create sources.json /new/backup-directory
workflow backup verify /new/backup-directory
workflow backup inspect /new/backup-directory
workflow schema backup-index
```

`create` and `verify` return the digest, counts, bytes and snapshot time range.
`inspect` returns the full verified inventory. No source worker is stopped by a
backup. New source commits can continue after the captured run snapshot.

For restoration, prepare a request identifying the actual local operator and
reason. This annotation does not authenticate a remote user:

```json
{"actor":"local-operator","reason":"recover after source storage loss"}
```

```sh
workflow backup restore /new/backup-directory /new/restored-directory restore-request.json
workflow run --artifacts /new/restored-directory/artifacts status /new/restored-directory/runs.sqlite my-run
workflow run --artifacts /new/restored-directory/artifacts recovery /new/restored-directory/runs.sqlite my-run
```

Omit `--artifacts` when the backup has no artifacts. Restore publishes
`runs.sqlite`, any copied artifact/registry stores, and `restore.json` only after
verification and ownership fencing. It never overwrites the source or merges into
an existing database. If the reply is lost, inspect the destination's run state,
recovery barrier and retained restore report before retrying to a new location.

## Restored ownership and external effects

Every restored run receives a fresh random ownership generation bound to the
backup digest and exact source snapshot. A journal record releases the old lease;
new leases retain increasing epochs and also carry the new generation. Even an
old source lease issued *after* the backup with an otherwise identical owner,
acquisition ID, epoch and timestamp cannot commit to the restored store.
History and retained retry budgets are not reset. Running runs start paused;
already paused runs keep their original pause reason and deadlines.

The restore also installs a durable recovery barrier. An explicit resume may
advance read-only work, timers and queries, but cannot admit a write while the
barrier exists. Resuming is not an assertion that external effects are absent.

Two external windows require different handling:

| Backup evidence | Recovery |
| --- | --- |
| Intent is present, receipt is missing | Use the retained intent/key to query the actual provider through `drive-effects` or the effect host API. Query absence cannot fence a surviving old writer. No new write is allowed under the barrier. |
| Source admitted an effect after the snapshot | First audit/quiesce the source and obtain its original intent and actual provider receipt. `run effect-import <db> <lease.json> <import.json>` validates the frozen task, inputs, policy, key, original receipt and dependency order, then atomically records the Applied fact and task transition. |
| Original intent or outcome cannot be established | Keep the barrier and investigate through the provider/source audit. Missing history is not evidence that a new write is safe. |

`schema run-restored-effect` exports the import format. It carries the actual
`EffectIntent` and a `ManualResolution` with Applied receipt, stable resolution ID,
actor annotation, reason and evidence. Import is available only during restored
reconciliation under a live lease, for the next pending managed effect. It makes
no provider call and does not invent lost attempts; imported effects have an empty
local call list and explicit import provenance. Identical imports are idempotent;
changed content conflicts. Never manufacture an original timestamp or receipt to
make an import pass.

After actual source retirement and provider audit, export
`schema run-recovery-acknowledgement` and submit:

```sh
workflow run recovery-acknowledge /new/restored-directory/runs.sqlite my-run audit.json
```

The audit binds the exact generation/backup, resolution ID, actor, reason and
evidence. `no_missing_effect_intents: true` is the operator's explicit attestation
that every post-backup admitted write has been accounted for. Known unresolved
effects still reject acknowledgement. Do not assert this when source/provider
history is unavailable. The response names the acknowledged generation and any
remaining barrier; retrying an old acknowledgement never clears a later restore.
No new call allowance is granted to a retained effect. Calls/history lost after
the snapshot remain unknown; this protocol does not assert their costs or counts.

Restore fencing protects the new database. It cannot retire a still-running source
service or fence its external provider by itself. Controlled migration with an
atomic source ownership handoff is separate R10 work. This is disaster recovery
with an explicit reconciliation hold, not permission to activate two copies.

## Storage, evidence and recovery objectives

Schema 10 introduced restore generations, barriers and imported receipts.
Current schema 12 uses the explicit backed-up [migration](05-version-migration.md)
for versions 1–11; older readers refuse newer storage. An actual v9 binary migration test retains paused Inbox data and a
prepared effect's lease/history exactly. Restore generation fields are absent
from old records, preserving their digests.

Tests cover:

- Relocation of gated artifacts, published/deleted definition history and pending
  paused Inbox entries; existing gates complete without rerunning the settled task.
- Actual provider write after the backup, both with and without a retained intent;
  query/import recovery leaves exactly one provider object and one write.
- An old post-backup lease with identical epoch/owner/timestamps, repeated restores
  and replay of an earlier acknowledgement.
- Seven process-kill points across snapshot, fencing and publication, plus actual
  child-process file-write quota failure; no incomplete destination is published.
- Source commits after snapshot capture, destination races, missing/corrupt files,
  symlinks, extra files and rehashed but inconsistent registry history.
- Real CLI backup/restore and refusal of writes until an explicit provider audit.

The captured run snapshot defines the backup RPO. Later database commits may be
lost after disk failure; external effects require the recovery steps above. There
is no RPO=0 claim. File verification and fenced restoration establish a recoverable
held state; human/provider reconciliation time contributes separately to business
RTO. Local process termination is tested, not arbitrary hardware power loss.

Five small CLI samples on the development Linux host (16 logical CPUs; statfs
label `ext2/ext3`) used two runs, one pending simulated callback and one deliberately
undispatched intent, without artifacts or external calls. Median elapsed times,
including process startup, verification and syncing, were 95.5 ms for backup,
43.3 ms for verification and 211.7 ms for restoration to the held state. These are
fixture observations, not production capacity or recovery guarantees. Full sample
metadata is retained with the MR evidence.

Shared PostgreSQL restoration is covered by the [shared recovery and R04
acceptance guide](../05-shared-execution/07-shared-recovery.md). Encrypted/signed archive distribution,
deployment archive retention and source-to-target ownership transfer remain
separate deployment and roadmap work.

<!-- book-navigation -->

4.4 Local backup and recovery

[Book contents](../README.md) · [4. Effects and recovery](README.md) · [中文](../../zh/04-effects-and-recovery/04-backup-recovery.md) · [Previous: 4.3 Ordered compensation](03-ordered-compensation.md) · [Next: 4.5 Version and storage migration](05-version-migration.md)

<!-- /book-navigation -->
