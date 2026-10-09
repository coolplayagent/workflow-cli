# Deterministic workflow kernel

`workflow-kernel` is a Rust library and explicit Bazel `rust_library`. It owns
legal workflow transitions. `start` and `apply` calculate a new state and ordered
command intents without files, network access, a system clock or a provider SDK.
The CLI exposes the same reducer for bundle checks and reproducible simulations.

## Try the scenarios

```sh
cargo run --locked -- kernel replay examples/kernel/review-approved.json
cargo run --locked -- kernel replay examples/kernel/review-rejected.json
cargo run --locked -- kernel replay examples/kernel/parallel-all.json
cargo run --locked -- kernel replay examples/kernel/parallel-any-cancel.json
cargo run --locked -- kernel replay examples/kernel/repair-third-round.json
```

All five scenarios supply **simulation contracts and simulated task results**.
They do not invoke coding/test adapters. Review approval succeeds; rejection is
cancelled; parallel all waits for both results; parallel any waits for the losing
branch's cancellation reconciliation; repair fails two rounds then succeeds on
round three. Changed parallel/repair definitions use version `2.0.0`.

Read the bundled [replay guide](../../../skills/workflow-cli/references/replay.md) for
the agent loop. `workflow schema kernel-bundle`, `kernel-event`, `kernel-scenario`
and `kernel-checkpoint` expose the input formats. A successful CLI evaluation exits
0, even when `snapshot.status` is `failed` or `cancelled`. Rejected inputs or
transitions exit 1; usage and I/O errors exit 2.

For a stepwise session, extract `bundle` from a scenario, then replay a copy with
an empty `events` array. Save the returned `checkpoint` object to its own file.
`kernel restore <bundle.json> <checkpoint.json>` verifies and replays it without
returning historical commands. `kernel apply <bundle.json> <checkpoint.json>
<event.json>` returns the next snapshot, transition and checkpoint. Put the
snapshot's actual `run_id`, `run_digest` and `revision` into the event, with a fresh
stable event ID and the intended timestamp. Always write output to a new path;
shell redirection over an input truncates it before the CLI can read it. The CLI
does not save a run or dispatch any of the returned commands.

## Bundle compilation and identity

A bundle contains a root reference, workflow definitions and capability
descriptors. Compilation checks every definition, exact ID/version references,
task contracts, child input/return contracts and effect query/compensation
references. Duplicate releases, missing bindings and recursive child/loop
references are rejected. Model-policy tasks resolve a frozen model policy and
exact task/tool contracts from the bundle; direct invocation cannot bypass their
policy. Provider execution is outside the reducer; see [model execution](../03-execution-and-evidence/04-model-execution.md).

Successful terminal nodes in a workflow must declare identical **input**
contracts. Their resolved inputs are the workflow's return values. A child or
loop node's inputs equal the child's workflow inputs, and its outputs equal that
return contract. Task/wait nodes receive typed result values; other control nodes
have empty output contracts. Only a succeeded producer supplies a node-output
binding. Missing actual values fail the consumer, including a losing `any` branch
whose outputs are not yet available.

Workflow/node ordering is canonical; edge order remains significant. Bundle
identity includes every supplied workflow and descriptor. Frames record exact
definition digests; task commands carry exact capability versions and contract
digests. `run_digest` binds bundle, run ID, initial inputs, start time and limits.
It rejects accidental event delivery between distinct run seeds. The host must
still enforce unique run IDs and verify external adapter availability. Supplied
contracts and digests do not establish authorization or adapter authenticity.

## Control flow

| Construct | Transition semantics |
| --- | --- |
| Sequence/task | Resolve typed inputs and preconditions, then emit `execute_task`. Accept typed success or a declared failure code. False preconditions skip; expression errors fail. |
| Decision | Evaluate cases in definition edge order using the shared evaluator. Exclusive multiple matches and missing comparisons fail. First-match uses the first true case; otherwise uses the default. Unselected edges skip. |
| Fork | Select all outgoing edges. Node activation uses stable frame and node order. |
| All join | Wait for every incoming token. Failure precedes cancellation; either prevents success. Skipped tokens are neutral if another succeeds. All skipped means skipped. |
| Any join | Select the first successful incoming token by its monotonic settlement sequence. Accepted event order determines external-result races; edge order and canonical traversal break internal ties. If no branch succeeds, it cannot succeed. |
| Wait | Emit `await_signal` with one absolute logical deadline. A matching accepted signal supplies typed outputs; rejection requires empty outputs. Consume once and route accepted/rejected/timed-out. |
| Subworkflow | Create a frame with typed inputs and fresh instances; propagate settled child status and return values. |
| Loop | Capture inputs once, keep the same pinned body and whole-loop deadline, allocate a fresh frame/instances per round. Success exits completed. Failed rounds retry within the iteration bound; exhaustion exits exhausted after outstanding child work settles. |
| Terminal | Record its declared outcome. A frame waits for all nodes, including losing branches. Active failed terminals precede cancelled terminals and successes; conflicting successful return values fail. A frame with all terminals skipped fails. |

