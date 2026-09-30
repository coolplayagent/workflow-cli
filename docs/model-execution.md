# Bounded model policy execution

A task may delegate local choices to a model while the workflow owns ordering,
typed handoffs, leases and mandatory gates. The business definition contains a
capability reference and a model policy reference. Its run bundle freezes the
policy, complete task/tool contracts, goal and budgets. Provider, endpoint, model
and credential lookup belong to a separate host binding.

## Components and authority

| Component | Responsibility |
| --- | --- |
| `workflow-models` | Portable `ModelAdapter`, policy validation, bounded tool loop, explicit records and deterministic verification |
| `workflow-model-http` | OpenAI Responses and Anthropic Messages HTTP formats, credential lookup, deadlines and response decoding |
| `workflow-worker` | Exact capability/policy selection, request/grant/output validation and invocation deadlines |
| `workflow-kernel` | Resolve frozen policies and tool contracts; deterministic transitions without model/network calls |
| `workflow-runstore-sqlite` | Immutable policy version locks, lease-fenced result settlement and record verification on recovery |
| `workflow-service` / PostgreSQL access | Scoped model-policy grants, HTTPS task delivery and transactional verification of the same records |
| `workflow-cli` | Compose registered tools and host bindings, run the driver, inspect policies and replay records |

Each component is an explicit Bazel `rust_library`. Provider libraries do not
enter the policy executor or kernel. The CLI currently registers the compiler
capabilities as tools; library hosts can register other read-only adapters.
Missing or changed tool contracts are rejected before the first model call.

The model can propose exactly one `call` or `complete` action per response.
Calls must name an exact allowed read-only capability, with valid typed inputs.
Completion must supply valid typed task outputs and a short visible summary.
Unknown fields/actions, undeclared tools, writes and recursive calls to the same
task capability are rejected. Tools receive independent requests linked to the
outer request by the session record. They do not inherit a fictitious workflow
attempt or acquire the ability to commit run state.

Typed outputs remain model claims. A successful model task does not prove that
those claims match its observations. Require independent validation tasks and
frozen postconditions for objective acceptance. The model cannot edit policies,
skip mandatory gates, commit transitions or turn an UNKNOWN into a PASS.
This release does not promote nested tool observations into independently settled
workflow evidence or attach artifacts to a model result. A gate that requires
missing evidence therefore waits at UNKNOWN even after a typed model success.

## Run one business bundle with another provider

`examples/models/start.json` uses `sop.inspect@1.0.0` with a frozen policy that
allows `workflow.validate@1.0.0`. Its goal asks the model to invoke the validator
and return the observed result. The following decision consumes `valid`.
This is a replacement demonstration, without an independent acceptance gate.

```sh
workflow model check-policy examples/models/policy.json > /tmp/checked-policy.json
workflow schema model-http-binding
workflow model describe-binding examples/models/openai.json
```

Copy either `openai.json` or `anthropic.json` to a host-owned file, replace
`HOST_SELECTED_MODEL` with a model available to that host, and provision the
named environment variable through the host's secret mechanism for local use.
Shared production CLI execution requires a [short-lived broker lease](security-acceptance.md)
in `credential`, with `api_key_env` omitted. Do not put the
key value in JSON, a workflow, an artifact or a command-line argument.
`describe-binding` validates configuration without reading the key or contacting
the provider. Model names and availability are host choices; examples do not
promise any particular model's availability.

Create the driver binding from the CLI's policy digest, not a hand-written hash:

```sh
python3 - /tmp/checked-policy.json /path/to/host-model.json /tmp/model-bindings.json <<'PY'
import json, sys
policy = json.load(open(sys.argv[1]))['result']['binding']
http = json.load(open(sys.argv[2]))
with open(sys.argv[3], 'w') as out:
    json.dump([{'policy': policy, 'http': http}], out)
PY
workflow run init /tmp/model-runs.db
workflow run start /tmp/model-runs.db examples/models/start.json
workflow run drive-models /tmp/model-runs.db model-inspect host-1 10 /tmp/model-bindings.json
workflow run execution-history /tmp/model-runs.db model-inspect 0 100
workflow run verify /tmp/model-runs.db model-inspect
```

Use a fresh database for the replacement demonstration, change only the HTTP
binding and repeat with the same start document. The workflow and bundle digests
remain identical; each execution records its own configured/resolved model and
binding fingerprint. This does not promise identical real-model outputs.
Changing a policy under the same ID/version is rejected; publish a new version.

The binding authorizes sending the task's input values, frozen policy and explicit
prior tool observations to that endpoint. Host operators must choose endpoints
and data access consistent with their task authorization. A read-only adapter
contract is not an operating system sandbox or a general prompt-injection defense.

## Remote model workers

The same prepared request, execution grant and model record cross the existing
authenticated HTTPS task protocol. A worker credential must allow the exact task
capability ID/version/digest and include its frozen `model_policy` binding:

```json
{
  "model_policy": {
    "policy": {"id":"sop.inspect-policy","version":"1.0.0"},
    "digest":"COPY_THE_DIGEST_FROM_CHECK_POLICY"
  }
}
```

This is a fragment of `CapabilityRule`; obtain the real digest from `check-policy`
as above. Its `task_contract_digest` supplies the rule's `contract_digest`.
A capability-only credential does not authorize model execution.
Changing the goal, tools or budget changes the policy digest and requires a new
grant. One credential grants one policy per task capability ID/version; provision
distinct identities for different policies using that same task contract.

Give the worker the published bundle and its host provider bindings. The bundle
contains policies and contracts; credentials and endpoints stay in host bindings.
For the example, extract `bundle` from the start document, then run:

```sh
workflow remote work-models worker-client.json bundle.json model-bindings.json 1000 100
```

