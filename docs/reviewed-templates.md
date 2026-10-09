# Reviewed SDLC templates (R13)

Templates are optional, versioned business definitions. Select one only when its
declared prerequisites and exclusions match the task. The normal definition and
run APIs remain available for autonomous exploration. A template proposes a
frozen bundle; an instance supplies business parameters and a separate inventory
of host capabilities, effect targets, model policies and permitted approvers.

## Portable business definitions

| Template | Preparation | Successful delivery | Failure handling |
| --- | --- | --- | --- |
| `template.defect@1.0.0` | Diagnose pinned source | Verified, approved PR **awaiting merge** | One repair with previous candidate and actual test diagnostics; repeated failure terminates |
| `template.feature@1.0.0` | Design from requirement | Reviewed design, implementation and test artifacts | Same bounded repair; no external publication |
| `template.release@1.0.0` | Prepare release candidate | Independent preflight, approval, provider publication and deployment smoke test | Failed smoke test compensates the exact original deployment receipt |

All three reuse `sdlc.implementation-attempt@1.0.0`. Its stable contract accepts
request, source repository/revision, context artifact and feedback; it returns
the candidate artifact, observed test result and diagnostics. A failed first test
passes those real outputs into a second invocation. This is an explicit two-attempt
graph, since repeating a loop with the same initial inputs would lose the feedback.
Independent final checkers are direct read-only tasks without a model policy.
Every successful root terminal has a frozen postcondition. Typed report evidence
must belong to the actual checker attempt, current inputs and candidate artifact.

Each branch reviews the digest of its current candidate and quality policy. A
rejected approval cancels the run. A missing response fails after five minutes.
These examples intentionally implement a timeboxed collaborative review; longer
business review windows need a new definition, fresh reports and owner review.
Delivery rechecks quality, approval and target at write time. The provider must
atomically compare the source and candidate or use a separately reviewed policy
that declares its remaining race. Compensation uses the prior immutable receipt.
Ambiguous writes retain the R05 query/recovery behavior; a report is never itself
authorization to retry a write.

`examples/templates/*-local.json` and `*-shared.json` use the same business
definition and parameter contract. Bindings contain logical adapter names, exact
capability digests, permitted target identities and approvers. Provider URLs,
credentials, database bindings and organization-specific implementation stay in
the host. The examples' `sdlc.*` and `delivery.*` contracts require registered
project adapters. `acceptance.py` provides runnable fixture adapters that create
candidate workspaces, execute real Python tests and use a disposable PR/deployment
provider. They establish execution and recovery contracts, not LLM task quality
or support for a production Git/release provider. A model-driven adapter can use
the existing bounded model protocol; adding a frozen model policy is a reviewed
definition change, not an instance override.

## Preview, plan and diff

```sh
workflow template validate examples/templates/defect.json
workflow template plan examples/templates/defect.json examples/templates/defect-local.json
workflow template diff before.json after.json
workflow schema template
workflow schema template-instance
```

Validation checks metadata, business roles and deliverables, exact root parameter
contracts, compiled graph/child contracts, frozen waits, mandatory acceptance and
approved effects. Planning resolves defaults, permitted overrides and business
facts, rejects missing/wrong capability or model-policy digests, unapproved effect
targets and unavailable responders, then constructs a `StartRun` request without
calling any adapter or creating storage. Errors identify the parameter/binding
path. The plan lists dependencies, all possible branch/compensation actions,
write flags, target identities, gates, waits and per-node budget ceilings. It is
an inventory of possible actions, not a prediction that every branch will run.

`proposed_run_digest` uses the supplied preview clock. Shared start replaces that
clock with authoritative database time, so its actual digest may differ. A
declared environment does not grant credentials or guarantee worker availability;
the shared scheduler and worker grants independently enforce execution access.
Budget ceilings bound task timeouts, loop bounds, waits, model calls per task and
effect calls per attempt. They do not implement aggregate monetary accounting
or distributed run-wide budget reservations (R12).

Diffs include JSON-pointer paths and complete before/after values, changed
sections, mandatory-policy changes and whether a fresh review is required.
Changing a gate cannot be expressed through instance parameters. Any different
candidate needs its own owner review; reusing a published template, component or
policy identity with different content is rejected atomically.

## Owner review and publication

```sh
workflow template init templates.db examples/templates/owners.json
workflow template propose templates.db candidate.json author
workflow template candidate templates.db sha256:CANDIDATE
workflow template review templates.db sha256:CANDIDATE process-owner approve 'Reviewed exact reports'
workflow template publish templates.db sha256:CANDIDATE
workflow template get templates.db template.defect 1.0.0
workflow template instantiate templates.db template.defect 1.0.0 instance.json
```

