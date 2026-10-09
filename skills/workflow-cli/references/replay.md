# Workflow replay

Use the wrapper resolved by SKILL.md and read `workflow help`.
Read `workflow schema kernel-bundle` and `kernel-scenario` for current formats.
The repository guide at [manual](manuals/en/02-process-definition/04-kernel-semantics.md) defines transition semantics.

Obtain exact workflow definitions and capability descriptors from the task's
catalog. Keep provider credentials and storage configuration outside the bundle.
Run `workflow kernel check <bundle.json>`. Resolve missing references and contract
mismatches from real definitions; model-policy errors require a policy adapter,
not removing policy constraints. A checked bundle establishes contract consistency,
not permission to execute its capabilities.

For an explicitly requested simulation, write a scenario with the checked bundle,
run ID, typed inputs, positive logical start time and supplied events. Begin with
an empty event list to inspect the initial snapshot and commands. Label simulated
task outputs as simulated; never use them as evidence that real work ran or that
an approval was granted. Repository examples under `assets/examples/kernel` are simulations.

Run `workflow kernel replay <scenario.json>` and inspect `snapshot.status`, node
states, reasons, winner edges and command intents. Exit 0 only means evaluation
succeeded. Failed/cancelled business outcomes must be reported as such. Rejected
inputs/transitions exit 1; usage and I/O errors exit 2. A requested task command
is not an execution receipt.

For checkpoint work, extract the returned `checkpoint` object into a new JSON
file. `kernel restore <bundle.json> <checkpoint.json>` verifies it and emits no
historical commands. Do not edit its journal or recompute checksums to hide an
integrity failure. Obtain the original bundle/checkpoint and investigate mismatch.

Read `workflow schema kernel-event` before forming the next event. Use the actual
snapshot run ID, run digest, expected revision and instance ID. Use a stable event
ID and nondecreasing logical timestamp. In real host integration, event facts must
come from authenticated, validated observations; do not invent them. Apply with
`kernel apply <bundle.json> <checkpoint.json> <event.json>` and save the next
checkpoint to a different path. Never redirect output onto an input file.

An exact event retry is deduplicated with no commands. On a revision conflict,
inspect the current checkpoint and intended event; do not blindly update the
revision and resubmit. On an expired signal, use a separately ordered advance-time
event to settle timeout. Uncertain task results require definite reconciliation;
a cancellation request alone cannot be reported as a settled cancellation.

Report bundle/run digests, final revision/status, observed branches and any
pending task, signal, timer or reconciliation intent. The kernel does not dispatch
these intents or provide durable run ownership. Stop at the requested replay or
analysis; actual execution requires the host's persistence and authority boundary.