The ordinary remote scheduler dispatches these tasks. `work-models` registers
the builtin tools and frozen model adapters, verifies local bindings before
polling, and uses the same `work_once` implementation as deterministic workers.
Both local registration and server dispatch require the full policy digest.
The server reconstructs the authoritative assignment before accepting a result;
missing or inconsistent records, wrong workers and stale leases cannot commit.
`settled_tasks` includes recorded model failures; inspect run status for success.

The HTTPS envelope remains version 1 while its assigned model request/result use
worker protocol 2. Older provisioning commands reject the new grant field;
older builtin-only workers cannot execute a model policy. Roll out the server
and scoped model worker before assigning model work. Provider substitution changes
only the worker's HTTP binding, and replay never invokes that provider again.

`https_model_bindings_records_failures_and_policy_authority_contract` uses real
HTTPS/PostgreSQL and separate worker processes with a deterministic adapter and
both provider HTTP formats. It checks equal business outputs against in-process
execution, policy denial/rollback, forged-result rejection, provider outage and
illegal transition proposals, and reopening without more provider calls. These
are sandbox protocol fixtures, not live-model quality or cost measurements.

## Records, protocol and recovery

Direct calls retain worker protocol 1 and omit all new fields. Model calls use
protocol 2, an exact `model_policy` binding in `WorkRequest`, a matching grant
version and a required `model_record` in `WorkResult`. Model policy/proposal/record
schema versions are independently 1. Export them with `workflow schema
model-policy`, `model-proposal`, `model-record` and `model-http-binding`.

The host records the full prepared request, policy digest, adapter/configuration
identity, requested and resolved model, visible response ID, reported token usage,
each explicit proposal, each tool request/result or rejection, monotonic event
times and the derived outcome. Missing usage remains unknown. The record excludes
credential values and hidden reasoning; it can still contain sensitive business
inputs and tool outputs. Treat the database and exported history accordingly.

`workflow model dispatch <policy> <http-binding> <request> <grant>` returns a raw
worker result. It is not a state commit. A trusted host must submit the result via
`run finish` with the current lease and attempt. `model check-record <policy>
<request> <result>` deterministically replays recorded decisions without network
or tool invocation. It rejects mismatched context/request/policy digests, changed
tool inputs, invalid output contracts, incomplete or extra events and event times
after completion. It does not authenticate a provider, grant or live lease.

SQLite settlement performs this replay and checks the outcome before atomically
committing result, event, state and receipt. Recovery rechecks the same history.
Raw successful task events are refused in model-policy runs. Digests establish
content binding, not signatures; local adapters and database integrity are trusted
host boundaries. A restarted completed run does not sample the model again.

Storage schema 5 protects these semantics from older readers. Use explicit
`run migrate` for schemas 1–4, supplying `--artifacts` when existing runs need it.
Old protocol 1 bytes, request/bundle digests and completed run history are preserved.

## HTTP and budget boundaries

The adapters send a non-streaming JSON proposal instruction, with no provider
built-in tools or function execution. OpenAI uses `instructions`, textual `input`,
`max_output_tokens` and `store: false`; Anthropic uses `system`, a user message and
`max_tokens`. These fields follow the official [Responses API](https://developers.openai.com/api/reference/cli/resources/responses/methods/create)
and [Messages API](https://platform.claude.com/docs/en/api/messages/create).
Provider output is untrusted: only completed assistant text containing the exact
proposal JSON is accepted. Hidden reasoning blocks are discarded. Refusals,
truncation, malformed/duplicate-key JSON and unexpected tool blocks fail closed.

HTTPS is required. Plain HTTP requires an explicit flag and a literal loopback
address for fixtures. Redirects, proxy inheritance and automatic HTTP retries are
disabled. Each request has the smaller of its remaining deadline and 60 seconds;
raw provider responses are capped at 256 KiB. Error bodies are not persisted.
The implementation uses reqwest's [client controls](https://docs.rs/reqwest/latest/reqwest/blocking/struct.ClientBuilder.html)
and [never-retry policy](https://docs.rs/reqwest/latest/reqwest/retry/fn.never.html).

Policies bound calls (1–16), tools (0–16), context (1–256 KiB), response (128 B–64
KiB) and requested output tokens per call (1–32768). The aggregate worker result
still has a 2 MiB limit; satisfying each local budget does not guarantee that all
records fit. Declared failures include `model_unavailable`, `model_invalid_response`,
`model_refused`, `model_budget` and `model_deadline`, all permanent. Driver exit 0
means a durable operation succeeded; inspect the business snapshot status.

Bounds are per attempt. A timeout or process crash can leave provider billing
unknown; a later attempt may incur another charge. No atomic run-wide monetary
budget, streaming, mid-session resume or exactly-once model billing is claimed.
Synchronous custom adapters must cooperate with deadlines; lease fencing rejects
late results but cannot terminate arbitrary Rust code. Committed records replay;
uncommitted in-flight sessions may be reattempted under the existing retry bound.

## Verification and remaining scope

Tests cover a deterministic fake model, real built-in tool execution, both actual
HTTP request/response formats through loopback fixtures, the same business bundle
under both bindings, durable rejection/recovery, mandatory UNKNOWN preservation,
and restart without provider access. No production provider credentials were used;
these tests establish wire and authority behavior, not live service compatibility
or model quality. Storage migration was also exercised against an actual prior
v4 binary/database, preserving snapshots and execution records byte for byte.

The [R02 acceptance guide](model-boundaries-acceptance.md) maps the public
interfaces, local/remote contract matrix and executable provider replacement
example. Dynamic model tools remain read-only; declared write nodes use the
durable effect executor and separate effect permissions. Run-wide cost accounting,
model quality evaluation, isolated tools, workspace/gate binding and automatic
artifact publication remain separate roadmap work.
