# Definition IR v1

This document specifies the first R01 increment. Implemented static behavior is
separated below from execution requirements that the later runtime must satisfy.

## Representation and identity

JSON, YAML and `WorkflowBuilder` produce the same `Workflow` struct. Unknown
struct fields, duplicate struct fields, unsupported schema versions and trailing
documents are rejected. A CLI document is at most 1 MiB; graphs have at most 4096
nodes and 16384 edges. Stable IDs are 1–128 ASCII letters, digits, `_`, `-` or `.`.
Node and edge IDs are unique within their respective namespaces.

Versions are opaque immutable release IDs with the same character set. Ranges,
`latest`, `head`, `main`, `master`, `stable` and `x` are rejected. This is a syntax
check: a registry must subsequently resolve the exact reference and verify its
content and capability contracts. A syntactically pinned version is not proof of
immutability or availability. Policy, capability and subworkflow references are
not executed or resolved by the compiler.

Canonical JSON sorts nodes by ID, emits typed struct fields in their declared
order, and sorts map keys. Edge order is preserved because first-match decisions
use it. The digest is `sha256:` followed by the SHA-256 of those UTF-8 bytes. This
is the workflow-v1 canonical form, not an RFC 8785 implementation. Workflow ID,
version, contracts and literals participate in the digest. Import/export retains
stable IDs and behavior; changing irrelevant source whitespace does not change
the digest. Validated content must be frozen before treating a digest as a
published version. The current API exposes canonicalization separately from
validation; the CLI emits a digest only after validation succeeds.

## Types and handoffs

A contract maps field names to a value type and `required` flag. Types are string,
boolean, integer, number, homogeneous array and closed object. All fields of a
nested object are required; top-level optional fields may be absent. Null is not
a value of any type. Integer can flow to number, with recursive compatibility for
arrays and objects. Numeric strings are not coerced.

Node input bindings select a workflow input, an earlier node output, or a typed
literal. Unknown inputs/outputs, missing required bindings, optional-to-required
bindings, future/self dependencies and incompatible types fail static validation.
Graph ancestry establishes possible ordering, not execution dominance: an output
on a skipped branch can still be unavailable. The execution layer must check
actual input presence and cannot substitute null or a fabricated success.

`validate_values` checks actual values independently and rejects undeclared fields,
missing required values and wrong types. Preconditions read the node's inputs.
Decision conditions also read inputs; binding a previous task output makes that
handoff explicit. Only task nodes should obtain external results from adapters;
control-node output semantics will be enforced by the execution layer.

## Graph and routes

The outer graph is acyclic, reachable from its entry, and every node must have a
path to an explicit terminal. This is a conservative structural check, not a proof
that every arbitrary business condition terminates. Every edge has an ID and a
route. Multiple incoming edges require a join or terminal; parallel scheduling,
join dominance and branch cancellation are execution-level checks still to come.

| Node | Static contract |
| --- | --- |
| `task` | Pinned capability and optional policy; exactly one `next` edge |
| `subworkflow` | Pinned workflow; exactly one `next` edge |
| `decision` | At least one `case`, exactly one `otherwise`; explicit mode |
| `fork` | At least two `next` edges with distinct destinations |
| `join` | At least two incoming edges; one `next`; explicit mode and remaining policy |
| `wait` | Positive timeout and event ID; exactly `accepted`, `rejected`, `timed_out` exits |
| `loop` | Pinned body, positive iteration limit and deadline; exactly `completed`, `exhausted` exits |
| `terminal` | `succeeded`, `failed` or `cancelled`; no outgoing edges |

A loop invokes a separately versioned body. The forthcoming runtime must allocate
a fresh node instance for each iteration, use the same pinned body for all rounds,
stop on successful completion, and retry a failed round only inside the specified
bounds. The deadline is measured across the whole loop and survives restarts.
Exhaustion uses the mandatory exit. Implicit graph backedges are rejected.
Subworkflow dependency cycles must also be rejected when the registry resolves
the complete definition bundle. That registry is not part of this increment.

## Implemented condition evaluator

Equality and inequality use structural JSON equality without coercion; integer
`1` and floating representation `1.0` are distinct. `exists` returns false for an
absent input. Comparing an absent value is an error. `all` and `any` require at
least one operand and short-circuit left to right, so `exists` can guard a later
comparison of an optional field. `not` negates one condition.

Before branch selection, supplied values are checked against the node contract.
All case expressions are evaluated in declared edge order; a condition error
fails selection. In `exclusive` mode, multiple true cases are an error. In
`first_match` mode the first true case wins. If no case matches, `otherwise` wins.
Malformed routing is rejected even if a matching case exists. Preconditions are
validated expressions; applying them to task lifecycle is runtime work.

## Required runtime semantics (not implemented here)

A skipped branch is distinct from successful work. An `all` join must wait for all
incoming branches to become terminal; failure/cancellation must not be counted
as success. An `any` join selects the first successful branch. With
`cancel_and_reconcile`, remaining branches receive cancellation and any uncertain
external effects must be reconciled; cancellation does not undo a committed
write. With `await`, their results must still be recorded. If no branch succeeds,
the join cannot report success. Ordering ties require a persisted deterministic
rule. These rules, wait event consumption, loop instances and terminal arbitration
need runtime contract tests before R01 is closed.

## Diagnostics

Static diagnostics carry `code`, `file`, `path`, optional `node`/`edge` IDs, and a
reason. Parse messages retain the parser's location and nested field path where
available. No model interpretation is needed to decide whether validation passed.
The validator has no file, clock, transport or provider dependency, so a service
can use exactly the same contract as the CLI. The deployment parity test is a
serialization round trip through that shared function, not a deployed-service test.
