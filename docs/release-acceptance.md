# Current evidence and protected delivery (R03)

Node completion, run completion and external release are separate decisions.
`postconditions` enforce the first two boundaries. An effect binding with
`release` protects the third. Legacy write bindings remain usable for general
effects; they do not claim quality-gated delivery. Use the protected contract for
publication, deployment or any action that requires current quality evidence.

Required checkers must be direct read-only tasks without a model policy. Bundle
compilation rejects model-driven or write-capability checkers, even when their
declared contracts expose a correctly typed Boolean result. Model-authored
structured output cannot certify its own quality; an independent registered
checker must execute and produce the evidence.

## Frozen release contract

`examples/gates/protected-release.json` is a complete checked example. The write
task has a required `delivery` input containing:

```json
{
  "source_revision": {"repository": "repository-id", "revision": "full-git-object-id"},
  "input_digest": "sha256:checked-task-input-digest",
  "artifacts": [{"artifact_id": "exact-artifact-id", "digest": "sha256:artifact-digest"}]
}
```

`EffectBinding.release` names 1–16 preceding gate nodes, this input's field,
0–16 ordinary approval requirements, and the target comparison contract. Every
gate must precede every control-flow path to the write. Its `action` must equal
the actual write capability ID/version. All gate subjects must equal the write
input: repository/revision, checked input digest and ordered artifact links.
Canonical approval subjects sort artifact links; execution subjects retain their
declared order and refuse a changed request.

The authority freezes the actual gate contexts and applied approvals from that
frame's verified history. Before **each write**, including a retry, it reads the
settled checker results and re-verifies artifact bytes, producer identities,
capability contract/tool versions and current evidence age. A stored earlier
PASS alone cannot authorize the call. The resulting `ReleaseAuthorization`
contains the new evaluations and the earliest evidence/approval expiry. The
effect attempt's deadline is clipped to that expiry and rechecked before commit.

Local runtime dispatch rechecks the committed call before invoking its adapter.
Authenticated shared delivery does the same inside the scoped PostgreSQL
transaction before its single-use assignment becomes delivered. Expired leases,
changed epochs, pause, cancellation, recovery barriers and expired evidence
cannot admit a write. Recovery recomputes each authorization from its original
execution prefix and recorded time, rather than trusting caller-computed hashes.
Queries carry no release authorization: they remain available to reconcile an
older write after quality evidence or an approval expires.

A run that started with, or migrated into, a protected release contract or a
quality-gated human approval flow cannot subsequently migrate its definition.
A changed graph or weaker policy requires a
new run with a distinct run digest and new evidence/approvals. This deliberately
conservative restriction preserves the original approved delivery. Legacy R11
migrations retain their reviewed restart behavior.

## Independently authorized exceptions

A postcondition may name `exception: {node_id, subject_field}`. The named prior
human wait must declare a digest subject and a versioned exception policy with
its own allowed responders and codes. R06 ingress authenticates the shared
Approver identity; the reducer independently checks its frozen responder list,
exception policy/code, exact run/instance/input correlation, reason and expiry.
Local ingress remains a trusted host operation. Model text and actor strings do
not authenticate a shared responder.

`workflow gate review-digest <gate-request.json>` computes the reviewed scope from
the complete quality policy, action, repository/revision, checked input digest
and artifacts. Run ID/digest are deliberately excluded to avoid self-referential
start inputs; the authenticated wait correlation separately binds the actual run
and instance. Different loop frames, inputs, policies and actions need their own
valid approvals. Ordinary approvals cannot act as exceptions, and an exception
cannot satisfy a required ordinary approval.

An admitted exception leaves the checker verdict **FAIL or UNKNOWN** intact.
`gate_exception` records the actor, reason, policy/code, review digest, message,
correlation and expiry; the transition reason is `postcondition_exception`.
Both node and terminal gates require their own correctly scoped admission.
The effect authorization retains the same distinction. Expired or mismatched
exceptions fail closed. A repair loop still uses its original iteration count,
overall deadline and total run budgets; an exception never resets those budgets.

## Actual target and local workspace

The trusted gateway owns provider-specific target observation and application:

* `atomic_compare`: compare the complete subject and apply the operation under
  the provider's atomic boundary. A stale candidate must be rejected without a
  write. This is an adapter contract, not a distributed transaction invented by
  workflow-cli.
* `observe_then_reconcile`: explicitly record a bounded, nonempty
  `remaining_race` explanation. The capability must declare a provider-backed
  query. Observation and application are separate; timeouts or uncertain writes
  use the existing effect reconciliation ledger and original operation key.

