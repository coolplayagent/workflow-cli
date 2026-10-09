# How to read this book

A business process often starts as a checklist: inspect a proposed change, ask an
operator to approve it, and publish the result only when the checks are current.
An agent can help perform each step, but a checklist alone does not answer what
has already happened after a crash, which version was approved, or whether a late
worker result still has authority. Workflow CLI makes those answers explicit.

This book uses that approval process as a running example. You will first execute
it locally, then learn how definitions, worker results and evidence fit together.
Later chapters extend the same contracts to external effects and shared execution.
The final part shows how to reproduce the acceptance evidence and diagnose a run.

## The four responsibilities

| Component | Responsibility | What to inspect |
| --- | --- | --- |
| Workflow definition | Declare legal steps, handoffs, decisions and terminal outcomes | Version, digest, contracts and explicit bounds |
| Skill | Help an agent choose the right CLI operation and read only relevant guidance | Task reference and local manual |
| Model and capability adapters | Propose bounded decisions or perform the actual work | Allowed tools, host bindings, request and result |
| Durable runtime | Admit transitions and retain ownership, history and evidence | Run state, leases, settled results and artifacts |

A valid definition describes a process. A completed CLI command reports an
operation. A successful business run additionally requires the declared terminal
state and any mandatory evidence gates. Keeping these distinctions visible is the
central habit you will practice throughout the book.

## Choose a reading path

New users should complete [installation](skill-distribution.md) and
[the first workflow](getting-started.md). The sample needs no model account or
remote database. Authors can then follow the definition and control-flow chapters.
Operators should read local execution, postconditions, backup and migration before
changing a live run. Integrators need worker contracts and the shared execution
part before exposing a service or accepting remote results.

Acceptance chapters document bounded experiments, supported environments and
remaining limits. They are useful evidence when evaluating a deployment, but are
not a guarantee about arbitrary provider adapters or a production recovery SLA.
The [delivery map](roadmap.md) separates implemented contracts from remaining
business-value evaluation.

## Use examples in the right environment

Release users resolve `workflow` through the installed `scripts/workflow.sh`.
Its examples and schemas live under `assets/examples/` and `assets/schemas/`;
the complete bilingual book is under `references/manuals/`. These resources are
local files and can be read without opening GitHub.

Maintainer examples beginning with `cargo`, `bazel`, or repository-relative
`examples/` paths assume a source checkout and the pinned toolchain. For a CLI
example in an installed skill, invoke the wrapper and pass the corresponding
absolute path beneath `assets/examples/`. Build commands and integration suites
still need their documented compiler, PostgreSQL or adapter prerequisites.
Keep all runtime databases, credentials and workspaces outside the skill directory.

## A checkpoint for every chapter

After the quickstart, keep the three questions beside each exercise: which
immutable inputs were used, what durable observation proves progress, and what
would happen if the caller disappeared before receiving the reply? The answers
connect the authoring, execution and recovery parts of the book.

<!-- book-navigation -->

[Contents](README.md) · [中文](zh/preface.md) · [Next: Install the skill](skill-distribution.md)

<!-- /book-navigation -->
