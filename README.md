# Workflow CLI

English · [中文](README.zh-CN.md)

Workflow CLI turns business SOPs into reviewable, durable processes. A definition
declares steps, typed handoffs, legal decisions and evidence requirements. An agent
uses one skill to operate the CLI; models propose bounded node decisions, adapters
perform work, and the runtime retains state, ownership and verified results.

Read [The Workflow CLI Book](docs/en/README.md) from installation to shared deployment,
or open the [documentation site](https://coolplayagent.github.io/workflow-cli/).
The [Chinese edition](docs/zh/README.md) follows the same chapters and examples.

## Install one skill

Download `workflow-cli-skill-v0.1.1-linux-x86_64.tar.gz` and `SHA256SUMS` from
[release v0.1.1](https://github.com/coolplayagent/workflow-cli/releases/tag/v0.1.1),
then run in the download directory:

```sh
sha256sum --check SHA256SUMS
mkdir -p ~/.codex/skills
tar -xzf workflow-cli-skill-v0.1.1-linux-x86_64.tar.gz -C ~/.codex/skills
~/.codex/skills/workflow-cli/scripts/workflow.sh version --format json
```

Use your agent host's configured skill directory if it differs. The archive
contains the matching CLI, [task references](skills/workflow-cli/SKILL.md), complete
English/Chinese manuals, examples, schemas and file digests. Progressive reading
stays inside the extracted package. The release supports Linux x86_64 with
Ubuntu 24.04 / glibc 2.39 or newer; it does not provide macOS, Windows or ARM
binaries. See [installation and upgrade](docs/en/01-getting-started/02-skill-distribution.md).

## Execute a real local example

Python 3 is needed for this demonstration; no compiler, model account or remote
service is required:

```sh
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision approve
```

The example runs two actual built-in validation tasks, supplies the explicit demo
operator decision, checks history and backup, and stops its daemon. Repeat with
`--decision reject` to observe cancellation. Follow the
[first workflow chapter](docs/en/01-getting-started/03-getting-started.md) to inspect durable progress and
prove a second drive does not repeat committed tasks.

CLI exit success and business success are separate: inspect
`result.snapshot.status` after a mutation and `result.status` after a status read.
Static `validate` reports `valid` and diagnostics; it neither executes a process
nor authorizes a capability.

## What is implemented

- Typed JSON/YAML definitions, static validation, revisioned drafts and immutable publication.
- Deterministic control flow, checkpoint replay, transactional state/event/outbox/Inbox storage, leases and fenced results.
- Typed artifacts, isolated attempt workspaces, exact provenance, mandatory evidence gates and bounded repair.
- Bounded model policies, explicit host bindings and recorded decisions; durable external effects, reconciliation and ordered compensation.
- Local daemon, verified backup and explicit migration; authenticated HTTPS workers, PostgreSQL authority and shared scheduling.
- Reviewed SOP templates with local and TLS acceptance experiments.

Start with the [book's reading paths](docs/en/01-getting-started/01-preface.md). Acceptance chapters explain
tested environments and limits; the [delivery map](docs/en/06-acceptance-and-maintenance/08-roadmap.md) retains the
remaining work, including business-value benchmarks. Models, business adapters
and shared services require their own bindings only when a workflow uses them.

## Develop and verify

Cargo and Bazel compile the same Rust sources with pinned toolchains. From a source
checkout:

```sh
cargo run --locked -- validate examples/review.yaml
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bazel test //...
bazel run //:workflow -- validate "$PWD/examples/review.yaml"
python3 -m pip install -r website/requirements.txt
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts/tests -v
```

Bazel resolves relative inputs in its execution directory, so pass absolute input
paths. After changing Cargo manifests or `Cargo.lock`, update and review the
Bazel lock with `bazel mod deps --lockfile_mode=update`. Verification rejects stale
locks. The existing Qualitygate policy runs formatting, Clippy, Cargo and Bazel;
CI additionally checks documentation, the extracted skill and real shared-mode
acceptance. See [contributing and diagnostics](docs/en/06-acceptance-and-maintenance/07-troubleshooting.md) and
[release verification](docs/en/01-getting-started/02-skill-distribution.md).

Licensed under [MIT](LICENSE).
