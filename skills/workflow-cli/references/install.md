# Installation and runtime resolution

Download the versioned Linux x86_64 skill archive and SHA256SUMS from
https://github.com/coolplayagent/workflow-cli/releases. Verify the archive checksum
before extracting its `workflow-cli/` directory into the host's skills directory.
For Codex, the default destination is `~/.codex/skills/`. Use the host's configured
skill directory when it differs. Preserve executable permissions during extraction.

Release builds target Ubuntu 24.04 (glibc 2.39 or newer). The full runtime currently
requires Linux; workspace operations also require Git and `/proc`. Python 3 is
needed only for the supplied demonstrations. The bundled CLI does not need Cargo,
Bazel, PostgreSQL or model credentials for local builtin execution.

Run `<skill-root>/scripts/workflow.sh --version` and `help` from the task directory.
The wrapper checks the package's exact CLI version, prefers the bundled executable,
and falls back to a compatible executable on PATH if the bundled one cannot run.
Use an absolute `WORKFLOW_BIN` path when explicitly selecting another installation.
It does not download software or rewrite environment configuration automatically.

The source skill and published archives both contain the operating references,
`references/manuals/`, `assets/examples/` and `assets/schemas/`. Every local reading
link stays inside the skill directory. Read the [English contents](manuals/en/README.md)
or [中文目录](manuals/zh/README.md) only when the task needs detailed guidance.
Release archives additionally contain `manifest.json` and the compiled executable.
Copying the source skill alone preserves its reading resources; supply a matching
CLI through PATH or `WORKFLOW_BIN` for execution.

For a deterministic isolated demo after extracting a release:

```sh
python3 <skill-root>/assets/examples/execution/offline-demo.py \
  --workflow <skill-root>/scripts/workflow.sh --decision approve
```

This creates disposable state, runs real builtin validators, supplies an explicit
demo approval, verifies history/backup and stops its daemon. It invokes no model
or external business provider. Other examples may require a source checkout,
project-specific adapters or test services as documented in their manuals.

Keep databases, artifacts, workspaces, credentials and daemon control directories
outside the skill installation. To upgrade, retain the old CLI while verifying
new store compatibility. Storage schema upgrades require the explicit backed-up
procedure in [version migration](manuals/en/04-effects-and-recovery/05-version-migration.md); replacing a skill
directory does not migrate a run database or alter a frozen run definition.
