---
name: workflow-artifact
description: Publish, verify, trace, export and import typed workflow-cli artifacts and attach checked report references to real worker results. Use for durable evidence and artifact handoffs; workspace isolation and business gate policy remain separate operations.
metadata:
  version: "1.0.0"
---

# Workflow artifact

Read `workflow help` and `docs/artifacts.md`. In the checkout, use
`cargo run --locked --` or the built binary; Bazel invocation requires absolute
input paths. The local store needs no model, network account or cloud service.

Use the artifact directory authorized by the task. Only `artifact init <store>`
creates one. Do not replace a missing/foreign/corrupt store with a fresh directory
to make a run appear healthy. Committed objects are recovery dependencies.

Read `schema artifact-type`, `artifact-publish` and `artifact-ref`. A type declares
an exact ID/version and bytes, UTF-8 or a closed JSON shape. Use the consumer's
expected schema; do not relabel incompatible data as bytes to pass a check.
Changing a schema requires a new type version.

For run evidence, obtain the real request/grant from `run acquire` and `run claim`.
Perform the actual work through its adapter. `artifact prepare <request.json>
<type.json> <source.json> <input-refs.json>` binds the producer/request/input
identities; its JSON `result` is the publish spec. The source repository and full
Git revision must identify the actual inspected input. Upstream references must
come from checked artifacts. Prepare alone does not prove execution or ownership.

Write the actual bounded report/file, then use `artifact put <store> <publish.json>
<payload>`. A successful response follows payload durability and manifest commit.
Record its complete `ArtifactRef`. `artifact_id` and `digest` identify the manifest;
`manifest.content_digest` identifies payload bytes. Never substitute one digest
for the other or edit a manifest's producer fields after publication.

Before consumption use `artifact verify <store> <id> <expected-type.json>`.
`artifact show` also verifies stored bytes and ancestors, but does not choose the
consumer's expected type. `lineage <store> <id> - <limit>` traces upstream evidence;
`impact <store> <replaced-input-id> - <limit>` identifies retained downstream
artifacts and producing nodes requiring revalidation. Follow `next_cursor` with
limits 1–100. These queries preserve history; they do not authorize recomputation
or mutate prior business decisions.

Attach the exact `{artifact_id,digest}` manifest link to the real worker result's
`outcome.evidence`. Submit with `run --artifacts <store> finish <db> <lease.json>
<attempt-id> <result.json>`. The host checks current ownership, the actual durable
request, producer/input binding and artifact integrity before committing success.
Use the same reader configuration for subsequent status/history/verify operations.
Do not invent a lease, task result, report assertion or approval to satisfy a gate.

A lost publication reply can be retried with the exact spec and bytes. A missing
or mismatched object is an error, not a successful handoff. Investigate using the
original records; do not rewrite hashes or remove evidence. A failed release or
lost run reply may follow a committed result: inspect the original execution
history before another invocation.

Export to a new path with `artifact export`, saving the returned reference.
Import that exact reference and payload into an initialized destination store.
Transfer ancestor artifacts first in lineage order. Verify again after relocation.
The current implementation covers local stores; no remote upload credentials or
temporary download permissions are implied.

`artifact cleanup-orphans` removes only uncommitted objects/uploads after catalog
verification and serialization with active publication. It never deletes committed
manifests, including currently unreferenced ones. Use it only for requested store
cleanup. Never delete live dependencies manually as a retention shortcut.

Report artifact/type identities, source revision, producer, integrity/lineage
results and run revision/business status. Digest verification proves binding and
integrity under the trusted-host boundary; it does not prove business assertions,
authenticate a remote producer or isolate concurrent attempt workspaces.
