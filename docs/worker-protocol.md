# Capability invocation and worker protocols

This R02 increment provides a working read-only capability boundary and the JSON
contract shared by local and remote worker hosts. It includes two actual
compiler capabilities. A worker produces checked observations; the RunStore and
runtime commit runs, arbitrate transitions and fence concurrent owners. Worker commands do not start a run; the separate
[run storage CLI](run-store.md) persists state and command intents.

## Independent components

- `workflow-worker` owns capability descriptors, strict JSON codecs, request and
  result validation, the `CapabilityAdapter` port and in-process dispatcher. It
  depends on the portable IR and validator, with no database, filesystem, network
  or provider SDK dependency. Clock access has an injectable port.
- `workflow-builtin-capabilities` implements the port with the existing compiler.
  Adapters receive immutable requests and return structured outcomes. They receive
  no run state writer or transition callback.
- `workflow-cli` composes the host and adapters. CLI file transport invokes the
  same `dispatch_json` path available to a remote transport implementation.

Each crate has its own Bazel `rust_library` and `rust_test`. This chapter describes the JSON
protocol boundary and local file transport. [Shared HTTPS execution](remote-service.md)
adds authenticated transport, scheduling and result admission.

## Discover and invoke

Run from the repository root after `cargo build --locked`, or use an installed
`workflow` binary. The following examples use `target/debug/workflow`; a Bazel
build also provides `bazel-bin/crates/workflow-cli/workflow`.

```sh
target/debug/workflow capability list
target/debug/workflow capability describe workflow.validate 1.0.0
target/debug/workflow capability invoke workflow.validate 1.0.0 examples/worker/inputs.json
target/debug/workflow capability invoke workflow.canonicalize 1.0.0 examples/worker/inputs.json
```

Both capabilities take `document` (a string, not a path) and `format` (`json` or
`yaml`). They read no files, access no models and perform no external writes.

`workflow.validate` returns an inspection outcome with `valid`, `diagnostics` and
an optional definition `digest`. A well-executed inspection can return
`status: succeeded` and `valid: false`; the caller must check `valid` before using
the definition. No digest is returned for an invalid definition. Diagnostic
`node` and `edge` are empty strings when absent because IR v1 contracts exclude
null. `workflow.canonicalize` requires validity and returns canonical JSON in
`document` and its definition digest; otherwise it returns a declared failure.

Exit 0 means a checked successful capability outcome, exit 1 means a rejected
request/result or declared capability failure, and exit 2 means usage or file I/O.
These commands emit compact JSON so their wire output respects the byte bound.

## Request, approval, dispatch, acceptance

Use a fresh temporary directory. Run this block together within the example's
60-second deadline:

```sh
worker_demo_dir=$(mktemp -d)
target/debug/workflow worker prepare workflow.validate 1.0.0 examples/worker/standalone-job.json > "$worker_demo_dir/request.json"
target/debug/workflow worker grant "$worker_demo_dir/request.json" > "$worker_demo_dir/grant.json"
target/debug/workflow worker dispatch "$worker_demo_dir/request.json" "$worker_demo_dir/grant.json" > "$worker_demo_dir/result.json"
target/debug/workflow worker check-result "$worker_demo_dir/request.json" "$worker_demo_dir/grant.json" "$worker_demo_dir/result.json"
```

`prepare` resolves an exact locally registered capability. A job has `request_id`,
`trace_id`, `timeout_ms`, `inputs` and optional `attempt`. Standalone jobs cannot
claim an attempt. The CLI sets the issue time and deadline from the host clock;
timeout must be positive and no larger than the descriptor's timeout. `prepare`
only proposes a request and does not invoke an adapter.

`grant` is explicit local approval of that exact read-only request. It checks
shape, current time, capability identity/digest and actual inputs, then emits a
request-bound grant. Protect the grant channel and files as host authority. A
digest is not a signature, authentication credential or proof of a live lease.
Anyone with permission to issue or replace a grant can authorize a request.
A remote service must authenticate its controller, obtain a current host grant
independently of worker/model data, and recheck it before committing state. The
request format rejects an embedded grant, undeclared state fields and transition
commands. The Rust `ExecutionGrant::bind` helper binds bytes; it does not perform
authorization by itself.

`dispatch` verifies the request and host grant before any adapter call, checks the
outcome against the registered descriptor, and emits a result bound to the whole
request digest. `check-result` revalidates an independently received result with
the current host grant and clock. An `AcceptedResult` is a validated observation,
not an authoritative run completion or independently verified business result.
The host must still verify artifact availability, gates and persisted ownership.
The [local executor](local-execution.md) supplies durable ownership and result
settlement; the [artifact reader](artifacts.md) validates manifest/content/type
and exact producer/input bindings when configured on the RunStore.

A grant cannot be reused for changed inputs, a different capability contract,
deadline, trace, request ID, node instance, attempt or lease epoch. An identical
read-only request can be invoked repeatedly until expiry: there is no durable
deduplication ledger in this worker. Do not interpret an accepted result as
exactly-once execution. Rejected/expired results are never converted to success.

## Invocation through a workflow node

