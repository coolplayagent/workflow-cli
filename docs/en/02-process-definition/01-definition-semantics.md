# Definition IR v1

This document specifies R01 static semantics. The [definition registry](02-definition-registry.md)
adds revisioned editing and immutable publication. The [kernel](04-kernel-semantics.md) adds bundle checks, deterministic control flow
and checkpoint replay. The [RunStore](../03-execution-and-evidence/01-run-store.md) persists these transitions;
external execution and authority remain host work.

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
the digest. The registry freezes validated content before treating a digest as a
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
handoff explicit. Task and wait results use declared output contracts. The kernel checks child
return contracts for subworkflow/loop nodes and requires other control outputs to
be empty. Successful terminal inputs become the workflow return values.

## Graph and routes

The outer graph is acyclic, reachable from its entry, and every node must have a
path to an explicit terminal. This is a conservative structural check, not a proof
that every arbitrary business condition terminates. Every edge has an ID and a
route. Multiple incoming edges require a join or terminal; the kernel settles
edge tokens and validates closed branch regions before cancelling losing branches.

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

A loop invokes a separately versioned body. The kernel allocates fresh node
instances for each iteration, pins the body for all rounds, stops on successful
completion and retries failed rounds inside both bounds. The logical deadline
is measured across the entire loop and preserved by checkpoint replay. Exhaustion
uses the mandatory exit after child cancellation settles. Implicit graph backedges
and recursive subworkflow/loop dependencies in the supplied bundle are rejected.

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
validated expressions; the kernel skips a node on false and fails it on an error.

## Kernel execution semantics

A skipped branch is distinct from successful work. An `all` join waits for all
incoming tokens; failed or cancelled tokens prevent success. Skipped tokens are
neutral when at least one branch succeeds; if all skip, the join skips. An `any`
join selects the first successful token by deterministic token sequence. With
`cancel_and_reconcile`, a verified closed fork region receives cancellation and
uncertain results require a definite reconciliation event. With `await`, remaining
results are recorded. The frame completes only after every node settles, including
losers. No successful branch means the join cannot succeed.

See [kernel semantics](04-kernel-semantics.md) for wait consumption, terminal
arbitration, data availability and event order. Unit tests and CLI replay cover
these transitions. RunStore tests persist waits and deadlines, and authenticated
HTTPS execution compares local and remote business state after scheduler loss.
Actual external cancellation and provider reconciliation retain their separate
runtime acceptance. See the [R01 acceptance map](../06-acceptance-and-maintenance/01-definition-acceptance.md).

## Diagnostics

Static diagnostics carry `code`, `file`, `path`, optional `node`/`edge` IDs, and a
reason. Parse messages retain the parser's location and nested field path where
available. No model interpretation is needed to decide whether validation passed.
The validator has no file, clock, transport or provider dependency. Both CLI and
HTTPS `validate_definition` use `validate_source` and `ValidationReport`; a real
HTTPS/PostgreSQL matrix compares complete reports, including field paths and IDs.
Reports cap diagnostic count and encoded bytes without accepting invalid input.
See [validation bounds and reproducible evidence](../06-acceptance-and-maintenance/01-definition-acceptance.md).

<!-- book-navigation -->

2.1 Definitions and types

[Book contents](../README.md) · [2. Define the process](README.md) · [中文](../../zh/02-process-definition/01-definition-semantics.md) · [Previous: 1.3 Your first durable workflow](../01-getting-started/03-getting-started.md) · [Next: 2.2 Author and publish](02-definition-registry.md)

<!-- /book-navigation -->
