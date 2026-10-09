# Workflow workspace

Read `workflow help`, [manual](../../../docs/workspaces.md) and `workflow schema workspace-checkout`.
Use the run database, artifact store, workspace store and repository mapping from
the task. The local adapter requires Linux, `/proc` and Git. Only `workspace init`
creates a store; investigate a missing/foreign store instead of silently replacing it.

Obtain the actual prepared request through the fenced `run acquire/claim` flow.
Preserve its run, node-instance, attempt, request and input identities. A supplied
request alone is not evidence of current execution authority. Use the declared
source commit, exact input artifact links and typed output paths. `workspace
prepare <request.json> <source.json> <input-refs.json> <outputs.json>` builds the
spec; save the response's `result` and allocate with `workspace checkout <store>
<repository-id> <repository-path> <spec.json>`.

Keep each attempt's allocation separate. An exact retry returns the same directory
without resetting edits. A changed contract for the same attempt conflicts; do not
change attempt IDs to hide uncertainty. Use `workspace path` for the native path.
The portable reference does not contain it. The export has no `.git` directory,
uses committed regular file bytes and rejects symlink/submodule source entries.
Do not silently substitute the caller's current or dirty checkout.

Run `workspace observe` to inspect actual changes. `ok: true` or exit 0 does not
mean clean; read `clean`, the tree digest and changes. `workspace verify-clean`
exits 1 for a dirty tree. Generated reports count as changes too. Neither command
advances the run. Use `run drive-workspaces` with an explicit workspace binding for integrated
builtin execution and capture. In a manual worker flow the host must bind the real
capability inputs and execution to this workspace.
`assets/examples/workspaces/validate-isolated.py` demonstrates that binding for the actual
inline definition validator and existing gates.

Write only within the intended workspace and authorized task scope. Allocation
provides independent files, not an OS sandbox or permission to access shared
resources. Preserve the spec's explicit merge policy. Propose changed outputs;
merging into a source branch needs its separately authorized operation and fresh
validation of the resulting revision.

Use `workspace capture <store> <workspace-id> <artifact-store>` for the declared
output paths. It checks types, input dependencies and a stable observed tree, then
returns immutable artifacts and an output manifest. Stop cooperating writers during
capture. A capture does not prove an external target remained unchanged after the
read. The base source revision remains the allocated commit; do not describe edited
files as a new verified commit.

Attach the actual capture references to the real worker result before `run
--artifacts <store> finish`. A later capture cannot modify settled evidence. Inspect
business status and gate decisions afterward; workspace observation alone is not
a gate PASS. Use the [artifact](artifact.md) and [run](run.md) contracts for retention
and fenced settlement. Do not fabricate reports, producer identities or Boolean
results to get a successful transition.

Retain the workspace reference, capture manifest, observed tree digest and run
result. `cleanup-orphans` removes only uncommitted allocations and staging data;
committed workspaces remain retained. Report conflicts, dirty state, unavailable
inputs and storage failures explicitly. After a lost reply, inspect and retry the
same identity before assuming allocation failed.
