# Explicit ordered compensation

Compensation is a declared write task with the same durable intent, provider query,
retry budget and lease fencing as a primary effect. The workflow selects the
business branch; the runtime validates that the original effect may be reversed
and that dependent effects have been settled first. Cancellation does not select
this branch or undo a provider resource.

## Definition and admission

A write descriptor can declare `compensation: {id, version}`. The compensating
capability must be a write capability in the frozen bundle. `irreversible: true`
forbids a compensation declaration. The default `false` preserves existing
contracts; it does not promise reversibility without a compensator declaration.

Each effect binding may include:

- `depends_on`: up to 128 unique managed node IDs in the same workflow frame.
- `compensates`: the managed original task whose declared compensator this task
  invokes. Each original has at most one compensator task per workflow definition.

Compilation rejects missing, self, later or bypassable prerequisites, mismatched
capability versions, compensation of compensation, irreversible originals and
contradictory reverse ordering. A declared prerequisite must precede every path
that reaches the dependent node. Subworkflow/loop frames retain separate concrete
instances; cross-frame compensation dependencies are not supported.

At first admission the store freezes prerequisite operation keys and the original
**actual Applied receipt** into the intent. The compensation key hashes the
original operation key, the compensation slot and the exact compensator version.
It remains stable across retries and ownership changes. Inputs, original resource,
provider receipt, target and policy remain bound by the intent digest.

The ledger checks the business dependency graph on every admitted write and on
replay. A prerequisite must be Applied and not under or after compensation. Before
undoing an original, every transitive primary dependent must either be confirmed
not applied (Failed/Cancelled) or have its own completed compensation. In-flight,
unknown and manually unresolved effects cannot be assumed absent. Starting
compensation also prevents later primary writes from depending on that original.

An Applied compensation atomically marks the original's `compensated_by`, commits
the task result and releases successors. Recovery never calls a provider. A
missing local receipt after provider commit uses the ordinary query-first
reconciliation path. A completed compensator is not executed again.

## Failure and manual takeover

Transient known-no-effect compensation failures retry only within the original
idempotency, call-count and time bounds. A permanent/permission/business rejection,
or exhaustion after all writes were confirmed not applied, produces
`needs_attention`; the kernel keeps the task Reconciling. Unknown earlier writes
remain unknown even if a later call was rejected. No budget resets on restart.

Inspect `run effects` and the execution history. `drive-effects` returns
`effect_uncertain` for both unknown results and known failed compensation; the
ledger distinguishes them. A trusted local operator may perform authorized
cleanup and submit its actual receipt with `effect-resolve`, retaining a stable
resolution ID, actor annotation, reason and evidence. This does not add an
automatic call allowance. Exact duplicate resolutions are idempotent; changed
content conflicts. Actor annotations are not authenticated identities.

`confirmed_not_applied` cancels the compensation task without marking the original
compensated. It is an audited acknowledgement that cleanup did not happen, not a
successful rollback. Inspect original receipts and `compensated_by`; a cancelled
run alone does not prove that all external resources were removed.

## Executable example and evidence

`examples/runs/effect-compensation.json` declares environment creation, deployment,
and a wait bound to both resulting resource IDs. Approval retains resources;
rejection or timeout selects deployment deletion followed by environment deletion.
The wait in the fixture is 100 logical milliseconds. Give operational runs their
intended ID, start time and reviewed timeout. Fixture callback sources explicitly
identify simulated decisions; they are not actual human approval.

The SQLite adapter tests execute resource mutations in a separate provider database
with a foreign key that forbids deleting an environment before its deployment.
They cover transient retry, reopening between compensators, permanent permission
failure and actual manual cleanup, approve/reject/timeout/cancel branches, and
killing a worker after the second provider deletion commits but before its local
receipt. Query recovery completes that undo, with exactly one automatic write for
each compensator and no repetition of the earlier deletion. Pure ledger tests
reject wrong order, original receipt substitution, unresolved dependents, late
new dependents and irreversible reversal without changing ledger state.

Storage schema 9 introduced these semantics; schema 10 added protections for
[backup recovery](backup-recovery.md). Explicit v8→v9 migration is verified
using the actual previous binary: pending paused Inbox entries, lease/attempt
history and a prepared effect intent remain byte-equivalent at the CLI boundary;
v8 refuses the upgraded database. The migration fixture does not dispatch that
prepared intent. Migration is not backup/restore.

This chapter describes explicit branch compensation with local trusted host
admission and bounded sandbox evidence. [Remote effect acceptance](remote-effects.md)
adds authenticated shared dispatch and the complete R05 matrix; [approval acceptance](approval-acceptance.md)
covers human authority. Arbitrary task-failure rollback and production provider
guarantees remain outside these fixture claims.

<!-- book-navigation -->

[Contents](README.md) · [中文](zh/ordered-compensation.md) · [Previous: Durable external effects](durable-effects.md) · [Next: Local backup and recovery](backup-recovery.md)

<!-- /book-navigation -->
