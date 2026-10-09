# R14 shared execution security

The supported shared boundary is the authenticated PostgreSQL application facade
and its HTTPS transport. Credentials determine tenant, project, actor and role;
neither a run ID nor a caller-supplied namespace grants access. Database clients,
provider bindings, executable registration and credential delivery belong to the
trusted host. Direct SQL and the raw local/RunStore ports are administrative APIs.

## Authorization at execution

| Identity | Authority |
| --- | --- |
| Definition maintainer | Validate and publish immutable contracts in its own scope; cannot start runs or approve them. |
| Runner | Start and control scoped runs; cannot publish, approve or dispatch. |
| Viewer | Scoped read operations; cannot change state or export administrative audit. |
| Approver / signal source | Submit only the matching declared decision/signal policy. Actor and scope come from authentication. |
| Scheduler | Acquire fenced ownership and dispatch exact contracts to explicitly permitted workers. |
| Worker | Retrieve and settle its own current assignments. Exact capability version, contract digest, model policy, effect policy and artifact policy are checked by the authority. |
| Recovery | Inspect unresolved work, perform bounded reconciliation under current ownership and export scoped audit. |
| Administrator | Provision/revoke identities, export scoped audit and perform explicit storage/definition migration; no general run-start, approval or worker authority. |

All reads/writes filter both tenant and project. The same authenticated transaction
locks the credential against revocation, checks assignment/run/attempt/generation
and rechecks database time before commit. Rotation creates a new credential and
revokes the previous one; assignments never transfer implicitly. Revocation cannot
undo a request already received by a provider. Delivered effect assignments remain
in the outstanding ledger and a successor must query/reconcile the original
operation identity. See [effect recovery](remote-effects.md).

Model outputs are proposals under the frozen node policy. Only listed read-only
tools can execute, and the actual Worker checks the full contract. Completion
data cannot replace a graph, supply an approval or become an administrative RPC.
The service has typed bounded operations and rejects unknown fields/operations;
there is no generic reducer-write or shell-execution RPC.

## Short-lived credentials and rotation

Provider/gateway bindings can reference a broker-owned credential lease:

```json
{
  "schema_version": 1,
  "provider": "openai_responses",
  "model": "HOST_SELECTED_MODEL",
  "endpoint": "https://model-gateway.example/v1/responses",
  "credential": {
    "path": "/run/workflow/worker-model-lease.json",
    "principal": {"tenant": "example", "project": "review", "actor": "model-worker"}
  }
}
```

The private delivery file has `schema_version: 1`, that exact `principal`, an
`audience` equal to the complete configured endpoint, positive
`not_before_unix_ms`, `expires_at_unix_ms`, and `secret`. Lifetime is at most
300,000 ms. `secret` is a provider/gateway-issued short-lived bearer, never a
workflow parameter. The broker authenticates the host identity, requests the
appropriate upstream grant, writes a new private file, fsyncs it and atomically
replaces the previous file. The parent directory must be private and controlled by
the host. No credential value is accepted in a binding or CLI argument.

The provider/gateway must enforce the issued key's identity, resource scope and
expiry. Wrapping a permanent API key in a local expiry document does not make that
key short-lived upstream. Deployments whose vendor exposes only permanent keys
place those keys in a trusted gateway and deliver expiring gateway credentials to
workers. The gateway is an external host integration, not an embedded vendor IAM
service. Provider-side key revocation and authoritative effect lookup remain part
of that integration's contract.

`workflow-credentials` rereads and verifies the delivery file on every model or
effect call. It rejects wrong principal/audience, early/expired/overlong leases,
relative paths, group/other-accessible files, wrong owners, hard links, symlinks,
special files and oversized files. The credential has redacted Debug and no
serialization implementation. The network timeout is bounded by the credential
expiry as well as the task deadline. Reference paths and binding digests remain
unchanged across rotation.

`remote work-models` and `remote work-effects` require these leases for production
endpoints. The assignment request includes the expected host principal as an
additional restriction; the server compares it with the authenticated identity
inside the same delivery transaction. A different tenant, project or actor is
rejected before provider I/O and before consuming effect delivery. This remains
safe if a service credential file changes between requests. SDK hosts use
`work_once_bound` / `work_effects_once_bound` with their configured principal.
Host bindings in one worker must agree on that principal.

