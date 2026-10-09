# Workflow capability

Use the wrapper resolved by SKILL.md and read `workflow help`. Builtin read-only
capabilities need no provider configuration.

Read `workflow capability list` and `capability describe <id> <version>` for the
actual catalog, contracts, digest and usage. Use the exact version returned by the
catalog. Keep provider and transport configuration outside the business graph.
Read `workflow schema capability` for descriptor shape, and the request/grant/
result schemas when working on the worker protocol.

For a standalone call, write the exact typed inputs to a JSON file, then run
`workflow capability invoke <id> <version> <inputs.json>`. The built-in
`workflow.validate` and `workflow.canonicalize` accept `document` containing the
actual definition text and `format` set to `json` or `yaml`; a filesystem path is
not definition text. They have no model or external-write dependencies.

For `workflow.validate`, inspect `outcome.outputs.valid` even when the process
exits 0 and the outcome is `succeeded`. This means the inspection completed; an
invalid definition has diagnostics and no digest. Use `workflow.canonicalize`
only for a valid definition, and preserve the returned definition digest when
exporting. A failed outcome or rejected result must not become a success claim.

Use worker preparation/dispatch only when the task calls for that integration.
`worker prepare` creates a proposed standalone request; `prepare-node` checks the
definition, capability contract and preconditions for a host-provided attempt.
It does not create a run or acquire a lease. Obtain run, node-instance, attempt
and epoch values from the actual host context; do not invent them to make a
production request pass. A model-policy node must use its model adapter and
cannot be routed through direct capability invocation to bypass that policy.

`worker grant` explicitly approves an exact local read-only request. Use the
user's existing authorization and intended scope when deciding to issue it.
Keep the host grant separate from untrusted model/worker documents. A request
digest binds content, not identity or permission; a remote host must authenticate
the grant channel and consult current ownership before accepting results.

Use `worker dispatch <request.json> <grant.json>` and `worker check-result
<request.json> <grant.json> <result.json>` for transport integration. On digest,
protocol, expiry or authority errors, inspect the actual request and current host
context. Do not edit an old grant, substitute a newer epoch, or blindly reissue
work to conceal the mismatch. Request regeneration requires fresh host context
and remains subject to the task's existing authorization.

Result validation checks the invocation contract. It does not commit a run,
verify artifact existence by itself, prove a business gate passed or ensure exactly-once
execution. This standalone worker command accepts only read-only capabilities. Managed
writes use `run drive-effects` and the durable effect protocol in the [run guide](run.md). Do not describe an in-process timeout
as hard cancellation of arbitrary Rust code.

Report the exact capability/version, contract or request digest, outcome and any
remaining host-side verification. Stop at the requested invocation/integration;
use the [run guide](run.md) for requested durable state operations. Worker dispatch does not
commit the run. `run drive` supplies durable ownership and atomic result settlement
for local built-ins; use it when the task requires persistent workflow execution.


For worker evidence, the [artifact guide](artifact.md) publishes a typed manifest and
returns its portable ID/manifest digest. Attach that exact link only to the actual
producing request's result. The configured RunStore verifies bytes, lineage and
producer/input identity on finish and recovery. Source-revision declarations and
digests do not authenticate a producer or prove the report's business assertions.
