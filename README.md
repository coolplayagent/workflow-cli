# workflow-cli

Workflow makes business SOPs explicit, reviewable and portable. It provides the
process contract around an agent: declared steps, typed handoffs, legal decisions
and evidence requirements. An LLM supplies node decisions; Skills explain how to
use capabilities; CLI/API adapters implement them. Provider and deployment choices
belong in bindings rather than in the business graph.

**Current implementation:** a Rust definition compiler, static validator,
transactional definition registry, checked read-only capability invocation, a
deterministic workflow kernel, and a transactional RunStore with event history,
checkpoints, a command outbox and a durable callback Inbox. Managed write tasks use
a durable effect ledger with stable keys, query recovery, bounded retries and explicit ordered compensation. A local driver executes read-only tasks with
durable leases, attempts and fenced result commits. Typed artifact manifests bind
content and provenance; result acceptance verifies their durable dependencies.
A portable evidence checker produces PASS/FAIL/UNKNOWN from settled execution records
and exact targets. Frozen task and terminal postconditions require PASS before
advancing; UNKNOWN waits for explicit retry and FAIL follows declared repair bounds.
A separate CLI supports read-only evaluation/revalidation. Attempt workspace
contracts and a local Linux/Git adapter allocate independent files, observe changes
and capture typed outputs with exact provenance. Bounded model policies invoke allowed
read-only tools through OpenAI Responses or Anthropic Messages host bindings, with
explicit records checked during settlement and recovery. Verified local backups retain
run and artifact history; restored runs use fresh lease generations and require
external-effect reconciliation before admitting writes. Authenticated HTTPS
execution connects separate schedulers and workers to PostgreSQL, with scoped
credentials and fenced read-only results. Remaining acceptance work follows the
[issue roadmap](docs/roadmap.md). `validate` is a
static check, not permission to execute a capability or proof of a successful run.

## Use

```sh
cargo run --locked -- validate examples/review.yaml
cargo run --locked -- export examples/parallel-tests.json yaml
cargo run --locked -- schema
```

Bazel builds the same sources with a pinned Rust toolchain. Every module has its
own explicit `rust_library`; Cargo provides editor and dependency-lock support.

```sh
bazel test //...
bazel run //:workflow -- validate "$PWD/examples/review.yaml"
bazel query 'kind(rust_library, //...)'
```