Replace the digest placeholder with the actual proposal digest. An instance emits
publication provenance and `plan.request`; pass the latter to the normal `run start`
or shared `Start` API. Proposal records contain the template, proposer, change
reason, compatibility statement and ten regression references: success, rework,
rejection, timeout and terminal failure in both local and shared modes. Each
reference binds the template/bundle, run, acceptance report, artifact digests,
terminal result and attempted repair rounds. Missing, duplicate or inconsistent
coverage cannot be published. Digests ensure integrity; they do **not** prove a
test was executed. Owners must inspect the referenced actual reports and artifacts.
Unit tests label their synthetic references explicitly. The end-to-end matrix
creates actual reports and candidate files for review.

Local actor names are trusted-host attestations. Protect the catalog file like
other local run stores. `init` cannot replace an existing owner policy or open an
unrelated SQLite database. Candidate, review, publication and component bindings
are append-only, and concurrent review attempts commit one immutable winner.
Rejected candidates need a new proposal; an approval cannot overwrite rejection.

For authenticated operation, provision project owners through the trusted host:

```sh
workflow service configure-template-owners server.json owners-binding.json
workflow remote template-plan runner-client.json template.defect 1.0.0 instance.json
```

`owners-binding.json` contains `tenant`, `project`, `expected_revision` (null on
creation) and `policy` with the same shape as `owners.json`. Updates use revision
CAS and preserve owner-policy history. This administrative operation is absent
from the public RPC. The explicit catalog schema is separate from access/runtime
schemas; configuring owners installs it, and unknown future versions fail closed.

Shared `TemplatePropose` and `TemplatePublish` require `DefinitionMaintainer`.
The proposer comes from its scoped credential, irrespective of the supplied actor
string. `TemplateReview` requires both `Approver` and the configured business owner
role, and forbids the proposal author's actor. Review records use database time
and bind the exact candidate plus current owner-policy revision. Changing owners
invalidates unpublished old reviews; submit a fresh candidate for current review.
Published versions retain their historical approval. `TemplateCandidate` and
`TemplateGet` follow existing project read roles. All records are tenant/project
scoped and all mutations are audited. Publication atomically freezes component
bindings and makes the underlying bundle available to authorized runners.

Subflow upgrades use new pinned versions. Existing run images, old publications
and their retained evidence remain unchanged; they are not retroactively assigned
the latest template. The PostgreSQL contract test starts a real stored run,
publishes a new child/root version, then verifies its original snapshot and bundle.
R16 tools may submit candidates to this same path; they have no owner-review bypass.

## Reproducible acceptance

```sh
cargo test -p workflow-templates -p workflow-registry-sqlite --locked
cargo build -p workflow-cli --locked
# WORKFLOW_TEST_POSTGRES must name a disposable PostgreSQL database.
python3 examples/templates/acceptance.py target/debug/workflow --output template-acceptance
```

The full matrix runs **30 executions** (three templates × five scenarios × two
modes), using exactly the checked-in business bundles. Local and TLS/PG workers
read the pinned Git source, transfer typed artifacts, execute candidate tests,
propagate actual failed-test diagnostics, perform independent final checks,
submit bound approvals and verify provider receipts. Release failures execute
compensation and retain both receipts. All six timeout cases wait for their real
five-minute durable deadlines, with no modified fixture definitions or fabricated
server clock. The harness parks those waits while running other cases.

The output directory retains complete acceptance manifests, artifact references
and payloads, regression candidates, fixture-owner publication records and a
summary. These fixture reviews exercise authenticated owner controls; they do
not stand in for a production process owner's review. CI retains the same files
as `template-acceptance` artifacts. `--quick` and `--local-only` are development
checks, explicitly reported as incomplete matrices, and never publish candidates.

Cargo/Bazel tests cover deterministic plans, precise pre-execution errors, fixed
project parameters, diff details, gate-removal rejection, missing/false regression
coverage, stale candidate reviews, owner/scope enforcement, owner rotation,
concurrent immutable reviews, component version conflicts, retained old runs and
schema exports. The three PostgreSQL template tests run in the mandatory isolated
database CI job. `generate.py` deterministically reproduces the checked-in examples;
changes to its output still follow the same review and regression process.

This fixture measures executed cases, actual repair attempts and artifact/effect
counts. It does not establish organization-wide configuration time, reuse rate,
cross-project adaptation effort or productivity improvement; those comparisons
belong in the R15 acceptance baseline.

<!-- book-navigation -->

[Contents](README.md) · [中文](zh/reviewed-templates.md) · [Previous: Control flow and replay](kernel-semantics.md) · [Next: Durable state](run-store.md)

<!-- /book-navigation -->
