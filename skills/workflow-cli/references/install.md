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

The source skill contains the operating references and wrapper; published archives
add `references/manuals/`, `assets/examples/`, `assets/schemas/`, `manifest.json`,
and the compiled executable. Build those archives with the repository's
`scripts/package_skill.py`. In a source checkout, the corresponding manuals and
examples live in `docs/` and `examples/`.

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
procedure in [version migration](manuals/version-migration.md); replacing a skill
directory does not migrate a run database or alter a frozen run definition.
