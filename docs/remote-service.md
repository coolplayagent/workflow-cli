# Authenticated remote execution

`workflow-service` carries the existing authenticated PostgreSQL application
operations over HTTPS. The API process, scheduler and worker are separate
processes. PostgreSQL holds run state, command intents, assignments, identities
and audit; a notification or HTTP response is never the only task record.

The server accepts HTTP/1.1 over TLS 1.2/1.3 at `POST /v1/operations`. There is no
plaintext listener or generic storage/event endpoint. Each request has one
`Authorization: Bearer ...` header, `Content-Type: application/json`, and this
envelope:

```json
{
  "protocol_version": 1,
  "request_id": "status-1",
  "operation": {"type": "get", "run_id": "review-1"}
}
```

`protocol.rs` defines the complete tagged operation/response contract. Unknown
versions, operations and fields are rejected. Requests cannot provide tenant,
project or actor authority. Read, publish, start, approve, scheduler lease/dispatch,
worker assignment/finish/fail, audit, revocation and recovery acknowledgement all
call `AuthenticatedService`; the same transaction validates identity and state.
Credential creation/rotation and bootstrap are privileged local library operations,
not public RPCs. The CLI exposes local bootstrap and issuance with exclusive private
file output, and never prints the resulting bearer.

Replies repeat the protocol version and request ID, and contain a Rust/Serde
`Result` (`{"Ok":{"type":...,"value":...}}` or `{"Err":...}`). The client checks
the response variant against the requested operation and rejects redirects. HTTP
success follows durable application commit. Authentication failure is 401, missing
scoped resources 404, invalid operation input 400, transition conflicts 409, and
database/capacity failures 503. Malformed HTTP/protocol requests may receive a
bounded fixed text error before a reply envelope exists.

The request ID correlates a response; it is **not** a universal idempotency key.
Start retries bind the run ID and original definition/inputs; result retries bind
the server assignment and exact result. Lease acquisition IDs are unique per
scheduler boot and attempt. A timeout/disconnect can happen after commit, so the
client performs no automatic mutation replay. Reconcile durable state/assignments
before retrying. Responses exceeding the response limit can also follow a committed
operation and return an unavailable error.

## Host configuration

See `examples/remote/` for bindings without credentials. Paths resolve against the
calling process's directory. Supply a service certificate whose SAN matches the
client endpoint, a trusted CA file and a private TLS key. Clients trust only their
configured CA, verify the hostname, disable redirects and implicit proxy discovery,
and reject credentials/query strings in endpoint URLs.

Secrets are `{"type":"file","path":"..."}` or
`{"type":"environment","name":"..."}` references. Files must be regular,
owned by the effective user, have no group/other permissions, have one hard link,
and not be final-component symlinks. These checks use the opened descriptor.
Secret Debug output is redacted. References are host configuration, never workflow
inputs. A file reference is reread for every request/connection, so an atomic file
replacement can rotate a bearer or database credential without changing a workflow.
Environment changes require a host restart; secret memory is not a protected vault.

Database binding `tls` requires an explicit CA and `sslmode=require`, with no
plaintext fallback. Binding `local` permits only explicit numeric loopback TCP
addresses or Unix sockets, including checks on any `hostaddr` override. A trusted
host owns this configuration; remote requests cannot change the connection or TLS
settings. The bundled integration fixture uses an isolated loopback PostgreSQL.

For a configured host, the CLI operations are:

```sh
workflow service bootstrap server.json tenant project operator secrets/admin
workflow service issue server.json admin-ref.json runner-provision.json secrets/runner
workflow service serve server.json
workflow remote call client.json request.json
workflow remote schedule scheduler-client.json scheduler.json 1000 100
workflow remote work worker-client.json 1000 100
```

Create private secret directories first. Bootstrap output is the public credential
ID and expiry. Issuance output is likewise metadata; the token is written to the
specified new 0600 file. Failure while delivering a newly issued credential can
leave its database identity without a usable output file: use the administrative
audit to reconcile/revoke it. Do not retry bootstrap by deleting identity records;
bootstrap never reopens an existing scope. Initial administrator recovery requires
the trusted database operator.

