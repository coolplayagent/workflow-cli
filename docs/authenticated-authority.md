# Authenticated PostgreSQL application boundary

`workflow_runstore_postgres::access::AuthenticatedService` is an in-process
application interface for a trusted service host. Its database connection stays
on that host. Untrusted callers supply a bearer credential and operation inputs;
they cannot supply authoritative tenant, project, actor, worker, or task leases.
The older `PostgresRunStore` remains a privileged storage adapter and must never
be exposed as a generic remote RPC interface.

## Credentials and roles

Deployment code calls `bootstrap` with its trusted PostgreSQL client to establish
one initial administrator per tenant/project. A PostgreSQL advisory lock serializes
bootstrap. Any existing credential in that scope prevents bootstrap from opening
again, even if every administrator has expired or been revoked. A host holding the
database credential is part of the trust boundary; bootstrap is not a public API.

Credentials contain 256 bits from the operating system random source and a fixed
`wf1_` format. The database stores only SHA-256 digests of high-entropy bearers.
`IssuedCredential` deliberately has no serializer and redacts Debug; hosts call
`expose_secret()` once to deliver it through private provisioning. The supported
TTL is 1 millisecond through 1 hour, measured with primary database time.

Each credential has one immutable role, one tenant/project, one actor and an
immutable capability allowlist. Separate credentials are required for separate
roles. Administrative issuance always inherits the administrator's scope.

| Role | Operations |
| --- | --- |
| Administrator | Issue, rotate and revoke credentials; read audit and unresolved assignments; clean expired artifact transfers |
| Definition maintainer | Validate definitions; publish an immutable bundle; read runs |
| Runner | Validate definitions; start only a published bundle; read runs; pause/resume/cancel with CAS |
| Viewer | Read run state, history, inbox and waits |
| Approver | Read runs; submit a human decision bound to a frozen responder/subject policy |
| SignalSource | Read runs; deliver external events to a frozen event policy; cannot approve human waits |
| Scheduler | Read runs, acquire/release ownership, advance timers, dispatch authorized tasks/effects |
| Worker | Retrieve its own task and return results; publish/read artifacts within its explicit capability policy |
| Recovery | Read runs, audit and unresolved assignments; acquire/release ownership for audited effect reconciliation; control runs; acknowledge an existing recovery barrier; clean expired artifact transfers |

Publication admits an immutable bundle digest and freezes its workflow,
capability and policy versions in one transaction, before any start. Conflicting
content returns `binding_conflict`; an identical publication is idempotent.
The shared identity calculation and sorted lock order also apply to run commits.
Older digest-only publications acquire these bindings when republished or started;
existing run bindings remain authoritative.
This publication allowlist does not yet provide shared draft editing, withdrawal,
or the full DefinitionRegistry authoring history.

There is no general `apply(Event)` endpoint. Workers cannot submit approvals,
change definitions, write run state, acquire scheduler ownership, or read whole
runs. Successful task submission returns only revision and duplicate status.
Result schema, exact request digest, protocol, graph transitions, current lease,
attempt and prepared task are checked against the durable reducer. An already
settled assignment cannot be fetched for another execution.

## Transaction and revocation semantics

Every operation authenticates from the credential digest and takes a `FOR SHARE`
lock on the live credential row. PostgreSQL derives the resource scope from that
row. Credential revocation updates the same row. The lock remains held across
authorization, reducer changes, assignment writes, audit and commit, which gives
revocation and submission a database order:

- A submission holding the lock can commit before revocation. Revocation waits.
- Once revocation commits, an operation waiting to authenticate reads the revoked
  row and fails. Returning a success from `revoke` therefore fences later commits
  using that credential, including transactions from other service processes.

Operation changes use a savepoint. An application rejection rolls back its
speculative changes while retaining a bounded rejection audit. Final database
time checks revalidate credential, worker, assignment, reducer and callback
admission windows after operation and audit writes. Failure to confirm durable
commit returns an error, and callers retry using the operation's existing
idempotency semantics. Database loss cannot return speculative success.

