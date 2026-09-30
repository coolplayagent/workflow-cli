# R07 artifacts, isolated attempts and revalidation

Artifacts retain exact types, immutable manifests, run/node/attempt and request/input
digests, source revision, input links, scope and retention. Storage and transport do
not change `artifact://` identities. Accepted worker outputs remain authoritative;
an uploaded report or a model's explanation alone cannot pass a gate.

## Acceptance evidence

| R07 criterion | Executable evidence |
| --- | --- |
| Trace release artifacts to requirements/design/code/tests/attempts | Local lineage/impact contracts, workspace capture/proposal/merge manifests, and `real_s3_roundtrip_lineage_download_auth_integrity_and_process_crashes`; the object CLI drill migrates actual accepted reports with their producing attempts and source revisions. |
| Wrong type, missing file or changed digest blocks consumption/gates | Local and S3 reader contracts verify the full ancestry; existing durable artifact/gate contracts plus `input_invalidation_revokes_prior_gate_pass_until_fresh_evidence_is_bound` prove fail-closed gate behavior. |
| Parallel repairs cannot silently overwrite; merged code is revalidated | Independent-process workspace tests and `sealed_parallel_repairs_require_explicit_conflict_resolution_and_create_a_new_verified_revision` exercise independent files, conflict selection and exact new Git objects. `execute-isolated.py` claims two real repair attempts, refuses their unresolved conflict, explicitly merges the sealed proposals, and executes the real validator and frozen gates against that exact new Git revision; it rejects an old source binding. |
| Local export to object storage preserves references | `object-contract.py` imports four accepted artifacts into real MinIO, reads and verifies two durable runs through the S3 reader, then exports back to a new local store with identical references. |
| Upload process interruption cannot create successful incomplete references; orphans are reclaimed | Existing local upload/commit process kills and real S3 subprocess termination before upload, midway through an 8 MiB streamed PUT, after PUT, before manifest commit and after commit. Reopen, cleanup and exact retries verify the boundary. |
| Input changes identify affected evidence/nodes and retain history | `durable_revalidation_view_blocks_old_and_new_stale_descendants_but_preserves_history` checks the transitive producer projection, later stale descendants, new evidence and original history. Gate revalidation revokes an earlier PASS under the selected current-input policy. |

These measure deterministic contract correctness and process interruption. They
do not estimate production throughput, model quality or business time savings.

## Run actual tasks in workspaces

```sh
cargo build --locked -p workflow-cli
python3 examples/workspaces/execute-isolated.py target/debug/workflow
workflow run --artifacts /host/artifacts drive-workspaces runs.db run-id owner 20 workspace-binding.json
```

The executable example creates its own Git repository and six durable attempts
using five isolated workspaces. Two repairs record conflicting fixture-host edits
as sealed proposals; an explicitly claimed merge produces a verified new Git
revision. The original and merged revisions each run the builtin validator, capture
typed reports and pass frozen postconditions. A mismatched source is rejected before
invocation. The example checks that the original repository and completed run are
unchanged. The fixture host supplies the proposed edits; no model or shell repair
adapter is implied.

`drive-workspaces` selects a host binding with `workspace_store`, `repository_path`,
`source_revision` and `capabilities`. Each capability entry selects an exact
capability version, `inline_files` mapping UTF-8 request inputs to committed paths,
optional typed `input_artifacts`, and `reports` with exact paths/types/output fields.
See the example for a complete generated binding. Initialize the workspace and
artifact stores explicitly before running it.

The runtime claims the actual request under a lease, binds the allocation to that
attempt, compares request bytes to committed source files, invokes the builtin,
writes exact typed outputs, captures them and settles through the existing fenced
completion protocol. It refuses undeclared source changes. Repeated output writes
must have identical bytes; a partially written/conflicting output requires a new
attempt. The environment includes OS, architecture, Git version, executable SHA-256,
CLI version and capability identity/contract. An existing attempt cannot continue
with a different executable binding. An output/capture failure never settles a task
as successful.

