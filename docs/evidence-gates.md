# Evidence checks and target binding

`workflow-gates` is a portable checker with an injected `EvidenceSource`. It has
no SQLite, filesystem, worker SDK or kernel dependency. The local CLI implements
the port using recovered RunStore execution records and verified artifacts.
It returns **PASS**, **FAIL** or **UNKNOWN** for an exact proposed action and target.
The standalone CLI produces read-only observations.
[Mandatory runtime postconditions](runtime-postconditions.md) consume the checker
inside fenced run transitions; external effect authorization remains open.

## Evaluate actual work

```sh
cargo build --locked --bin workflow
python3 examples/gates/check-validation.py "$PWD/target/debug/workflow" /tmp/new-gate-example
```

Use a new output directory. The example executes the actual validation capability,
publishes its typed report, settles the result under a durable lease, then checks
it. The inspected definition is read from a pinned Git commit. The example retains
the request, policy, target, artifact reference and decisions. Changing the target
revision returns UNKNOWN; reusing the prior PASS for that new target is rejected.
There is no publication or other external effect in the example.

```sh
workflow schema gate-request
workflow schema gate-decision
workflow gate evaluate <run-db> <artifact-store> <request.json>
workflow gate revalidate <run-db> <artifact-store> <request.json> <decision.json>
```

Requests and prior decisions are raw JSON objects, not CLI envelopes. Save the
`result` field of an evaluation as `decision.json`. Only a PASS exits **0**.
FAIL, UNKNOWN and invalid/unavailable evidence exit **1**; usage/input I/O errors
exit **2**. `ok: true` means evaluation completed, including FAIL and UNKNOWN;
inspect `result.verdict`. Opening an unavailable/corrupt run or store produces
`ok: false` with an error and no decision. These commands never create a store,
change a run revision, dispatch work or issue an execution grant.

## Policy and target

A policy has an exact identity and 1–64 named requirements. Each requirement pins:

- the workflow node ID and capability ID/version;
- the complete capability contract digest;
- the complete expected artifact type, including a closed JSON object containing
  the required Boolean field;
- the Boolean field to inspect in the **accepted worker output**;
- a maximum age from 1 millisecond to 30 days.

The target binds a run ID and immutable run digest, exact proposed action version,
repository/full Git revision, common checked input digest and up to 128 exact
artifact manifest links. This first contract requires each check to have the same
input digest. Heterogeneous per-check input projections are not implemented.
Every report must list exactly the target artifact set as its direct inputs.
Empty artifact sets support checks on inline inputs, such as a workflow definition.

Supply an explicit evidence link for each requirement. Omitted evidence is UNKNOWN;
duplicate or unknown requirement IDs are rejected. Missing requirements cannot be
made optional by leaving them out of the evidence list. Removing a policy
requirement changes the request/policy digest and invalidates a previous decision.
Policy authority belongs to the host: this CLI does not decide who may create a
new policy or permit an agent to adopt a weaker one for an existing approved run.

## Verification order and meaning

The checker verifies report manifest identity, bytes, type and ancestors through
the reader. The report's source revision, producing run, input digest and direct
artifact dependencies must equal the supplied target. Each target artifact must
also verify and belong to that run.

A report alone is insufficient. The trusted execution adapter must find the
**settled** attempt with the exact producer, request and input identities. Its run
digest, node, capability and contract must match; its accepted worker result must
explicitly include the exact report link. Local reads recover the execution
journal, validate requests/grants/results, epoch fences and the committed kernel
event/receipt. Publication without settlement and copied producer metadata on a
new, unreferenced report cannot establish execution.

The verdict comes from the accepted worker output. A report saying `valid: true`
while the actual validation output says `valid: false` is FAIL. The checker does
not interpret natural-language summaries, arbitrary payload assertions or an
LLM's claimed verdict. The typed report is a retained evidence attachment, not the
source of the Boolean verdict. A successful capability invocation with no required
Boolean output is UNKNOWN. An accepted permanent capability failure is FAIL;
invalid input, cancellation, transient failure and uncertain effects are UNKNOWN
because they do not establish a completed quality check.

Completion must be positive, no later than settlement, and settlement no later
than the supplied host evaluation time. Expiry is exclusive:
`now >= completed_at + max_age_ms` is UNKNOWN. Future, overflowing and expired
observations cannot pass. Any verified failure yields FAIL; otherwise any UNKNOWN
yields UNKNOWN; only every required check and target artifact passing yields PASS.

Each decision records request/policy/target digests, evaluation time, earliest
expiry, per-requirement reason, evidence link, verified producer and completion
time, plus per-artifact integrity findings. Preserve the full request with the
decision for review; digests do not substitute for the policy/target documents.
No approval is implied or included. A complete final acceptance manifest with
human approvals is still pending R03/R06 integration.

## Revalidation and authority boundaries

`revalidate` requires the identical request, recomputes the previous PASS from
verified evidence (rejecting fabricated fields), then evaluates at current host
time. A changed action, revision, input, artifact, policy or evidence binding
requires a new evaluation. An expired observation cannot pass again. This is
recomputation, not a cryptographic signature or evidence that the old CLI invocation
occurred. Local SQLite/filesystem ownership and the host ingress remain trusted.

The local source checks run revision before and after its bounded paginated read;
a concurrent business transition rejects the read for retry. Accepted records are
immutable. At most 10,000 execution records are read. The pure checker also rejects
duplicate JSON keys, unknown fields, oversized documents and deep type contracts.

The host must independently observe the current workspace/target and protect the
policy. A manifest's repository/revision remains a host claim: neither metadata
nor a digest proves that a worker actually inspected a clean checkout. The CLI
cannot authenticate a remote worker, turn model-authored JSON into trusted host
facts, or prevent a caller from supplying an old target as if it were current.
Future remote adapters must enforce these obligations before implementing the
same `EvidenceSource` port.

A PASS has no authority to execute an external action. To consume a decision, the
future runtime/effect adapter must re-observe the current target, check expiry,
revalidate, and atomically compare the exact target when applying the action. If
an external service cannot do this, a remaining race and a reconciliation path
must be represented explicitly. Evidence may become unavailable or a mutable
external target may change after a read. This increment does not close that race.

## Acceptance evidence and remaining R03 work

Tests exercise actual compiler output and ledger settlement, forged report
assertions, missing/uncommitted/substituted references, corrupt payloads, exact
revision/input/tool/type/provenance checks, strict Boolean results, stale/future
reports, changed actions/policies, decision tampering and schema parity. The
in-memory port and local adapter use the same evaluator; no cloud transport or
cloud identity acceptance is claimed.

Mandatory task/terminal gates and declared bounded repair now use this checker
through the [runtime integration](runtime-postconditions.md). R03 remains open for
action authorization/consumption, current-workspace verification, independent
approval/exception authority, and the final acceptance manifest. R06 adds authenticated human responses and durable event waits.
