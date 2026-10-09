# Troubleshooting and contributing

Start diagnosis with an observation, not a new write. Preserve the exact command,
exit status and JSON error; identify the installed CLI version, run ID, database
and binding paths. Redact secrets before sharing logs. Use `workflow help` and
`workflow schema <kind>` to check the command and input shape for that version.

## Follow the symptom

| Symptom | Inspect next | Recovery boundary |
| --- | --- | --- |
| Wrapper reports no compatible runtime | `VERSION`, host OS/architecture, `WORKFLOW_BIN`, executable permissions | Install the matching release; do not mix skill and CLI versions |
| Static validation fails | `diagnostics[].path`, node/edge identity, exported schema | Edit the draft and validate; no run has been executed |
| Drive exits 0 but business work is unfinished | `run status`, pending commands, waits, pause and gate state | `idle` or `UNKNOWN` can be a legitimate wait |
| Lost reply after a write | Execution history and durable effect ledger | Query/reconcile the original operation before retrying |
| Stale revision or expired ownership | Current run revision and execution history | Acquire current authority; never forge a new worker observation |
| Missing or corrupt evidence | Artifact store/binding, payload and manifest identity | Restore verified bytes; a fabricated PASS cannot replace them |
| HTTPS request denied | TLS trust, credential scope/role, revocation and current assignment | Correct authorized host configuration, retain the denial audit |
| Restored run cannot write | Recovery hold, provider observations and source audit | Complete explicit reconciliation before resuming |

For local state, query and replay without invoking adapters:

```sh
workflow run status /absolute/path/runs.db run-id
workflow run execution-history /absolute/path/runs.db run-id 0 100
workflow run verify /absolute/path/runs.db run-id
```

Follow `next_cursor` to inspect additional execution history. If artifacts are part
of the run, supply the same `run --artifacts <store>` configuration used during
execution. A database owner may need writable access even for ordinary SQLite
reads so crash recovery can finish; a copied live database file is not a verified
backup. Read [backup and recovery](../04-effects-and-recovery/04-backup-recovery.md) before relocating state.

## Maintain both editions and the offline manual

English and Chinese live under `docs/en/` and `docs/zh/`, with identical
`NN-topic/NN-chapter.md` paths. Each language and volume has a `README.md` index. `docs/book.json` declares the single reading order and
chapter labels. Both editions contain complete operating guidance. When changing
a contract, update both chapters and their examples in the same change.

`python3 scripts/book.py` refreshes Markdown contents and chapter navigation;
`--check` verifies them without writing. `python3 scripts/check_docs.py` checks
chapter parity, local destinations and heading fragments in the README, book and
skill. After updating the book, run `python3 scripts/sync_skill_resources.py`
to refresh the committed manuals, examples and schemas inside the skill. Source
and installed skill links must stay within the skill directory. CI rejects stale
copies and external checkout dependencies. Run the regression suite before packaging:

```sh
python3 -m pip install -r website/requirements.txt
python3 scripts/book.py --check
python3 scripts/sync_skill_resources.py --check
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts/tests -v
python3 scripts/build_site.py --output /tmp/workflow-book-preview
```

Use an empty output directory. Serve the rendered preview with
`python3 -m http.server --directory /tmp/workflow-book-preview 8000` and check both
languages, chapter switching, mobile navigation and fragment links. The site uses
the same chapter order as the Markdown book. Existing flat documentation URLs
continue to lead to the corresponding English chapter.

## Verify a change before release

Build and test the same Rust sources through Cargo and Bazel:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bazel test //...
```

After changing Cargo manifests or the lockfile, run
`bazel mod deps --lockfile_mode=update` and review `MODULE.bazel.lock` before the
checks. Shared acceptance additionally requires the real PostgreSQL/TLS fixtures
configured in CI. Local tests that skip those cases do not prove shared behavior.

Follow [release packaging](../01-getting-started/02-skill-distribution.md) to build and exercise the actual
archive outside the checkout. The release workflow requires successful CI for
the exact main-branch commit before publishing its matching tag. After publishing,
download the release, check SHA256SUMS, rerun archive verification and inspect the
deployed book's version/revision metadata. A local archive alone does not prove
that the published asset or deployed documentation is correct.

<!-- book-navigation -->

6.7 Troubleshooting and contributing

[Book contents](../README.md) · [6. Acceptance and maintenance](README.md) · [中文](../../zh/06-acceptance-and-maintenance/07-troubleshooting.md) · [Previous: 6.6 Security acceptance](06-security-acceptance.md) · [Next: 6.8 Architecture and delivery map](08-roadmap.md)

<!-- /book-navigation -->
