# Definition authoring and publication

The registry implements the editing and publication portion of R01. It holds
portable definition data and history. It does not execute a node or resolve an
external capability, subworkflow bundle, model policy or tenant permission.

## Try the authoring loop

From the repository root (the parent directory for the database must exist):

```sh
cargo run --locked -- draft create /tmp/workflow-demo.sqlite review examples/review.yaml
cargo run --locked -- draft publish /tmp/workflow-demo.sqlite review 1
cargo run --locked -- draft edit /tmp/workflow-demo.sqlite review examples/registry/review.patch.json
cargo run --locked -- draft diff /tmp/workflow-demo.sqlite review 1 2
cargo run --locked -- draft publish /tmp/workflow-demo.sqlite review 2
cargo run --locked -- release get /tmp/workflow-demo.sqlite requirements-review 1.0.0
cargo run --locked -- release export /tmp/workflow-demo.sqlite requirements-review 2.0.0 yaml
```

Use a new database or draft ID for a fresh demonstration. The patch changes the
version to `2.0.0` and reduces the approval deadline. Version `1.0.0` still contains
the original definition. `workflow help` lists every command. Pass absolute file
and database paths when invoking the binary through `bazel run`.

## Independent modules

`workflow-definitions` owns edits, review diffs, errors, revisions and the
`DefinitionRegistry` port. It depends only on the IR and validator. It has no
SQLite, filesystem, command-line, transport or provider dependency.

`workflow-registry-sqlite` implements that port for a local SQLite database.
`workflow-cli` composes it with the application commands. Each crate has an
explicit Bazel `rust_library`, and the same tests run through Cargo and Bazel.
A future service can reuse the port and domain operations with its own adapter.
The existence of this adapter does not satisfy the separate durable RunStore or
cluster scheduling requirements.

## Three different identities

| Identity | Meaning |
| --- | --- |
| `draft_id` | Permanent authoring identity, independent of a workflow's business ID |
| `revision` | Positive increasing integer for that draft; compare before every edit, replacement, deletion or publication |
| workflow `id` + `version` + `digest` | Immutable published content; a named version can never point to different content |

A draft starts at revision 1. A changed edit appends a full history revision and
moves its current pointer in one transaction. A no-op retains the same revision,
but still requires the correct expected revision. A whole-definition replacement
is an import with the same concurrency rules. The workflow ID cannot change
within a draft; create another draft when copying or renaming a workflow.

Deleting appends a tombstone revision. Current lookup then returns `not_found`,
but historical revisions and published definitions remain readable. Deleted draft
IDs cannot be reused. This prevents an old revision token accidentally matching
a newly created draft of the same name. There is no automatic history pruning or
unpublish operation in storage schema 1.

## Incremental edits and diagnostics

`workflow schema patch` returns the JSON Schema for edit requests. A patch contains
`expected_revision` and 1–256 operations. Allowed operations change the workflow
version, entry or inputs; add/replace/remove a node or edge; or explicitly reorder
all edges. Add-edge optionally names an existing edge to insert before. Replace
retains the stable ID and position. Remove-node does not silently remove attached
edges or bindings; update them in the same batch or repair the resulting draft.

Operations apply in sequence to a private copy. An unknown ID, duplicate insertion,
invalid edge permutation or malformed request rejects the whole batch. No revision
is appended if the batch fails. A conflict response includes both expected and
actual revision. Read the new draft, inspect its diff and rebase the intended edit;
changing the token blindly may overwrite another author's work.

Drafts may have incomplete graphs, bad input bindings or other semantic diagnostics.
They must still have schema version 1, unique stable node/edge IDs, fit the graph
and 1 MiB canonical document limits, and round-trip through the bounded JSON
parser. A draft's digest identifies its content even when it has diagnostics.
This differs from `workflow validate`, which emits a digest only when static
validation succeeds. Creation and edits return the draft plus diagnostics.
Node order is canonicalized by ID consistently in returned and stored snapshots.
Edge order is retained.

