# Typed artifacts and verified run evidence

Workflow treats a report or file as a versioned handoff. `workflow-artifacts`
defines the portable contract and reader/store ports. `workflow-artifact-local`
implements durable local files with a transactional SQLite manifest catalog.
Neither depends on the worker, kernel or run database. The RunStore adapter takes
an `ArtifactReader` and validates references before accepting a worker result and
during recovery. Each module has an explicit Bazel `rust_library`.
See [R07 acceptance and object/workspace operations](../06-acceptance-and-maintenance/04-artifact-acceptance.md) for the completed cross-adapter workflow.

## A real report through the CLI

```sh
cargo build --locked
python3 examples/artifacts/record-validation.py "$PWD/target/debug/workflow" /tmp/workflow-artifact-demo
```

Choose a new output directory. This executable example reads the committed
validation input from the current Git revision, starts a run, acquires a durable
lease, claims a task, invokes the actual built-in worker, publishes its typed
validation report, and finishes the task with verified evidence. It also exports
and imports the report into a second local store and verifies the same run there.
No model, network account or external write adapter is involved. Source revision
identifies the inspected committed input, not every uncommitted development file.

The intermediate JSON files make the host protocol inspectable. `run acquire`,
`renew`, `claim`, `tick-due`, `finish`, `attempt-failed` and `release` expose the
existing execution port. Supply exact lease/request/grant/attempt identities from
the committed responses; never invent them. `worker dispatch` performs the call;
`run finish` performs the fenced, atomic result/event/state/receipt transaction.
A lease file is a trusted local host token, not a remotely authenticated credential.

```sh
workflow artifact init /path/to/artifacts
workflow artifact prepare request.json type.json source.json input-refs.json
workflow artifact put /path/to/artifacts publish.json report.json
workflow artifact verify /path/to/artifacts <artifact-id> expected-type.json
workflow artifact lineage /path/to/artifacts <artifact-id> - 20
workflow artifact impact /path/to/artifacts <replaced-input-id> - 20
workflow run --artifacts /path/to/artifacts status runs.db <run-id>
```

Most responses use `{"ok":true,"result":...}`. Save the `result` object as the next
input file; worker request/result commands retain their existing direct protocol
JSON. `artifact prepare` binds the supplied request's producer and input digests.
It does not prove that the request was claimed or executed; `run finish` checks
against the authoritative persisted attempt. The source revision and upstream
artifact list are host declarations, not authenticated checkout attestations.

## Reference semantics

An `ArtifactRef` contains an immutable manifest, its SHA-256 digest, a derived
`artifact-<manifest-hash>` ID and portable `artifact://<id>` location. The URI is
resolved through the configured store; it is never a URL to fetch or a local path.
The manifest binds:

- Exact artifact type ID/version and content schema: bytes, UTF-8 or typed JSON.
- Byte length and SHA-256 digest of the original payload bytes.
- Producing run, node instance, attempt, exact worker request and input digests.
- Source repository identity and full 40/64-character lowercase Git revision.
- Exact input artifact IDs/manifest digests, run access scope and retention policy.

The worker protocol's `EvidenceRef.digest` binds this **manifest**, including
provenance and content identity. It is not the payload digest; the latter is
`manifest.content_digest`. No worker protocol version or capability descriptor
changes are needed. The manifest/ref format is independently versioned.

Typed JSON uses the IR's closed `ValueType` contract; required members must exist,
unknown members and wrong types reject, and duplicate keys reject at any depth.
A consumer supplies the complete expected type through `artifact verify` or
`verify_expected`; matching a filename or a media-type label is insufficient.
An artifact type ID/version cannot be rebound to a different schema in one store.
Publishing the same spec and bytes is idempotent. A different attempt, input,
source revision, type or payload produces a new manifest identity; it cannot
silently overwrite the old handoff. Identical payload bytes may share one object.

