# Durable approval and event acceptance (R06)

R06 uses the same deterministic wait and Inbox reducer in local SQLite and the
authenticated PostgreSQL authority. The committed bundle contains the waiting
type, subscription event, required inputs, responders, policy version, validity
limit and accepted/rejected/timed-out routes. Waiting creates no model invocation
or worker assignment. A host may exit and recover the same instance later.

## Policy and identity

`examples/approval/protected-start.json` runs the existing offline compiler
workflow with a human review of the supplied `review_digest`. Applications must
compute that digest from the actual content being reviewed; the example supplies
a deterministic fixture digest. `wait_policies` binds a pinned policy to a
workflow version and node. A human policy requires at least one exact required
input declared as `digest` or `artifact`. Artifact inputs are single
`{artifact_id, digest}` links and must belong to this run. A digest subject binds
content identity without claiming that this service stores those bytes.

The policy allows named authenticated actors, limits response validity from
durable receipt, and optionally defines a separately versioned exception policy.
Its responder list and allowed exception codes are explicit. Exception responses
retain their code, policy, actor and reason; they still require the current
subject, exact correlation and an unexpired deadline. Policy content cannot be
changed under an existing identity/version in either store.

Local `workflow run receive runs.db response.json` is a trusted administrative
interface. Protect the database and do not feed model text into that interface.
The remote boundary requires TLS and opaque scoped credentials. `approve` accepts
an `approver`; `signal` accepts a `signal_source`. Both replace payload `source`
with the authenticated actor and require a matching bound wait type. The kernel
then enforces the frozen actor/subject/expiry rules. An unauthorized responder
gets a retained rejection; a wrong endpoint role is denied and audited without
changing the run. Raw `Signal` events cannot settle a protected wait.

Existing access schema 1 deployments explicitly run:

```sh
workflow service migrate-access server-binding.json
```

Migration is transactional and repeatable. New installations create access
schema 2; activating cluster scheduling upgrades it to schema 3. Old bundles without policies remain readable and locally operable by
the trusted host, but authenticated responses require a protected bundle.
Published versions and in-flight run definitions are never rewritten.

## Local and remote operation

```sh
cargo build -p workflow-cli --locked
python3 examples/approval/local-demo.py target/debug/workflow
```

The executable example stamps current start times, drives the workflow, checks
that a fresh session executes no work while waiting, submits all three decisions
and verifies duplicate receipts and full replay. For your own run, use
`workflow schema run-signal` to construct `response.json`. Copy the run
digest and target/correlation from the actual query. Supply a unique provider
message ID, `approve`, `reject` or `request_changes`, a nonempty reason, outputs
matching the wait contract, and absolute expiry. Retrying an identical response
returns the current receipt with `duplicate: true`; changed content under its ID
is a conflict. An applied receipt, not exit status alone, proves consumption.
The local source must name an allowed host-attested actor such as `reviewer`.

Remote clients use the same submission inside the operations protocol:

```json
{
  "protocol_version": 1,
  "request_id": "review-delivery-1",
  "operation": {
    "type": "approve",
    "request": {
      "schema_version": 1,
      "run_id": "approval-demo",
      "run_digest": "COPY_FROM_RUN",
      "message": "REPLACE_WITH_RUN_SIGNAL_MESSAGE"
    }
  }
}
```

The placeholders describe the envelope; generate a valid message from the schema
before `workflow remote call client-binding.json request.json`. CI/webhook
gateways use `type: signal` with a `signal_source` credential and an
`external_event` wait policy. A webhook gateway authenticates upstream signatures
and keeps a durable retry queue. During service downtime, messages remain at the
gateway or are recovered by provider queries; a stopped process cannot receive
callbacks. Once the service acknowledges a receipt, the shared Inbox owns it.

Reject/request-changes take the definition's `rejected` edge; their distinct
decisions remain audited. Define a revision task followed by a new wait, or a
bounded subworkflow loop, for rework. New input digests and instance identities
invalidate old decisions. `timed_out` can lead to escalation, another wait or a
terminal outcome. Pause/resume preserves the original deadline. Cancellation
and timer/response transactions serialize; timeout is reduced before a response
at the same observed time. Receipt admission rechecks expiry at commit.

## Acceptance matrix

| R06 criterion | Executable evidence |
| --- | --- |
| Same wait after session and service restart | SQLite `paused_inbox_persists_across_sessions_and_cancelled_or_expired_receipts_never_advance`; real HTTPS `tls_wait_survives_service_restart_and_resumes_only_from_the_authorized_current_response` kills the server, creates fresh sessions, compares instance/deadline/subjects and resumes both human and CI waits |
| Early, duplicate, reordered and late events | Kernel Inbox suite plus PostgreSQL `early_events_survive_restart_and_lost_acknowledgements_in_receive_order`; first receipt wins even when message IDs sort differently; exact retries keep the original receipt |
| Unauthorized actor, wrong subject or expired approval cannot release work | Kernel `protected_wait_rejects_raw_signals_and_wrong_actor_subject_validity_or_exception`; PostgreSQL actor/channel/expiry test; SQLite artifact corruption rollback and recovery test; HTTPS wrong-role tests |
| Reject/change request takes rework; old decision cannot be reused | Kernel `rejection_and_request_changes_rework_to_a_new_subject_and_reject_old_approval` tests both decisions, a revision task and a new wait, then rejects old instance and old input digest |
| Timeout, cancellation and response admit one legal transition | PostgreSQL `timeout_cancel_and_response_serialize_to_one_transition_after_scheduler_restart`; SQLite independent-process cancellation/duplicate races, killed writers and commit-expiry rollback tests |
| No model cost or persistent worker slot while waiting; complete audit | HTTPS waiting test observes no pending worker assignments; PostgreSQL early-event test settles/releases assignments; existing runtime wait/lease tests; Inbox preserves response/actor/reason/decision and access audit preserves endpoint/credential/outcome |

Run the local and shared suites with:

```sh
cargo test -p workflow-kernel -p workflow-runstore-sqlite --locked
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo test -p workflow-service --locked -- --ignored --nocapture --test-threads=1
cargo test --workspace --locked
bazel test //...
```

The ignored suites require `WORKFLOW_TEST_POSTGRES` pointing to a disposable
database; the required PostgreSQL CI job supplies it. Each competing transaction
uses an independent connection; HTTPS restart tests use actual OS processes.
These are correctness fixtures, not production throughput or human-turnaround
benchmarks. No provider billing reduction is inferred from fixture timing.

<!-- book-navigation -->

[Contents](README.md) · [中文](zh/approval-acceptance.md) · [Previous: Model boundary acceptance](model-boundaries-acceptance.md) · [Next: Artifact and workspace acceptance](artifact-acceptance.md)

<!-- /book-navigation -->