The `TaskExecutor` runtime port supports host execution wrappers without moving
claim, lease or result authority out of storage. This supplied wrapper runs read-only
builtins over exact inline inputs. It does not invoke shell commands, model-selected
programs or shared external writes. Stateless inline-only tasks can still use the
ordinary `drive` command. Process sandboxing and arbitrary command tools belong to
R14; existing effect adapters retain separate declared targets and effect authority.
Directory separation is not an OS security boundary.

## Seal, review and merge proposals

```sh
workflow workspace seal workspaces workspace-id artifacts summary.json
workflow workspace merge-plan artifacts repository-id source-repo source.json proposals.json resolutions.json
workflow workspace merge-apply artifacts repository-id source-repo plan.json merge-request.json summary.json new-merged.git
```

`summary.json` is a bounded JSON string explaining the decision. `proposals.json`
is a sorted array of exact proposal artifact links. All proposals must share one
run, source revision and complete baseline. At most 16 proposals and 64 declared
file modifications/deletions per proposal are accepted. Sealing verifies all input
artifacts, captures the changed typed bytes and records the entire observed tree.
Later edits cannot change a sealed proposal. Undeclared changes, unsafe filesystem
entries or changed files during capture stop sealing.

Plans combine disjoint or identical edits. Different edits/deletions of one path
produce a conflict; `resolutions.json` maps that exact path to one of its proposal
artifact IDs. Unused or invented resolutions are rejected. Path/case/directory
collisions reject the plan. Application re-verifies the source, retained proposals,
selected bytes and complete plan. It never reads later mutable workspace edits.

The apply command requires a workflow request for the merge producer in that run.
It writes a new bare Git repository, verifies its actual SHA-256 commit/tree/blob
objects and publishes an immutable merge record with proposal lineage. The original
repository/index/refs and proposal workspaces are untouched. The new root commit
records the reviewed plan digest and original revision; logical ancestry remains
in the manifest rather than claiming a Git parent which was not imported.

Both the plan and merge record require revalidation. Use the resulting source
revision for the next task's inputs and frozen gate target. Existing gate checks
bind reports to the exact revision/input/attempt, so a report from a proposal or
the old revision cannot validate the merged target. The host must verify the
supplied merge request's authority before invoking standalone CLI building blocks;
the integrated runtime performs that check through the durable task claim.

A failure after repository creation can leave an unconfirmed directory. Reusing
an existing destination is refused; an explicit retry into another new directory
produces the same Git identity from the same reviewed plan.

## Current evidence after an input change

```sh
workflow artifact invalidate artifacts request.json source.json replacements.json summary.json
workflow run --revalidated-artifacts artifacts plan-artifact-id status runs.db run-id
workflow run --revalidated-object-artifacts object-binding.json plan-artifact-id verify runs.db run-id
```

Each replacement is `{ "old": ArtifactLink, "new": ArtifactLink }`. Replacement
inputs must have the same exact type/run scope, be independently verified, and not
still depend on a superseded input. The retained plan records the inventory digest,
changed inputs, transitive affected artifact/producer/attempt identities, the
`require_fresh_evidence` policy and an explicit decision summary. Publishing the plan
retains the old and new input ancestry. No historical manifest or run event changes.

The current reader rejects old inputs and every dependent artifact, including
artifacts published after the projection. A report that merely adds a new attempt
ID while retaining an old dependency remains invalid. Recomputed evidence over the
replacement inputs becomes eligible only through normal result/gate validation.
Use the original artifact reader for historical audit queries. A current-view
recovery read may correctly reject a historical run containing superseded evidence.

This is an explicit host-selected policy, not mutation of immutable in-flight
inputs or automatic replay of business writes. A new revision/run or declared
rework path supplies the new task inputs. The policy requires fresh evidence and
does not reuse or silently rewrite earlier successful decisions.

## S3 objects and shared manifest authority

`workflow-artifact-s3` implements the same reader/store/inventory ports. S3 holds
payloads; a PostgreSQL namespace holds the append-only manifest chain. Different
hosts using the same database namespace serialize publication and cleanup on its
catalog row. The namespace is bound to the endpoint, bucket, region and prefix.
Only `s3-init` creates the catalog; the bucket is provisioned by its owner.

