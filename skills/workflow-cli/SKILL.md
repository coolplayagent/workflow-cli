---
name: workflow-cli
description: Author, validate and execute durable workflow SOPs through the workflow CLI, including evidence gates, model bindings, recovery and reviewed templates. Use for workflow-cli process definitions and run administration.
metadata:
  version: "0.1.0"
---

# Workflow CLI

Use the packaged CLI as the authority for definitions, state transitions, evidence
and recovery. Resolve this skill's absolute directory, then run
`<skill-root>/scripts/workflow.sh version --format json` and
`<skill-root>/scripts/workflow.sh help`. In the examples below, `workflow` means
that wrapper with the same arguments. It preserves the caller's working directory.

The wrapper prefers the matching bundled Linux x86_64 executable, then a matching
version on PATH. `WORKFLOW_BIN` explicitly selects an executable. An incompatible
version is rejected. Release archives contain the binary, manuals and examples;
source installations require a matching published CLI on PATH. Do not compile an
unrelated checkout to repair a missing runtime. See [installation](references/install.md).

Read only the reference needed for the task:

| Task | Reference | CLI entry |
| --- | --- | --- |
| Create, edit, compare or publish definitions | [Definitions](references/definition.md) | `validate`, `draft`, `release` |
| Discover capabilities or integrate a worker | [Capabilities](references/capability.md) | `capability`, `worker` |
| Check a bundle or simulate/replay transitions | [Replay](references/replay.md) | `kernel` |
| Execute, inspect, pause/cancel, handle effects or recover | [Runs](references/run.md) | `run`, `daemon`, `backup` |
| Publish or verify typed evidence | [Artifacts](references/artifact.md) | `artifact` |
| Evaluate current evidence and enforce postconditions | [Gates](references/gate.md) | `gate`, `run` |
| Isolate attempt files and capture outputs | [Workspaces](references/workspace.md) | `workspace`, `run drive-workspaces` |
| Bind a provider and execute bounded model policies | [Models](references/model.md) | `model`, `run drive-models` |
| Use authenticated shared execution | [Remote execution](references/remote.md) | `service`, `remote` |
| Plan, review and instantiate reusable SOPs | [Templates](references/templates.md) | `template` |

Use `workflow schema` or `workflow schema <kind>` for actual input shapes and
`capability list/describe` for exact available contracts. All relative command
paths resolve against the working directory. Packaged manuals are under
`references/manuals/`; packaged examples and schemas are under `assets/`.
References use these paths relative to the skill root unless they are Markdown links.

Choose store paths and bindings from the task. Only explicit initialization
commands create stores. Diagnose missing, foreign or corrupt storage before
changing configuration. Runtime data belongs outside the installed skill directory.

Interpret both command success and business state: a zero exit code can accompany
a failed or waiting run. Inspect `result.snapshot.status` for mutations and
`result.status` for status reads; definition validation uses `valid`. A static
validation, plan or replay does not execute business work. Explicit leases,
settled results and verified artifacts determine durable progress.

Keep credentials in host bindings/secret references. Preserve exact versions,
digests, expected revisions and retry identities. Use existing task authorization;
never manufacture approval, worker observations or evidence to advance a run.
Uncertain writes require reconciliation through the effect ledger before retrying.

The builtin capability descriptors retain the legacy documentation reference
`workflow-capability@1.0.0` for digest compatibility. Its instructions are now in
[Capabilities](references/capability.md); a second skill installation is unnecessary.

Report the actual definition/run identity, business state, evidence or diagnostics,
and unresolved bindings. The same CLI serves local and remote modes; local builtin
execution needs no remote service. Business adapters, model providers and shared
PostgreSQL services are configured only for workflows that require them.