Publishing checks the current revision and runs the full static validator inside
the write transaction. It stores the canonical content, digest, source draft ID
and source revision. Republishing identical content at the same ID/version returns
the original publication, including its original provenance. Different content
returns `publication_conflict`; assign a new workflow version. Later draft edits
or deletion cannot alter a publication. Reads verify content digests, definition
identity, static validity and the source revision's digest before returning it.
These are integrity checks, not cryptographic authentication of a database owner.

## Review diffs and queries

`workflow diff <before-file> <after-file>` and `workflow draft diff <db> <id>
<before-revision> <after-revision>` return digests and deterministic changes.
Review paths are RFC 6901 pointers into an ID-keyed representation: for example
`/nodes/review/kind/timeout_ms`. Each change has `added`, `removed` or `modified`,
plus before/after values. Nested object fields are compared individually; arrays
are compared as whole values. Node order is ignored. Edge priority changes appear
at `/edge_order`, since it can change first-match decision behavior. Draft revision
diffs additionally report `before_deleted` and `after_deleted` so a deletion is
visible even though its tombstone retains the previous content.

Draft and release lists use explicit keyset pagination: `after-id` or
`after-version`, then a limit in 1–100. Use `-` for the first page. `next_cursor`
indicates another page. Ordering is bytewise lexicographic, not semantic-version
ordering; there is no implicit `latest` selector. Each page has one read snapshot;
pages across separate requests may observe concurrent additions or deletions.
`draft revision` addresses exact history. `release digest` addresses immutable
published content by SHA-256.

Registry JSON responses use `ok` plus `draft`, `publication`, `page`, `diff` or
`error`. Exit 1 covers invalid definitions, missing records, duplicate identities
and optimistic/publication conflicts. Exit 2 covers invalid requests, database
format mismatches, corruption, lock exhaustion and I/O failures. Raw export writes
only definition JSON/YAML to stdout; export failures go to stderr. Export a draft
to retain an incomplete work in progress; ordinary file `workflow export` and
published release export require valid definitions. Never redirect export onto
its own input file or database.

## SQLite behavior and limits

The adapter uses immediate transactions for writes, foreign keys and synchronous
FULL. Revision checks, history insertion and head updates are one transaction.
Publication validation and insertion are one transaction. Immutable revision and
publication tables additionally reject SQL updates/deletes with triggers. The
application ID and independent storage schema version reject foreign or future
databases; this increment contains no automatic storage migration.

Only `draft create` may initialize an empty database. Other CLI commands open an
existing file without CREATE, allowing SQLite to recover an interrupted journal.
Even query commands therefore need a writable database for crash recovery. The
library also exposes `open_readonly` for a stable database, which cannot recover a
hot journal. SQLite URI interpretation is disabled; empty and `:memory:` filenames
are rejected by the durable adapter. Paths are explicit; there is no implicit
per-user database.

Lock contention waits up to five seconds, then reports `busy`. No unsafe force
option bypasses revision checks. Mutations recheck storage version within their
transaction. A commit whose acknowledgement is lost must be resolved by reading
current/history or the immutable publication. Retrying an old mutation token is
safe because it cannot create a second changed revision; it may return a conflict.

The tests race independent OS processes using the same revision, reopen after a
successful child process, and terminate a writer with an uncommitted journal.
They verify exactly one revision winner, intact old releases and rollback of an
uncommitted update. These tests cover process failures on a local filesystem, not
power loss, disk destruction or a network filesystem. The database needs normal
backup and capacity planning. This is a local registry, not a shared cluster DB.

SQLite transaction behavior follows [SQLite's transaction contract](https://www.sqlite.org/lang_transaction.html)
and the [rusqlite transaction API](https://docs.rs/rusqlite/0.40.2/rusqlite/struct.Transaction.html).

<!-- book-navigation -->

2.2 Author and publish

[Book contents](../README.md) · [2. Define the process](README.md) · [中文](../../zh/02-process-definition/02-definition-registry.md) · [Previous: 2.1 Definitions and types](01-definition-semantics.md) · [Next: 2.3 Capabilities and workers](03-worker-protocol.md)

<!-- /book-navigation -->