## Scheduling, recovery and resource bounds

The scheduler scans a bounded page of scoped runs, remembers current leases and
dispatches through a configured list of worker identities. It rotates pages and
worker choices, and creates a fresh boot nonce for acquisition IDs. Its local clock
only discards expired hints; the database decides lease validity. The worker scans
its own pending assignment IDs, validates the current assignment, invokes the same
`Worker` contract as local execution and submits the result to the API. Lost
notifications or dispatch replies can therefore be recovered by scanning.

Worker transport failures use a fixed diagnostic in the execution ledger. Already
settled assignments are not executed again. Uncertain completion transport errors
are surfaced for reconciliation. This worker CLI registers builtin read-only
capabilities; this API does not dispatch external effects. A host may compose its
own compatible Worker through the library. Possessing an assignment does not grant
arbitrary filesystem, model, artifact or external write access.

Admission limits are explicit: up to 256 concurrent connections and 32 operations,
with defaults supplied by the binding rather than hidden runtime defaults. A
connection slot includes TLS handshake time. Handshake/header/body reads each have
a five-second bound, headers have 32 entries/32 KiB buffer bounds, requests 2 MiB,
and replies 16 MiB. Connections do not remain alive for reuse. Database work runs
outside the async reactor and retains its operation permit even after HTTP timeout
or disconnect. No request body, header or raw database error is logged.

SIGINT/SIGTERM stops API admission and drains connections plus admitted database
operations, with a bounded drain failure reported as an error. Scheduler/worker
CLI loops have explicit iteration and polling bounds for service-manager use.
They do not renew leases: choose a lease longer than the supported task deadline;
expired ownership is reacquired with a new epoch. Pause keeps in-flight ownership
until it expires rather than proactively fencing accepted tasks.

These mechanisms do not complete R09/R14: shared quotas and tenant/model/capability
fairness, priorities, measured throughput/p95 delay, dead-letter administration,
worker version routing and rolling drain, lease renewal with assignment rebinding,
public edge admission policy, sandbox/egress, provider secret grants, artifact archive/deletion policy,
and authenticated external-effect reconciliation remain required. This transport
is not permission to launch a general multi-tenant public deployment.

## Executable acceptance evidence

The service tests use generated certificates and actual HTTPS sockets. Normal
Cargo/Bazel checks cover wrong CA/hostname, plaintext and credential-bearing URL
rejection, malformed/unknown protocol input, oversized bodies, private secret
permissions and graceful API exit. The mandatory PostgreSQL CI job also runs the
ignored multi-process fixture:

- Two real scheduler processes and three worker processes share PostgreSQL through
  the TLS API. One worker computes a read-only result and is SIGSTOPped; the test
  waits for the kernel stop notification and kills the owning scheduler.
- A successor acquires a higher epoch after the five-second lease expires. Its
  workers complete the builtin fork/branch/loop workflow through human approval.
- The old worker is resumed; its result is explicitly rejected and the run remains
  unchanged. Final frames, inputs/outputs, routes and status match local execution.
- Cross-tenant reads and task lookup are rejected. A read-only database cannot
  acknowledge a new run. Credential file rotation takes effect on an existing
  client, and child logs contain no bearer values.

The test prints observed takeover-to-approval duration with its lease/scan settings.
The 60-second assertion is a test observation ceiling, not a production RTO. This
fault model covers process loss and a paused stale worker, not external write
effects, whole-database loss or a multi-host network partition. It claims no
production RPO, throughput or fairness metric.

Implementation references: [Hyper HTTP/1 connection bounds](https://docs.rs/hyper/1.11.1/hyper/server/conn/http1/struct.Builder.html)
and [PostgreSQL rustls integration](https://docs.rs/tokio-postgres-rustls/0.14.0/tokio_postgres_rustls/).

## Shared artifact operations

[Shared artifacts](shared-artifacts.md) adds assignment-bound chunk uploads,
credential-bound expiring downloads, typed input/lineage authorization and
artifact-backed result recovery. Content is stored transactionally in PostgreSQL.
The artifact CLI verifies complete content before returning a reference or writing
a private download. Object storage and full retention/backup policy remain open.