The [inspection workflow](../examples/worker/inspect-definition.json) uses exactly
the same capability input/output contract and exact version as standalone calls.
Its decision node explicitly checks the inspection's `valid` output. In a new
temporary directory, the equivalent node request can be prepared with:

```sh
target/debug/workflow worker prepare-node examples/worker/inspect-definition.json inspect examples/worker/node-job.json
```

Pass its output through `grant`, `dispatch` and `check-result` as above. The example
attempt fields are demonstration correlation metadata; they do not identify a
created run or acquired lease. In a runtime integration, the host supplies actual
persisted run/instance/attempt identities and lease epoch. The request preparer
checks the complete definition, exact capability reference, identical node and
capability contracts, actual values and preconditions. It locks the workflow-v1
definition digest into the request. Model-policy task nodes are rejected here,
because invoking the raw capability would bypass the requested model policy.

The host is responsible for resolving data bindings from actual workflow inputs
and committed predecessor outputs before passing node values. These commands do
not traverse the graph, evaluate the following decision, establish data provenance
or advance the example's terminal state. The false-precondition error likewise
does not decide how the runtime will record a skipped node.

## Descriptor and wire contracts

`capability list` advertises `protocol_versions: [1]` for direct built-ins.
`model check-policy` advertises protocol 2 for model-policy execution. Requests,
grants and results must agree on the version; protocol 2 requires a policy binding
and execution record, while direct calls omit those fields. Unknown fields and
versions are rejected. See [model execution](model-execution.md). The exported
worker request/result schema files retain their historical filenames and describe
both variants; version/field dependencies are also checked at runtime. Descriptor
schema version is separate and currently 1. The same direct ID/version or the same
ID/version/policy digest cannot be registered twice in a worker. Distinct policies
may share one exact task contract. Contract changes must
publish a new capability version; a mismatched digest is refused even if its
ID/version spelling matches.

Descriptors include input/output IR contracts, timeout, named error classes,
effect declarations, usage semantics and an optional exact Skill reference.
Skill references are usage metadata, not loaded code or a security policy. The
descriptor digest covers every field. Write descriptors state idempotency key
scope/retention, query and compensation capabilities. This read-only dispatcher
and result acceptor reject writes; the separate [durable effect executor](remote-effects.md)
authorizes and records them through its intent/observation protocol.

Every work request includes protocol version, request/trace IDs, input values and
digest, capability ID/version/contract digest, issue time and deadline. Workflow
scope additionally carries definition digest, run ID, node ID, node instance,
attempt and positive lease epoch. Results carry protocol version, request digest,
completion timestamp, typed outputs or a declared error code/class, and artifact
ID/digest references. Evidence references are syntax checked, not fetched or
attested by this worker; they cannot be treated as verified artifacts solely from
this result. With the artifact contract, this digest identifies the manifest and
its bound provenance/content, while the payload digest lives inside that manifest.

Use `workflow schema capability`, `schema request`, `schema grant` and `schema
result` for generated JSON Schemas. Runtime checks additionally enforce identity,
digest, time, shape and contract semantics. Unknown/duplicate JSON keys are
rejected at every depth, including input objects. Trailing documents, messages
over 2 MiB and excessive JSON depth are rejected. Descriptor size is at most
128 KiB; contracts have at most 128 top-level fields, 4096 type members and depth
16. Built-in workflow documents retain the compiler's 1 MiB limit.

Protocol digests are SHA-256 over compact UTF-8 JSON with sorted object keys as
encoded by the pinned `serde_json` implementation; array order and numeric
representation are preserved. This is not RFC 8785. Definition digests continue
to use the existing workflow-v1 canonicalization, which also sorts nodes.
Use the Rust codec for interoperability and compare reference fixtures before
introducing a different language implementation.

## Time and failure boundaries

The worker rejects future-issued and expired requests, an insufficient grant
lifetime, changed descriptor/input digests, missing capabilities and bad actual
inputs before invoking an adapter. It rechecks the result after execution.
Effective execution deadline is the earlier of the request deadline and the
adapter timeout measured from dispatch. Wall-clock rollback and results arriving
after the deadline are errors; elapsed monotonic time additionally bounds result
acceptance if the wall clock changes during a call.

In-process Rust calls use cooperative deadlines: an adapter receives its effective
deadline, but an arbitrary blocked Rust function cannot be safely killed. A
late result is rejected after it returns. Hard preemption needs a process/remote
adapter. Rust panics under unwind are caught; process aborts and external worker
crashes require host recovery. A read-only declaration is a trusted adapter
contract, not an OS sandbox against malicious adapter code.

Failures must use a descriptor's declared code and matching class. Transient does
not authorize automatic retry. Missing outputs, wrong types, undeclared outputs,
invalid evidence, forged request identity and attempted transition fields fail
result validation. Durable leases and artifact verification are implemented by
the separate host adapters linked above. See [R02 acceptance](model-boundaries-acceptance.md)
for model adapters, provider binding and the shared local/remote transport contract.

<!-- book-navigation -->

[Contents](README.md) · [中文](zh/worker-protocol.md) · [Previous: Author and publish](definition-registry.md) · [Next: Control flow and replay](kernel-semantics.md)

<!-- /book-navigation -->
