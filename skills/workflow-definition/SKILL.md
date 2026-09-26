---
name: workflow-definition
description: Create, validate, or convert portable workflow-cli SOP definitions in JSON or YAML using the workflow compiler. Use for business process definitions and their diagnostics; this compiler does not execute workflows or administer running jobs.
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

Report the resulting file, validation result, digest and unresolved bindings.
Stop after the requested definition task; this CLI version has no execution,
publication or draft-registry commands.