The first build downloads the pinned toolchain and dependencies. Bazel runs a
binary in its execution directory: pass an absolute filename when using
`bazel run`. A directly installed `workflow` binary resolves paths against the
caller's working directory. See [rules_rust's Cargo workspace integration](https://bazelbuild.github.io/rules_rust/crate_universe_bzlmod.html)
for the build integration used here.

## LLM and Skill over CLI

The bundled [workflow-definition Skill](skills/workflow-definition/SKILL.md) instructs an agent to write a draft definition, run `workflow validate
<file>`, inspect JSON diagnostics, fix the indicated field and validate again.
`workflow schema` exposes the complete input shape without sending the model any
provider credentials. Exit status is `0` for success, `1` for invalid definitions,
and `2` for usage or I/O errors. A valid report has a SHA-256 definition digest;
an invalid report never has one. `workflow remote validate <client-binding> <file>`
uses the same report over authenticated HTTPS, with server-side revalidation.
The [R01 acceptance guide](docs/definition-acceptance.md) maps requirements to
checks and gives reproducible definition-error, edit-latency and replay-step baselines.

```json
{"valid":false,"digest":null,"diagnostics":[{"code":"dangling_edge","file":"draft.json","path":"edges[0].to","node":null,"edge":"finish","message":"unknown node missing"}]}
```

Definitions are data: reading or validating one does not invoke its capabilities.
The registry supports incremental draft edits, historical queries and publication.
Read the [authoring guide](docs/definition-registry.md) for the full CLI loop and
concurrency semantics. The [workflow-capability Skill](skills/workflow-capability/SKILL.md)
covers typed invocation of the compiler capabilities and worker request/result
checks. Read the [worker protocol guide](docs/worker-protocol.md) for standalone
and node invocation examples. The [workflow-replay Skill](skills/workflow-replay/SKILL.md)
and [kernel guide](docs/kernel-semantics.md) cover bundle checks, simulated transitions
and checkpoint restore. The [workflow-run Skill](skills/workflow-run/SKILL.md) and
[run storage guide](docs/run-store.md) cover `run start/status/event/cancel`, history
and pending delivery. The [local execution guide](docs/local-execution.md) covers
`run drive`, which calls built-in adapters and commits real results. No background
timer service remains after the command exits. The [workflow-artifact Skill](skills/workflow-artifact/SKILL.md)
and [artifact guide](docs/artifacts.md) cover typed reports, provenance, integrity
checks and their connection to fenced result submission. The [workflow-gate Skill](skills/workflow-gate/SKILL.md)
and [evidence checker guide](docs/evidence-gates.md) cover exact policy/target checks
and decision revalidation. The [runtime postcondition guide](docs/runtime-postconditions.md)
covers frozen mandatory gates, durable UNKNOWN waits and bounded repair.
The [workflow-workspace Skill](skills/workflow-workspace/SKILL.md) and [workspace guide](docs/workspaces.md)
cover host-managed attempt directories and typed output capture.
The [workflow-model Skill](skills/workflow-model/SKILL.md) and [model execution guide](docs/model-execution.md)
cover frozen policies, provider replacement and explicit decision replay.

```sh
cargo run --locked -- kernel replay examples/kernel/review-approved.json
cargo run --locked -- kernel replay examples/kernel/repair-third-round.json
```

These examples contain simulation contracts and supplied task results. Replay
calculates commands without invoking adapters; inspect `snapshot.status` even when
the CLI exits 0.

## Contracts and development

- [R01 acceptance and reproducible definition baselines](docs/definition-acceptance.md)
- [Draft editing, semantic diff and immutable publication](docs/definition-registry.md)
- [Capability contracts, worker protocol and host authority](docs/worker-protocol.md)
- [Typed artifacts, provenance, atomic publication and evidence](docs/artifacts.md)
- [Evidence checker, policy/target binding and revalidation](docs/evidence-gates.md)
- [Attempt workspaces, source objects and captured outputs](docs/workspaces.md)
- [Mandatory task/terminal gates and bounded repair](docs/runtime-postconditions.md)
- [Local execution, leases, attempts and migration](docs/local-execution.md)
- [Local daemon, live status, stopping and offline example](docs/local-daemon.md)
- [R08 local acceptance and supported environments](docs/local-acceptance.md)
- [Authenticated HTTPS service, remote schedulers and workers](docs/remote-service.md)
- [Durable event Inbox and callback matching](docs/event-inbox.md)
- [Durable write effects and gateway protocol](docs/durable-effects.md)
- [Ordered compensation and manual takeover](docs/ordered-compensation.md)
- [Consistent local backup and fenced recovery](docs/backup-recovery.md)
- [Durable run state, events, checkpoints and outbox](docs/run-store.md)
- [Deterministic kernel, bundle checks and replay](docs/kernel-semantics.md)
- [IR and decision semantics](docs/definition-semantics.md)
- [Generated JSON Schema](schemas/workflow-v1.schema.json)
- [Requirement review](examples/review.yaml), [parallel tests](examples/parallel-tests.json),
  [bounded repair](examples/bounded-repair.json) and its [body](examples/repair-round.json)
- [Architecture and issue delivery map](docs/roadmap.md)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bazel test //...
```

The tests run under Cargo and Bazel, cover malformed definitions and decisions,
and check the CLI, examples and committed schemas. Registry tests also race
independent processes and recover an interrupted SQLite transaction. Worker tests
reject protocol drift, changed authority, expired requests and malformed outputs
before accepting observations. Kernel tests cover branch arbitration, cancellation
and reconciliation, bounded iterations, logical deadlines, event conflicts and
checkpoint replay. RunStore tests force process termination around commit, race
independent writers, inject SQLite disk-full/read-only failures and verify complete
journal/checkpoint/outbox recovery. Execution tests race process ownership, fence
expired attempts, kill result writers around commit and execute actual compiler
capabilities through business decisions. Artifact tests interrupt uploads, reject
corrupt/missing/type-conflicting evidence and preserve references across local
export/import. `qualitygate.yaml` runs these
four commands against its captured delivery snapshot. No business benefit or
recovery SLA is claimed before the R15 benchmarks have been collected.

Shared storage: [PostgreSQL authority library and contract boundary](docs/postgres-authority.md).

共享存储的认证应用接口、角色、任务签发、撤销和审计边界见 [认证权威服务](docs/authenticated-authority.md)。

Authenticated shared artifact uploads/downloads and artifact-backed remote result
recovery are documented in [shared artifacts](docs/shared-artifacts.md). Transfers
use PostgreSQL storage, scoped credentials and current assignments.