`lineage` returns verified ancestors before the requested artifact. `impact`
returns retained downstream artifacts that would need revalidation if a particular
input were replaced, including their producing node/attempt identities. Both are
read-only projections with page limits 1–100 and an exclusive `next_cursor`.
They do not edit historical decisions or resume/recompute a run. Changed request
inputs or an old attempt cannot reuse evidence at result submission. Current run
inputs remain immutable. The [R07 current evidence view](../06-acceptance-and-maintenance/04-artifact-acceptance.md#current-evidence-after-an-input-change) records replacement policy and rejects transitive stale evidence until recomputed outputs are accepted.

## Publication, recovery and cleanup

The local store has `objects/`, `uploads/` and `catalog.sqlite`. Paths derive from
checked hashes; caller-supplied artifact locations cannot escape that layout.
Only `artifact init` creates storage. Opening a missing store does not create one;
foreign application IDs/schemas and symlink directory/file entries are refused.
New Unix store directories and files use private creation modes (0700/0600).
The host remains responsible for access to an existing root and exported files.
Use a host-owned local directory. A malicious OS principal that can replace the
root, alter files or rewrite catalog hashes is outside this boundary; this is not
attempt workspace isolation or a sandbox against hostile local processes.

Publication uses a SQLite immediate transaction to serialize manifest changes and
cleanup. It writes a unique temporary file, syncs it, publishes the content without
overwriting another object, syncs the object/upload directories, and commits the
manifest plus catalog count/digest chain with `synchronous=FULL`. A reference is
returned only after commit. If the reply is lost, retry the exact spec and bytes.
Different bytes have a different identity; do not replace hashes to hide uncertainty.

Readers validate the full immutable manifest catalog and its count/digest chain,
check all input identities/scopes, and verify bytes and schemas throughout the
selected lineage. Missing tails, missing files, byte changes or conflicting type
versions reject. There is no repair-by-overwrite command. An existing corrupt
object is not silently repaired by republishing it.

`artifact cleanup-orphans <store>` obtains the same write lock as publication,
verifies the catalog and every retained object, then removes uncommitted temporary
files and objects with no committed manifest. An active upload cannot race cleanup
into losing its object. Committed manifests and all of their content are retained,
even when no run currently refers to them. The only policy in this version is
`run_dependency`; releasing/archiving manifests and time-based retention are not
implemented. Thus cleanup cannot delete an active run's recovery dependencies.
Cleanup errors can leave some unreferenced files already removed; retry is safe.

Limits are explicit: payloads at most 64 MiB, typed JSON at most 2 MiB, manifests
at most 64 KiB, 128 direct inputs, 512 unique artifacts in a lineage/impact query,
and 10,000 catalog entries. Reads verify full selected content, and publication
buffers a bounded payload in memory. There is no production throughput or
constant-time recovery claim. CLI responses remain bounded at 2 MiB; reduce page
size for large manifests. Payload export avoids putting large bytes in JSON.

## Result acceptance and durable dependencies

Configure `SqliteRunStore::with_artifacts` in Rust, or prefix each relevant CLI
operation with `run --artifacts <store>`. The location is host configuration,
never stored as an absolute path in a run. Every result evidence reference must
resolve to a verified artifact from the exact run/node-instance/attempt/request
and input digest. Completion also remains subject to live lease, output contract,
node eligibility and commit-time deadline checks.

Artifact publication precedes run completion. A crash between them may leave a
retained artifact without a run result, which is safe. Run state never confirms
missing or unfinished content. A crash after run completion does not republish or
invoke the worker during replay. Result retries use the original identities.

Reads and mutations of an evidence-bearing run require its artifact reader. Missing
configuration or dependencies return an error instead of reporting a healthy run.
Payload corruption also blocks `status`/`verify`/recovery. A replacement adapter
must validate equivalent manifests and bytes and honor retention. No automatic
filesystem or network lookup is attempted without the host configuration.
Administrative `run event` accepts other trusted kernel facts without evidence;
it must not be exposed to untrusted workers. Runs containing postconditions reject
raw successful task events, and all raw gate decisions are refused.
Artifact integrity is not proof that the reported business assertions are true:
[Evidence policies](07-evidence-gates.md) and [mandatory postconditions](08-runtime-postconditions.md)
check accepted worker outputs. [Shared artifact access](../05-shared-execution/04-shared-artifacts.md)
adds authenticated upload and producer admission.

Run storage uses schema **11**. `run --artifacts <store> migrate <db>
<new-backup-file>` explicitly upgrades schemas 1–10 after a verified backup,
checks dependencies and preserves retained execution records; foreign/future
stores are refused. See [version migration](../04-effects-and-recovery/05-version-migration.md). Artifact
catalog schema is separately versioned at 1. Existing definition/worker wire
formats remain unchanged.

## Portability and verified boundary

`artifact export <store> <id> <new-file>` writes verified payload bytes without
overwriting an existing file and returns the exact reference. Save that reference,
then use `artifact import <destination-store> <reference.json> <payload>`. Import
checks the expected reference before publication. For graphs, export/import in
`lineage` order so exact upstream references are present first. Two local stores
produce identical references. An object-store implementation can use the same
port; [R07 acceptance](../06-acceptance-and-maintenance/04-artifact-acceptance.md) describes the S3-compatible
adapter and its scoped transfer authority.

Tests terminate uploading processes halfway through the payload, after file sync,
after object publication, after manifest writes and after commit. Recovery finds
an orphan or a complete durable manifest. Tests also race independent publishers,
exclude concurrent cleanup, inject SQLite full/read-only failures, change/remove
files, inject symlinks and damage catalog tails. A real built-in worker report is
accepted only after publication; incorrect input provenance, missing readers and
corrupt relocated content reject. SQLite schema 2 migration preserves lease fencing.

The evidence covers Linux local process failures and injected SQLite faults.
It does not establish arbitrary power-loss/filesystem durability, disk-loss
recovery, hostile shared-directory safety, remote authentication or business
benefit. [R07 acceptance](../06-acceptance-and-maintenance/04-artifact-acceptance.md) documents isolated workspaces,
reviewed merges, shared storage and current evidence after input replacement.
[Backup/recovery](../04-effects-and-recovery/04-backup-recovery.md) and [shared recovery](../05-shared-execution/07-shared-recovery.md)
document the separate archive and lifecycle boundaries.

## Outputs from attempt workspaces

The [workspace adapter](06-workspaces.md) captures declared paths with exact artifact
types and retains an output manifest whose lineage includes the original input
artifacts and captured files. It preserves the allocation's base source revision
and records the changed tree digest separately. Host-managed execution must bind
actual inputs to that directory and settle its real result before the captured
reports can be eligible evidence. Workspace observation does not authorize effects
or automatically inspect the current target during gate consumption.

<!-- book-navigation -->

3.5 Artifacts and provenance

[Book contents](../README.md) · [3. Execute and verify](README.md) · [中文](../../zh/03-execution-and-evidence/05-artifacts.md) · [Previous: 3.4 Bounded model execution](04-model-execution.md) · [Next: 3.6 Attempt workspaces](06-workspaces.md)

<!-- /book-navigation -->