Successful receipts must attest the **observed** subject, comparison mode,
authorization digest and observation time within the admitted write window.
The ledger matches the receipt to a retained write authorization, including when
a later query retrieves it. Wrong-target or malformed replies establish no
success; the HTTP adapter returns UNKNOWN. An administrator can retain an actual
provider receipt during explicit backup recovery, with that separate recovery
audit, but cannot rewrite a quality verdict into PASS.

An HTTP host binding may additionally specify
`workspace: {repository, path}` with an absolute local Git checkout path. For
writes it requires `observe_then_reconcile` and verifies full HEAD plus actual
file bytes/modes against the checked commit immediately before HTTP dispatch.
It checks untracked and ignored files, rejects symlinks/unsupported entries and
uses the existing bounded object/tree readers. Git stat caches,
`assume-unchanged` and `skip-worktree` do not hide edits from this byte scan.
Only the root `.git` administrative entry is excluded. Host clocks/deadlines are
sampled again after this observation. Query calls do not require a clean tree.

The checkout is not locked across observation and the external request. Hosts
must use immutable publication inputs or an atomic provider comparison where
available, and preserve the declared remaining race otherwise. A malicious
privileged host/gateway can lie about its observation; these are authenticated
adapter boundaries, not hardware attestations or an OS sandbox.

## Final acceptance manifest

```sh
workflow run --artifacts <store> acceptance <db> <run-id>
workflow remote acceptance <client.json> <run-id> <new-private-output>
```

The shared protocol also supports `acceptance {run_id}`. Both backends use the
same reducer and consistent verified snapshot. The report includes frozen
requirements, source-bound artifact manifests, every gate decision (including
failed repair rounds), applied inbox approvals, external effect intents/calls/
receipts, run/bundle/revision identity, snapshot and execution-history digests,
and a content digest. Artifact payloads are re-verified before export; missing or
corrupt committed bytes abort the report instead of emitting partial acceptance.
Reports above 8 MiB are refused; the local JSON stdout envelope additionally
obeys the CLI's 2 MiB message limit. Shared reads use the existing scoped read roles;
the CLI writes an exclusive 0600 file and never overwrites an existing archive.

`accepted` requires a successful run and an admitted root terminal postcondition.
`accepted_with_exceptions` additionally identifies historical exception use.
Ungated or unfinished runs report `incomplete`; they do not fabricate acceptance.
This is a historical delivery record, **not a new permission to publish later**.
Each subsequent external write still needs a fresh authorization. The digest
detects accidental modification; it is not a signature against a privileged
database administrator or an untrusted report author.

## Reproducible acceptance

```sh
cargo test -p workflow-kernel postconditions --locked
cargo test -p workflow-runstore-sqlite gates --locked
cargo build -p workflow-cli --locked
python3 examples/gates/release-acceptance.py target/debug/workflow
WORKFLOW_TEST_POSTGRES='...' python3 examples/gates/release-acceptance.py target/debug/workflow --https
```

The CLI matrix uses a disposable Git repository, the actual definition validator,
a gateway with durably stored receipts and, for shared mode, real TLS,
PostgreSQL, authenticated worker grants and scoped artifact upload. It publishes
no real release. Both executions are mandatory in CI.

| R03 acceptance | Evidence |
| --- | --- |
| Old commit PASS cannot release a new target; changed bytes fail integrity | `old-target`, `provider-change`, `dirty-workspace`; SQLite payload-corruption test and existing shared artifact corruption contracts |
| Model text, empty/missing or forged PASS cannot bypass checks | `missing`, `fake-report`; model/write checker compile-rejection regressions; evidence-checker missing Boolean/type/provenance tests; R14 model malicious-response admission |
| Confirmed failure uses bounded repair and total budget | Kernel/SQLite three-round repair, total-deadline and UNKNOWN idle/retry tests |
| Parallel, changed and delayed observations remain version bound | Existing instance/input/concurrent-claim/late-settlement tests; release expiry/pause/lease/query tests |
| Final requirements/artifacts/checks/approvals manifest | `pass`, `approved`, `workspace-pass`; digest tampering and replay equality tests |
| Local/shared parity with traceable human exception | Identical CLI matrix; `exception` preserves FAIL and the authenticated incident responder in both gates and the manifest |

The deterministic target is zero unauthorized writes in this matrix. These
checks do not measure production business benefit or guarantee a third-party
gateway implements its advertised atomicity; validate that adapter separately.

## Compatibility

Storage tables remain schema 11; artifact catalogs remain schema 1. Optional
release/exception/workspace fields are omitted from legacy documents so their
canonical hashes remain stable. Generated v1 schema families include the new
typed fields. Old binaries with closed schemas reject new protected contracts;
they must not be used to execute those runs. R11's explicit backup/upgrade and
retained-compatible-binary recovery procedure continues to apply.
