# Install and publish the workflow skill

One `workflow-cli` skill provides the complete CLI entrypoint. The release archive
includes task references, the complete English/Chinese book, examples, schemas, a file-digest manifest and
the matching executable. Internal Rust crates remain implementation modules.

## Install

Download `workflow-cli-skill-v0.1.1-linux-x86_64.tar.gz` and `SHA256SUMS` from the
[release page](https://github.com/coolplayagent/workflow-cli/releases/latest).
In the download directory:

```sh
sha256sum --check SHA256SUMS
mkdir -p ~/.codex/skills
tar -xzf workflow-cli-skill-v0.1.1-linux-x86_64.tar.gz -C ~/.codex/skills
~/.codex/skills/workflow-cli/scripts/workflow.sh version --format json
```

Use the configured skill directory for other agent hosts. For an upgrade, retain
the old installation and CLI before replacing it, and check storage compatibility.
The wrapper resolves its own installation path and preserves the task's working
directory. It prefers the bundled binary, permits an explicit `WORKFLOW_BIN`, and
accepts a PATH fallback only when its CLI version matches the skill exactly.

The full release supports Linux x86_64, Ubuntu 24.04 / glibc 2.39 or newer.
Workspaces additionally require Git and `/proc`; Python 3 runs the demos. Local
builtin execution requires no compiler, model account or remote database.
macOS, Windows and Linux ARM packages are not provided by this release.

## Run a local workflow

```sh
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision approve
```

The demo uses temporary stores, executes two real builtin validation tasks, submits
an explicit demo operator decision, verifies history and backup, and stops its local
daemon. Repeat with `--decision reject` to observe cancellation. No model or external
business provider is invoked. Other fixture adapters and integration suites can
require a source checkout or test services; inspect their prerequisites first.

The eight former skills are now task references inside the single entrypoint.
`workflow-capability@1.0.0` remains in builtin descriptors to preserve existing
digests; it resolves conceptually to the capability reference, with no second
installation. Documentation reorganization does not change stored contracts.

## Build and verify the release

```sh
python3 -m pip install -r website/requirements.txt
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts/tests -v
cargo build --release --locked -p workflow-cli
python3 scripts/package_skill.py --binary target/release/workflow --output dist --tag v0.1.1
python3 scripts/verify_skill.py dist/workflow-cli-skill-v0.1.1-linux-x86_64.tar.gz
```

The verifier extracts into a temporary path containing spaces, checks all manifest
digests, recursive local Markdown links and heading fragments, runs CLI schema/validation and real successful
and failed workflows from an unrelated directory, proves settled tasks do not
execute again, and tests daemon approval/rejection, backup and runtime resolution.
It never uses the source checkout's CLI or resource paths. Packaging refuses a
version/tag/platform mismatch, a missing local resource or an existing archive.
Source skill links resolve in the checkout. Packaging maps those destinations
into `references/manuals/` and `assets/`, including transitive manual, language,
example and schema links; it cannot replace a missing resource with a GitHub URL.
The book reading order comes from `docs/book.json`. Both Markdown editions and
Pages expose chapter navigation and language switching; previous flat Pages URLs
redirect to the same English chapter and preserve its fragment.

The `Release skill and Pages` GitHub Actions workflow requires a successful `CI`
push run for the exact main-branch commit. Push its matching `v*` tag after CI
passes. It builds on Ubuntu 24.04, packages and verifies the extracted skill,
publishes the archive and SHA256SUMS to GitHub Releases, then deploys the generated
documentation through the official GitHub Pages artifact/deployment actions.
The repository Pages source must be **GitHub Actions**.

Release archives pin `source_revision` and every file digest in `manifest.json`.
Skill/CLI versions must advance together when publishing a changed artifact.
Definitions, capabilities, policies and database schemas retain their independent
versioning contracts. Binary replacement does not upgrade a database; follow
[version migration](version-migration.md) when an upgrade is needed.

<!-- book-navigation -->

[Contents](README.md) · [中文](zh/skill-distribution.md) · [Previous: How to read this book](preface.md) · [Next: Your first durable workflow](getting-started.md)

<!-- /book-navigation -->