Named `api_key_env` references remain supported for local trusted execution and
explicit literal-loopback HTTP fixtures. A binding must select exactly one
source. Shared production CLI commands reject an environment-only provider
binding. No browser redirect, ambient proxy or automatic HTTP retry is permitted.

Successful provider replies are also untrusted. Before retaining any proposal,
model identifier, receipt, output or reason, the adapter rejects a reflection of
the current invocation credential, including decoded JSON escapes and object
keys. Model reflection becomes a fixed invalid-response failure. An effect
reflection becomes a fixed unknown observation, preserving query-first recovery;
it is never converted into proof that no write occurred. Provider error bodies
are discarded. The transport rejects its current bearer in request data,
including byte-array content. These checks cover known invocation credentials,
not arbitrary unknown secrets or encoded exfiltration. Application authors must
keep secrets out of business data and artifact bytes.

## Workspaces, arguments and network scope

Registered builtin capabilities accept typed data; there is no arbitrary command
or shell interpolation facility. Actual isolated execution allocates a private
workspace for each attempt and records its capability/tool/environment identity.
Workspace sources use a host-selected local Git object database and full object
IDs. They never execute source scripts, hooks, filters or filesystem monitors.
Git runs with explicit argument vectors, a cleared environment, disabled network
protocols/lazy fetch and disabled global/system configuration. Provider/database
credentials are absent from that subprocess. Symlinks, hard links, special files,
parent traversal and undeclared outputs fail before publication. Reviewed merge
content is a new retained revision requiring fresh evidence.

HTTP adapters can contact only the host-configured endpoint and fixed effect
`write`/`query` suffix. Task/model output cannot choose a host, URL, proxy, command
or extra capability. HTTPS is mandatory except for explicit literal-loopback
fixtures. This scope provides no general-purpose hostile-code runner. A host
adding arbitrary third-party executable adapters must first provide OS/container
isolation and an egress policy; registration is privileged host code. Filesystem
separation alone is not a security sandbox against code running as the same UID.

## Artifact access, retention and archives

The shared artifact protocol requires a bearer on every chunk. Download grants
bind tenant, project and the exact credential, plus the assignment for worker
reads, and expire within both the configured TTL and credential/assignment
windows. Copying a grant to another credential or tenant cannot authorize data.
Declared typed inputs and transitive lineage determine worker reads. Assignment
policy determines writes and the service supplies provenance; client path names
never become host filesystem paths. Content size and digest are checked on upload,
recovery and complete download. Shared CLI download writes a new private file
only after content verification.

S3 presigned URLs from the trusted object-storage adapter are bearer capabilities;
they do not replace the authenticated shared download protocol. Shared tenant
clients receive credential-bound protocol grants, not raw S3 signing credentials
or unprotected presigned URLs.

| Data | Retention / deletion policy |
| --- | --- |
| Published definitions, run history, execution/effect proofs, accepted decisions, committed artifacts and lineage | Retain for the run's lifetime, including completion, cancellation and migration. No online selective delete API; deleting a referenced dependency would invalidate replay/recovery. |
| Credentials and audit | Expired/revoked credentials lose authority but their non-secret identifiers and hashes remain for audit and reconciliation. No online audit truncation. |
| Staged uploads / uncommitted workspace or object content | Only the bounded orphan-cleanup protocols may remove expired uncommitted objects. Committed dependencies are excluded. |
| Archives | Local verified backups retain definitions/artifact closure; full PostgreSQL backup includes every authority/access/artifact/effect schema. Treat the archive as confidential, restrict readers, and store encryption keys separately under deployment policy. |
| Permanent retirement | Stop admission, reconcile effects, revoke credentials, verify the archive and its restore, then let the deployment custodian retire the entire isolated store and its backups under its retention decision. Tenant actors and online APIs cannot perform physical database deletion. Record the custodian, archive digest, retention decision and deletion evidence in the deployment audit system outside the store being deleted. |

This repository does not infer a legal retention period or silently purge data.
Selective tenant erasure across shared archives is not an offered API. Full
database restore preserves tenant/project ownership, validates dependency closure,
revokes old identities and establishes fresh ownership before any business work.
The [R04 restore drill](shared-recovery.md) checks this with actual pg_dump/restore,
fresh scoped administrators, stale credential rejection and effect reconciliation.

