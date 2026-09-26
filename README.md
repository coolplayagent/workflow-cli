# workflow-cli

Workflow makes business SOPs explicit, reviewable and portable. It provides the
process contract around an agent: declared steps, typed handoffs, legal decisions
and evidence requirements. An LLM supplies node decisions; Skills explain how to
use capabilities; CLI/API adapters implement them. Provider and deployment choices
belong in bindings rather than in the business graph.

**Current implementation:** a Rust definition compiler and static validator for
version 1 of the IR. Execution, persistence and external capability resolution are
being delivered through the [issue roadmap](docs/roadmap.md). `validate` is a
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
an invalid report never has one. The same validation library can be called by a
remote service, which must revalidate the received definition.

```json
{"valid":false,"digest":null,"diagnostics":[{"code":"dangling_edge","file":"draft.json","path":"edges[0].to","node":null,"edge":"finish","message":"unknown node missing"}]}
```

Definitions are data: reading or validating one does not invoke its capabilities.
The draft editing commands, published-version registry and execution protocol are
subsequent deliveries; there is no `run` command in this first increment.

## Contracts and development

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
and check the CLI, examples and committed schema. `qualitygate.yaml` runs these
four commands against its captured delivery snapshot. No business benefit or
recovery SLA is claimed before the R15 benchmarks have been collected.