Dispatch also locks the target worker credential, verifies the same tenant/project
and an exact capability ID/version/contract digest, then stores the assignment in
the same transaction as the prepared attempt. The worker sends an assignment ID
and result, never an authoritative lease. Delivery and result submission compare
the stored prepared task with execution history and verify the exact current
lease, including generation and epoch. Even an identical result retry is rejected
after the lease is released or replaced.

Rotation issues a new credential and revokes the old one atomically. Outstanding
assignments remain bound to the old credential. Administrators can page through
them for reconciliation and let fenced scheduler recovery create new attempts;
rotation never silently transfers a running task to a new identity. The [effect protocol](remote-effects.md) adds separate assignments with exact
target/principal/policy grants, single delivery and audited manual resolution. Revocation cannot physically stop code already running in a worker.

## Audit and remaining boundaries

Audit records contain scope, actor, public credential ID, fixed operation name,
bounded resource identifier, fixed outcome and database timestamp. Raw tokens,
token digests, request payloads, raw errors, SQL and connection strings are not
copied into the audit. Returned operation errors contain fixed messages.
Unauthenticated attempts and database failures have no claimed durable audit;
transport hosts must supply bounded ingress failure metrics. A final
expiry/commit failure can roll back the audit with the entire transaction.
Caller-supplied workflow content is still application data; this API does not
claim to detect arbitrary secrets deliberately embedded in it.

The schema is additive (`workflow_access`, version 2); it neither alters existing
run images nor upgrades an unknown access schema. Administrative DB access can
modify these tables, so deployment must restrict it and apply backup/retention
policy. This is not a tamper-proof audit or an independent identity provider.

The [HTTPS service](remote-service.md) exposes these application operations with
explicit CA verification, host secret references, bounded admission, and separate
scheduler/worker processes. Its real PostgreSQL acceptance fixture kills an owner
among two schedulers and three workers, resumes a stale worker, and compares the
result with local execution.

[R14 acceptance](security-acceptance.md) now defines the authenticated execution
boundary, broker credential leases, private audit export and retention/archive
policy. Shared definition migration and full scoped database restoration are
implemented separately. R09 still owns scheduling quotas/fairness, routing/drain
and performance/fault evidence. Enterprise identity provisioning and arbitrary
third-party executable isolation remain privileged deployment integrations.

The mandatory PostgreSQL CI job runs the ignored integration tests in this crate.
They exercise real PostgreSQL locks through independent connections, cross-scope
negative cases, role separation, a complete builtin workflow through approval,
forged result/contract rejection, rotation/revocation, stale leases and expiry
after speculative mutation. A test observes `pg_stat_activity` lock waits before
releasing its barrier rather than assuming a scheduling delay proves ordering.

Implementation references: [PostgreSQL row locks](https://www.postgresql.org/docs/17/explicit-locking.html#LOCKING-ROWS)
and [getrandom OS entropy](https://docs.rs/getrandom/0.4.3/getrandom/fn.fill.html).

## Artifact authority

[Shared artifact transfer](shared-artifacts.md) uses an optional immutable policy
on each exact capability rule. Worker reads follow declared typed inputs and their
lineage; writes derive provenance from a current assignment. Run-reading roles
can inspect scoped artifacts. Short-lived grants bind scope and credential, and
every phase participates in the same revocation and final-time transaction checks.
The verified PostgreSQL catalog supplies artifact evidence to result settlement
and recovery. It adds version-1 storage without modifying run images.

Approval and signal endpoints require a `wait_policies` binding; previously
published unbound waits remain readable but cannot receive remote responses.
Publish a new protected bundle for new runs. Existing local trusted-admin Inbox
semantics remain supported. Access schema 1 installations explicitly run
`workflow service migrate-access server-binding.json` before issuing a
`signal_source` credential; new installations create schema 2. See
[durable approval acceptance](approval-acceptance.md).
