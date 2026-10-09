# Workflow gate

Read `workflow help`, [manual](manuals/en/03-execution-and-evidence/07-evidence-gates.md) and `workflow schema gate-request`.
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
node/terminal gates requires the runtime integration below; external action
consumption remains a separate integration.

## Mandatory runtime gates

Read [manual](manuals/en/03-execution-and-evidence/08-runtime-postconditions.md) and `schema kernel-bundle`. Use the authorized
frozen `postconditions` when starting a run; bind required checker instances through
the workflow, exact policy/action and target bindings. Never weaken or omit gates
from an existing approved task. `assets/examples/gates/drive-guarded.py` demonstrates real
execution and separate task/terminal PASS transitions.

Use the fenced `run acquire/claim/finish` flow for actual worker observations and
retained reports, then `run --artifacts <store> drive` to check pending gates. The
built-in worker does not publish reports automatically. UNKNOWN records a decision
and stops; inspect its cause before requesting `run retry-gate` with a stable event
ID and current revision. A retry retains the target and does not execute new work.
FAIL follows the declared bounded repair path. Do not fabricate a gate event,
manually acknowledge its intent, edit settled evidence or reuse another loop
instance. Report the committed run status and both node and terminal decisions.