## Scoped audit export

```sh
workflow remote audit-export administrator-client.json /private/new-audit.json
```

Only administrator/recovery credentials may export their own tenant/project.
One PostgreSQL statement reads the complete scope audit at a consistent snapshot;
the export's own audit entry is committed afterwards. The export contains scope,
authenticated exporter, ordered sequence/actor/credential ID/operation/resource/
outcome/time entries and a canonical SHA-256 content digest. It excludes request
payloads, raw errors, credential values, hashes of bearer values and provider
responses. Normal audit records contain only bounded identifiers and fixed
outcomes. Export rejects the entire request above 10,000 entries or 8 MiB of
canonical content; it never silently truncates or loops over its own new entries.
Larger deployments need an independently governed database archive until a larger
audited export protocol is introduced.

The CLI verifies the document before exclusive 0600 creation, fsyncs bytes and the
parent directory, and prints only scope/count/digest. Existing paths are not
overwritten. The digest detects corruption against a retained value; it is not a
signature or a defense against a privileged database administrator who can rewrite
both data and digest. Custodians retain the exported digest through their own
authenticated archive channel. Unauthenticated ingress and rolled-back final
expiry checks do not claim durable application audit; the host owns bounded
transport failure monitoring.

## Executable admission evidence

The normal Cargo/Bazel suite tests provider success/error reflection, malformed
leases, file attacks, policy restrictions, workspace traversal and secret-free Git
subprocesses. Mandatory PostgreSQL CI runs the real authorization, artifact,
revocation/reconciliation, restore and HTTPS contracts, plus:

```sh
python3 examples/security/acceptance.py target/debug/workflow
```

This CLI drill starts a TLS service and an independently listening provider. It
uses two authenticated tenants, refuses foreign runs/history/assignments, refuses
three mismatched lease-principal fields before provider I/O, rotates the provider
credential between two calls in one worker process without changing definitions
or binding identity, rejects the retired credential at the provider, blocks four
malicious output modes, verifies export roles/digests/private files and checks
captured logs, histories and exports for every issued fixture secret.

| Issue #9 acceptance | Required evidence |
| --- | --- |
| Foreign run/event/task/artifact access and result submission denied | `authenticated_scope_roles_dispatch_result_and_approval_contract`, `authenticated_artifact_upload_download_recovery_and_scope_contract`, `tls_artifact_transfer_resume_binding_and_result_recovery_contract`, security CLI. |
| No escalation, forged approval or graph mutation from outputs | Policy/worker contract tests and security CLI's `escalate`, `approval`, `graph` modes; graph digest and node set remain fixed. |
| Revoked worker/old lease cannot settle; possible effects stay reconcilable | `worker_revocation_rotation_old_lease_and_expiry_reject_results`, `effect_revocation_takeover_requires_query_and_retains_operation_identity`, `provider_lease_principal_is_checked_in_the_assignment_delivery_transaction`, real HTTPS provider crash recovery. |
| Rotation without definition edits; secret-free records/exports | Lease contract tests, both provider-wire reflection suites, Git subprocess test and security CLI's in-process rotation plus retained-output scan. |
| Expired/copied/traversal download grants cannot reveal content | Artifact scope/revocation/expiry/lineage contracts, real HTTPS chunk transfer, private filesystem tests. |
| Role/recovery/audit export positive and negative cases; restore keeps scope | `audit_export_is_complete_scoped_role_checked_and_tamper_evident`, security CLI, `database_restore_verifies_dependencies_atomically_and_fences_every_old_identity`, R04 pg_dump/restore drill. |

These are reproducible contract observations, not a claim of penetration-testing
coverage, protection from a compromised trusted host, arbitrary-code isolation,
production throughput or availability. R09 still owns scheduling/quotas/drain and
R12 owns its operational visibility requirements.

<!-- book-navigation -->

[Contents](README.md) · [中文](zh/security-acceptance.md) · [Previous: Local platform acceptance](local-acceptance.md) · [Next: Troubleshooting and contributing](troubleshooting.md)

<!-- /book-navigation -->