An `any` join with `await` can release its successor while other branches run,
but the frame retains their results and waits for them to settle. A losing
branch's failure does not invalidate an already selected success unless it reaches
an independently active failed terminal.

`cancel_and_reconcile` requires a closed fork region: disjoint branches, one
incoming join edge per branch, no shared internal nodes, outside entrances or
escaping terminal paths. Ambiguous regions fail compilation. The winner cancels
other groups. Issued tasks receive `cancel_task`; they remain outstanding until a
definite result. `uncertain`, or an error declared as `unknown_effect`, emits `reconcile_task`
and requires a definite
`task_reconciled` result. A late success after cancellation remains recorded but
cannot advance the cancelled branch. Cancellation does not undo external writes.

A global cancel stops pending work, cancels timers and children, and waits for
issued tasks/reconciliation before the run becomes cancelled. Ordered intents can
include execute then cancel in one transition if a sibling immediately wins;
the host must preserve this order and reconcile whether work was dispatched.

## Events, time and replay

Events are trusted host facts. They carry a stable ID, run binding, expected
revision and explicit monotonic timestamp. The host must validate worker results,
permissions, approvals and artifacts before forming one. An in-memory engine is
not a concurrent storage transaction or an untrusted worker ingress.

An exact repeated event returns `duplicate: true` and no commands, including after
termination. Reusing its ID for changed content is an event conflict. A fresh
stale revision fails. New events cannot reopen a terminal run. Any rejected
transition leaves state, journal and commands unchanged.

Ordinary events first expire all deadlines at or before their timestamp. A task
result at a loop deadline cannot make the loop succeed. An already expired signal
is rejected atomically; submit a separate `advance_time` event to persist the
timeout transition. An explicitly ordered global `cancel` takes precedence over
timer expiry in that event. No wall-clock polling happens inside the kernel.

A checkpoint stores the run seed and accepted event journal, bundle digest,
replayed state digest and a checksum over the entire checkpoint content. Restore
rebuilds from the seed and silently replays events, then verifies the state digest.
It preserves node instances, winners, deadlines, cancellation and deduplication.
The checksum detects corruption; it is not authentication against an actor able
to rewrite the checkpoint and recompute hashes. There is no checkpoint migration
or arbitrary snapshot injection API in this version.

## Bounds and host integration

Strict JSON parsing rejects duplicate keys, unknown fields and inputs over 2 MiB.
Bundles allow 128 workflows, 256 descriptors, 4096 total nodes and 16384 edges.
Each workflow also meets the compiler's individual size/depth limits. Default run
limits are 256 frames, 16384 node instances, 100000 microtransitions and 4096
events; configurable upper bounds are 4096, 100000, 1000000 and 10000. Historical
frames are retained. Snapshot and checkpoint sizes each remain bounded at 2 MiB;
the aggregate CLI response must also fit 2 MiB. Oversized histories fail explicitly.
Global cancellation and its settlement can exceed normal event/transition budgets,
while hard serialized-size limits still apply.

A durable host must atomically commit the accepted event, new revision/checkpoint
and ordered command outbox, then dispatch from that outbox with stable command
identity, e.g. `(run_id, revision, command_index)`. It must also supply live leases,
fencing, authenticated event ingress and recoverable timer delivery. Replay must
not dispatch historical commands. The [SQLite RunStore](../03-execution-and-evidence/01-run-store.md) implements
atomic state/event/outbox commits and verified recovery; the [local executor](../03-execution-and-evidence/02-local-execution.md)
adds run leases, fenced attempts and bounded read-only retries. Model adapters
produce explicit records checked by the durable host. [Durable effects](../04-effects-and-recovery/02-durable-effects.md)
and [ordered compensation](../04-effects-and-recovery/03-ordered-compensation.md) use separate host adapters.
Kernel replay itself remains a pure calculation; neither layer claims exactly-once
effects, business benefits or recovery SLAs.

<!-- book-navigation -->

2.4 Control flow and replay

[Book contents](../README.md) · [2. Define the process](README.md) · [中文](../../zh/02-process-definition/04-kernel-semantics.md) · [Previous: 2.3 Capabilities and workers](03-worker-protocol.md) · [Next: 2.5 Reviewed SOP templates](05-reviewed-templates.md)

<!-- /book-navigation -->
