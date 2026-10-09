# Your first durable workflow

The approval example inspects a definition with real built-in validators, waits
for an explicit operator decision, and verifies the resulting history and backup.
Use it to learn the difference between a command completing and the business
process succeeding. Complete [installation](02-skill-distribution.md) first; Python 3
is needed for the demonstration driver.

## Run both business outcomes

From any working directory on a supported Linux host:

```sh
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision approve
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision reject
```

Substitute your actual skill location if it differs. Each invocation creates its
own temporary stores and daemon, runs the validators, supplies the selected demo
decision, verifies the backup, and stops the daemon. `approve` ends in business
success; `reject` ends in cancellation. Neither invocation calls a model or an
external business provider. The supplied decision belongs only to this disposable
demonstration; a live approval must come from its authorized operator.

## Inspect a smaller run step by step

For a persistent exercise, create a fresh directory and keep it until you have
finished inspecting the history. The following shell function invokes the packaged
runtime without changing the current directory:

```sh
workflow_skill="$HOME/.codex/skills/workflow-cli"
workflow() { "$workflow_skill/scripts/workflow.sh" "$@"; }
workflow_demo=$(mktemp -d)
workflow run init "$workflow_demo/runs.db"
workflow run start "$workflow_demo/runs.db" \
  "$workflow_skill/assets/examples/execution/valid-start.json"
workflow run drive "$workflow_demo/runs.db" inspect-valid learner 10
workflow run status "$workflow_demo/runs.db" inspect-valid
workflow run execution-history "$workflow_demo/runs.db" inspect-valid 0 100
workflow run verify "$workflow_demo/runs.db" inspect-valid
workflow run drive "$workflow_demo/runs.db" inspect-valid learner 10
```

The first drive executes one validator task. Its mutation result contains
`result.snapshot.status` equal to `succeeded`; the separate status read uses
`result.status`. The last drive executes zero tasks because the earlier result is
already committed. `verify` replays retained facts without calling the validator
again. Keep the database path when continuing the exercise; the CLI has no implicit
default run store.

Now start `assets/examples/execution/invalid-start.json` in the same database and
drive run `inspect-invalid`. The validator itself returns a valid observation with
`valid=false`; the process selects `failed`. CLI exit code 0 does not turn that
business outcome into success.

## Change the definition, then validate it

Read the bundled [review definition](../../../../assets/examples/review.yaml). Copy it to your
working directory before editing; installed examples are versioned inputs. Run
`workflow validate <your-file>` and inspect `valid`, `diagnostics` and `digest`.
A missing target node produces a field-level diagnostic and no digest. Correct
the edge and validate again. This static operation does not start a run or grant
approval. The next part explains typed handoffs and immutable publication.

## Check your understanding

You should now be able to identify the run ID, the exact business terminal, the
number of newly executed tasks, and the retained evidence for the second drive.
If anything differs, follow [troubleshooting](../06-acceptance-and-maintenance/07-troubleshooting.md) with the complete
CLI error, run status and execution history before retrying.

<!-- book-navigation -->

1.3 Your first durable workflow

[Book contents](../README.md) · [1. Getting started](README.md) · [中文](../../zh/01-getting-started/03-getting-started.md) · [Previous: 1.2 Install the skill](02-skill-distribution.md) · [Next: 2.1 Definitions and types](../02-process-definition/01-definition-semantics.md)

<!-- /book-navigation -->
