# Authenticated shared artifact transfer

The HTTPS service publishes and consumes typed artifacts in PostgreSQL. A remote
worker can return artifact evidence that the run reducer verifies during result
settlement and recovery. Manifest identity, content types, lineage and producer
contracts match [local artifacts](../03-execution-and-evidence/05-artifacts.md).

This adapter stores bytes in PostgreSQL `bytea`. [R07](../06-acceptance-and-maintenance/04-artifact-acceptance.md)
adds a separate trusted S3/object adapter and actual workspace/evidence execution.
[R14](../06-acceptance-and-maintenance/06-security-acceptance.md) defines authenticated access, provider leases,
retention/archive/deletion policy and private audit export. S3 presigned URLs do
not replace the credential-bound shared download protocol.

## Initialize and authorize

Fresh `workflow service bootstrap` initializes the additive `workflow_artifacts`
schema, version 1. For an existing authenticated deployment, a trusted host with
the database binding runs:

```sh
workflow service init-artifacts server-binding.json
```

Initialization is idempotent for version 1 and rejects unknown versions. Public
requests cannot initialize or upgrade storage. Existing read-only deployments
can operate without this schema, but cannot accept artifact-backed results until
it is initialized. Include this schema's content, metadata, credentials, assignments
and `workflow_authority` in a consistent database backup. [Shared backup/restore](07-shared-recovery.md) verifies that complete dependency closure.

A worker credential's exact capability ID/version/contract digest rule optionally
contains an `artifacts` policy. Absent policy denies all artifact access. Example
policy for a capability with committed `repository`, `revision` and `source`
inputs:

```json
{
  "inputs": {
    "source": {
      "identity": {"id": "build-report", "version": "1.0.0"},
      "content": {"format": "utf8"}
    }
  },
  "output": {
    "types": [
      {
        "identity": {"id": "build-report", "version": "1.0.0"},
        "content": {"format": "utf8"}
      }
    ],
    "repository_input": "repository",
    "revision_input": "revision"
  }
}
```

`source` must be an ArtifactLink in the prepared task's inputs. The service checks
its exact declared type, tenant/project, current run and full lineage. Worker read
authority covers only those roots and their required dependencies. Output-only
workers use `"inputs": {}`; consumers without publication permission use
`"output": null`. Source revision is bound to committed input fields; publication
does not independently measure the worker's filesystem.

The service derives producer run/node/attempt, request digest, input digest, run
access scope, lineage and `run_dependency` retention. The worker cannot choose a
producer manifest or substitute a caller-supplied identity. Every upload phase
rechecks credential, assignment, exact capability contract, current lease and
primary database time. A released/replaced lease, settled assignment, expired or
revoked credential fences subsequent calls, including identical completion retries.

## Upload, resume and consume

With a current assignment and a host-provisioned artifact policy:

```sh
workflow remote artifact-upload worker-binding.json ASSIGNMENT_ID report-request-1 artifact-type.json report.txt
workflow remote artifact-download viewer-binding.json download.json downloaded-report.txt
```

The upload command computes content length/digest locally and prints the completed
ArtifactRef. Its request ID is scoped to the worker credential and assignment.
Explicitly rerunning the same command and exact bytes resumes persisted progress;
changing the specification or previously stored chunk fails. Transport errors return
without blind automatic replay. Concurrent identical resumptions can reconcile
progress and verify the final reference.

`download.json` contains the exact artifact link, an optional worker assignment,
and a TTL of 1–300000 milliseconds:

```json
{
  "artifact": {"artifact_id": "artifact-<manifest-sha256>", "digest": "sha256:<manifest-sha256>"},
  "assignment_id": null,
  "ttl_ms": 60000
}
```

Run-reading roles can read artifacts in their authenticated scope. A worker must
supply its own current assignment, whose declared inputs authorize the link. An
administrator credential alone has no read permission. A download grant is bound
to the exact credential and scope, expires no later than credential/assignment
expiry, and requires the bearer on every chunk. It is not an unauthenticated URL.
Copying it to another credential never transfers access.

`RemoteClient::upload_artifact` and `download_artifact` perform the same complete
manifest/type/content checks. The download command validates all bytes before
creating an exclusive 0600 file and synchronizing it and its parent directory.
Existing paths are refused. If filesystem synchronization fails, delivery is
unconfirmed and a newly created partial file may require cleanup. File inputs
reject FIFOs and other non-regular files without waiting for a writer.

The low-level typed operations are `artifact_begin`, `artifact_put`,
`artifact_complete`, `artifact_grant`, `artifact_get`, and `artifact_cleanup` through
`workflow remote call`. Incomplete uploads have no catalog reference. Completion
verifies all bytes and lineage, then commits content, manifest, completion receipt
and chunk cleanup atomically. Attach the returned artifact ID/digest to an actual
WorkResult's evidence. The existing result ingress checks its producer/request/input
binding before advancing the run; merely uploading content cannot finish a task.

The supplied [shared report fixture](../../../../assets/examples/artifacts/shared-report-start.json)
uses a test-host `fixture.report` capability. It is not registered by the builtin
CLI worker. The HTTPS integration test supplies that adapter explicitly.

## Bounds, cleanup and recovery

- One artifact: at most 64 MiB; fixed 64 KiB chunks except the final chunk.
- One run: at most 512 retained artifacts and 512 MiB of content plus active upload
  reservations; at most 64 incomplete live uploads and 512 transfer records.
- One credential: at most 1000 outstanding download grants. Expired grants are
  reclaimed on grant creation.
- `artifact_cleanup` is available to administrator/recovery roles, scoped to their
  tenant/project, with a bounded limit. It removes expired transfer metadata and
  staged chunks, including expired completed-upload receipts. Retained manifests
  and content remain intact. Full archive/deletion retention policy is defined in [R14](../06-acceptance-and-maintenance/06-security-acceptance.md).

The reducer loads a bounded, verified catalog while holding the run lock. It reads
one bounded content object at a time, retaining manifests, and rejects corrupt
bytes, missing references, inconsistent types or invalid lineage before accepting
settled state. This scan cost applies to reads and mutations; the limits are not a
production throughput claim. Stored artifacts remain available through another
service instance without relying on the producer's local files.

The mandatory PostgreSQL CI job covers actual upload/retry/conflict, corrupt digest,
cross-tenant writes with identical worker policy, copied/expired/revoked grants,
stale leases, typed input roots and transitive dependencies, cleanup and recovery.
The HTTPS fixture kills a separate uploader after its first committed chunk,
resumes through the client, submits artifact-backed work, and checks another
server's recovered state and content. Cargo/Bazel also check TLS/protocol/file
boundaries. R07 and R14 combine these fixtures with their additional acceptance evidence.

<!-- book-navigation -->

5.4 Shared artifacts

[Book contents](../README.md) · [5. Shared execution](README.md) · [中文](../../zh/05-shared-execution/04-shared-artifacts.md) · [Previous: 5.3 HTTPS service and workers](03-remote-service.md) · [Next: 5.5 Remote external effects](05-remote-effects.md)

<!-- /book-navigation -->
