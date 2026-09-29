# Authenticated shared artifacts

This implementation work is part of R07 and R14. The existing remote service can
commit builtin read-only results, but its authenticated reducer has no artifact
reader. This prevents a remote run from validating durable artifact-backed results.
The delivery must connect artifact authorization, durable content and result
settlement; an isolated upload API would not satisfy that requirement.

## Authority and immutability

Tenant/project come only from the authenticated credential. A worker publishes
through a current server assignment. The service derives the producer run, node,
attempt, request digest and input digest from that assignment and checks the exact
lease and task deadline again before commit. Revocation orders against publication
through the same credential lock used by result submission. A caller-supplied
manifest or content hash never grants access.

Committed manifests and content are immutable. A reference is returned only after
all content, manifest and lineage checks commit durably. Retry uses stable upload
identity and exact bytes/specification; conflicting retries fail. Interrupted
uploads remain invisible to consumers. Retention can reclaim expired incomplete
uploads but cannot remove a committed dependency of an active or retained run.

## Bounded transfer and verification

Use bounded chunks to preserve the existing 64 MiB artifact contract without
exceeding HTTP request/response bounds. Every chunk is authenticated and scoped to
the upload's assignment; duplicates must match exactly. Finalization checks total
length, full content digest, content type, manifest identity and all source links.
Download chunk boundaries never accept filesystem paths or arbitrary URLs.

The initial shared catalog uses PostgreSQL transactions and immutable content.
This is shared database storage, not an S3 implementation. R07's genuine object
storage transfer requirement remains separate and must receive an actual adapter
and transfer test before that issue closes.

A reducer must receive a bounded, verified artifact catalog for its run. Loading
that catalog must detect missing/corrupt content and lineage before settled state
is accepted, and must not borrow a transaction through unsafe lifetime tricks.
Catalog count/byte limits and scan costs are explicit; they do not imply a measured
production throughput guarantee.

## Read access and temporary grants

Run-reading roles can inspect artifacts in their authenticated scope. Worker reads
are tied to its current assignment's declared inputs and required lineage, not to
all artifacts belonging to another worker or run. A short-lived download grant is
bound to the requesting credential, scope, exact artifact digest and database time.
Copying a grant to another tenant or identity never yields data; an unauthenticated
link is not an alternate access path. Revocation/expiry is checked on every read.

Audit records retain fixed operation names, opaque resource identifiers, actor
and outcome, never payload bytes, bearer credentials or raw database errors.
Bootstrap/schema initialization remains a trusted local host action and must not
silently upgrade an unknown schema from a public request.

## Required evidence

- Actual PostgreSQL publication, interrupted chunk delivery, identical retry and
  conflicting retry; no successful reference to a partial or corrupt object.
- Cross-tenant read/write/result rejection, wrong producer/input/assignment,
  revoked identity, replaced lease, expired and copied download grants, and path
  traversal strings.
- Artifact-backed result completion and subsequent recovery through the HTTPS
  application boundary, using the same manifest/content contract as local storage.
- Content/type/lineage corruption prevents consumption and gate advancement.
- Expired upload cleanup preserves committed data and active recovery dependencies.
- Cargo, Bazel, PostgreSQL CI and a complete final snapshot-bound Qualitygate.

This document records the intended implementation contract. It is not acceptance
evidence, and neither R07 nor R14 is complete from this design alone.
