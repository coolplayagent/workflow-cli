# Test

This directory is governed by `codespec/codespec-map.yaml`. Update its map entry through `relay-knowledge map directory` and keep reviewed source material within the declared content scope.

R01 definition acceptance is mapped in [definition-acceptance](../../docs/en/06-acceptance-and-maintenance/01-definition-acceptance.md).
The mandatory CI runs the actual CLI authoring/replay baseline and HTTPS CLI parity,
in addition to the full Rust diagnostic and immutable-publication matrix. Preserve
raw outputs with the final commit; simulated events and CLI latency are not
production task completion or human productivity measurements.

R05 effect acceptance is mapped in [remote-effects](../../docs/en/05-shared-execution/05-remote-effects.md).
The mandatory PostgreSQL suites include real HTTPS worker/scheduler termination
after provider commit, single delivery races, query recovery, scoped policy
rejection, bounded retries, cancellation and authenticated compensation recovery.