```json
{
  "namespace": "project-artifacts",
  "database": {
    "connection": {"type": "environment", "name": "WORKFLOW_OBJECT_DATABASE"},
    "transport": {"type": "tls", "ca_file": "/host/postgres-ca.pem"}
  },
  "object": {
    "endpoint": "https://s3.example.invalid",
    "bucket": "workflow-artifacts",
    "region": "us-east-1",
    "prefix": "project"
  },
  "access_key": {"type": "environment", "name": "WORKFLOW_S3_ACCESS"},
  "secret_key": {"type": "file", "path": "/host/private/s3-secret"},
  "ca_file": "/host/s3-ca.pem"
}
```

An optional `session_token` uses the same secret-reference format. Object endpoints
require HTTPS; literal loopback HTTP requires `allow_http_loopback: true` and is
used only by the executable fixture. Requests have bounded bodies/timeouts, no
redirects/proxy discovery/automatic retries, and redact provider bodies and credentials
from errors. [S3 Signature V4](https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sig-v4-authenticating-requests.html)
authenticates the request; [conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
prevent replacing a publication key.

Publication reserves a unique key durably before sending bytes. It then performs
one conditional PUT, reads back and verifies bytes, and commits the manifest plus
integrity head in a synchronous PostgreSQL transaction. No reference is returned
before commit. A lost reply after commit is recovered by exact artifact identity;
an earlier failure leaves an uncommitted reservation. No later attempt adopts its key.

Cleanup expires uncommitted reservations after five minutes, verifies all retained
objects, and deletes only abandoned keys. It never deletes a committed manifest or
its object. Tombstones remain so repeated cleanup also catches a delayed PUT from
a dead process. Cleanup is paginated and reports deletion requests rather than
claiming bytes which it did not measure. PostgreSQL/catalog outages or corrupt
retained content stop cleanup. Incomplete uploads are not successful artifacts.

```sh
workflow artifact s3-init object-binding.json
workflow artifact s3-import object-binding.json local-artifacts artifact-id
workflow run --object-artifacts object-binding.json verify runs.db run-id
workflow artifact s3-export object-binding.json artifact-id new-local-artifacts
workflow artifact s3-download-grant object-binding.json artifact-id 60 new-private-ticket.json
workflow artifact s3-cleanup-orphans object-binding.json - 100
```

Import/export verify the whole lineage and preserve every reference. Native paths,
bucket keys, database endpoints and credentials are host bindings, never artifact
identity. `drive-workspaces` can publish directly with `--object-artifacts` too.
The existing authenticated shared-service transfer protocol continues to enforce
assignment-derived producers, exact types, run scope and short-lived chunk grants.

The S3 download operation is a trusted host authorization boundary: authorize the
worker/run/assignment before requesting its ticket. It writes the bearer URL to a
new private 0600 file and reports only the artifact and expiry on stdout. The URL
allows one exact GET resource for 1–300 seconds, without list/PUT/catalog credentials;
ancestors need separate tickets. The worker must obtain the expected reference over
its authenticated channel and verify bytes against it. Signed URLs can be reused
within their validity period; they are not single-use credentials. See
[S3 presigned request semantics](https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sigv4-query-string-auth.html).

Retained artifacts use `run_dependency`. Limits per object catalog are 512 artifacts,
512 MiB retained payloads, 64 MiB per object, 64 KiB per manifest, 512 ancestors and
10,000 abandoned reservation records. Capacity exhaustion is explicit. Rotate to
a new namespace instead of deleting retained history or tombstones. Backups must
retain both the PostgreSQL object catalog and all referenced S3 objects; a local
run archive alone does not back up this adapter. A restored catalog must not run
cleanup against an independently active source namespace.

Run the real interoperability acceptance against disposable PostgreSQL:

```sh
WORKFLOW_TEST_POSTGRES='host=127.0.0.1 port=5432 user=postgres password=fixture' \
  python3 examples/artifacts/object-contract.py target/debug/workflow
```

The script verifies a pinned MinIO executable digest, starts it only on loopback,
runs process-fault and permission tests, migrates actual accepted task evidence,
then stops it and removes its temporary files. PostgreSQL is explicitly supplied.
The fixture release is for reproducibility, not a recommended production service.
Both this drill and the automatic workspace example are mandatory CI steps.
