---
name: workflow-gate
description: Evaluate and revalidate workflow-cli evidence against an exact policy, action and target using settled execution history and typed artifacts. Use for R03 evidence checks; this read-only surface does not authorize effects or complete a run.
metadata:
  version: "1.0.0"
---

# Workflow gate

Read `workflow help`, `docs/evidence-gates.md` and `workflow schema gate-request`.
Use the task's host-controlled policy, run database and artifact store. Do not
create a replacement store, remove requirements or change the accepted Boolean
field to manufacture a PASS. Policy changes require the authority established
for the task; reading evidence itself does not require a new approval.

Obtain the current target from the authorized host/workspace observation. Bind
the exact proposed action, run digest, repository/revision, input digest and
artifact links. Each checker currently uses the same input digest. Report direct
inputs must equal the target artifact set. Do not copy an old target merely
because it had passing evidence. Local source metadata alone does not establish
that a worker inspected the current clean workspace.

Select reports actually included in settled worker results. A published report,
copied producer metadata or an LLM's success assertion cannot replace an accepted
execution record. Use `run --artifacts <store> execution-history` and follow its
pagination when investigating provenance. Retain actual reports and failure
observations; do not invent or edit worker outputs or artifact metadata.

Run `gate evaluate <run-db> <artifact-store> <request.json>`. Inputs are raw JSON.
Save the full request and the response's `result` as the decision. Exit 0 means
PASS; FAIL and UNKNOWN both exit 1. `ok: true` alone is insufficient. Read the
per-check reason and expiry. An operational error produces no decision.

For reuse, freshly observe the target and use `gate revalidate <run-db>
<artifact-store> <request.json> <decision.json>`. Changes to any request binding
invalidate the old PASS. Missing, late, stale or mismatched evidence requires
investigation or new actual work within the task's authority. New evidence must
be evaluated under the required policy. Do not repeatedly retry a confirmed
failure as if it were an unavailable report.

Report verdict, policy/target digests, evidence references, expiry and unresolved
requirements. A PASS is a read-only observation. This command does not advance the
run, grant execution permission, implement repair loops, or produce human approval.
Do not describe a workflow as gated merely because this checker was run. Enforcing
node/terminal gates and atomically consuming a decision are later integrations.
