---
name: workflow-definition
description: Create, edit, validate, compare and publish portable workflow-cli SOP definitions using revisioned drafts and the workflow compiler. Use for business process definitions and their diagnostics; this compiler does not execute workflows or administer running jobs.
---

# Workflow definition

Use the `workflow` CLI as the authority for definition shape and validation. A
Skill explains how to use the compiler; it does not replace its checks with a
model judgment.

Resolve the executable and read `workflow help`. If working in the workflow-cli
source checkout, use `cargo run --locked --` or `bazel run //:workflow --` with the
same arguments. Bazel requires absolute definition paths. Do not install or build
an unrelated checkout just because `workflow` is missing.

Run `workflow schema` for the current IR. Translate the requested SOP into stable
node and edge IDs, explicit input contracts, legal routes and terminal outcomes.
Use binding references for data handoffs. Keep provider credentials, database
connections and queue configuration out of the business definition.

Capability and subworkflow references require exact versions. Use versions from
the user's capability catalog or existing definitions. If a required binding is
unknown, identify that missing information instead of presenting a fabricated
version as executable. The compiler checks version syntax only; it does not
resolve capabilities or authorize execution.

Run `workflow validate <file>` after editing. Read the JSON `diagnostics`, including
`code`, `path`, `node` and `edge`, and fix the affected fields. Exit 1 means an
invalid definition; exit 2 means a usage or I/O failure. A successful static check
returns `valid: true` and a definition digest. It does not establish successful
execution, runtime branch synchronization or approval of external effects.

For format conversion, use `workflow export <file> json` or `workflow export
<file> yaml`. Write to a different path when using shell redirection: redirecting
onto the input would truncate it before the compiler reads it. Preserve stable
IDs. Export performs the same static checks before emitting the definition.

For implicit cycles, model a bounded `loop` with a separately versioned body,
positive iteration and time limits, and an exhaustion route. Decisions need a
default route. `exclusive` rejects multiple matches; `first_match` respects edge
order. Guard optional comparisons with `exists` inside an ordered `all`.

For registry authoring, use an explicit database path from the task or project.
Only `draft create` initializes storage. Read `workflow schema patch` for the
current patch format. `draft get` returns the current revision; put that revision
in the patch's `expected_revision`. A draft can retain incomplete graph
semantics, with diagnostics. Its digest identifies content and is not proof of
validity. Add or replace individual nodes and edges to keep edits reviewable.

When a mutation returns `revision_conflict`, read the current draft and inspect
`draft diff` between historical revisions before rebasing the intended changes.
Do not blindly substitute the newest revision into an old replacement. Failed
batches have no partial effect. Node removal leaves attached references visible
for explicit repair. Edge order changes can affect first-match decisions.

For a requested publication, inspect the semantic diff and use `draft publish`
with the expected revision. Publication revalidates the whole definition and
freezes its ID/version/digest. On `publication_conflict`, preserve the old release
and choose a new workflow version consistent with the task. Use `release get` or
`release digest` to inspect the exact stored release. Publishing a definition is
local registry state; it does not resolve external bindings or start execution.

`draft revision` retrieves old content. Deletion retains a tombstone and history;
the draft ID cannot be reused. Draft/release listing is paginated with an explicit
cursor and a limit of 1–100; `-` selects the first page. Follow `next_cursor` for
additional records. Release versions are sorted lexicographically, not by which
version should be executed.

Report the resulting file or registry identity, revision, validation diagnostics,
digest and unresolved bindings. Stop at the definition operation requested by the
user; this CLI still has no execution or running-job administration commands.
