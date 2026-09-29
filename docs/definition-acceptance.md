# R01 definition acceptance and baseline

The definition contract is a portable, versioned graph with mandatory checks
before execution. JSON, YAML and the Rust builder produce the same IR. The local
CLI and authenticated HTTPS API use the same parser, static checker, canonical
digest and diagnostic report. Deployment bindings contain queues, endpoints and
credentials; these are not fields in the business definition.

## Acceptance map

| R01 obligation | Executable evidence |
| --- | --- |
| Review acceptance/rejection, parallel testing and success after two failed repair rounds | Five committed `examples/kernel/*.json` scenarios; `examples/validation/baseline.py` asserts every expected node state, dispatched task count, loop frame outcome, unique instance and loser reconciliation command |
| Stable import/export and IDs; precise edits; immutable releases | `workflow-ir` JSON/YAML/builder digest tests; `workflow-definitions` node/edge CRUD and diff tests; `workflow-registry-sqlite` revision CAS, tombstones, process races and interrupted transactions; baseline asserts canonical round trips, exact patch paths, stale revision rejection and unchanged v1 after v2 publication |
| Reject invalid graphs before execution | Validator tests plus the real HTTPS matrix reject unreachable nodes, dangling references, implicit cycles, unbounded loops, incompatible inputs, duplicate IDs and unpinned capabilities; bundle tests resolve exact capability/subworkflow/policy contracts before start |
| Defined condition and join behavior | `workflow-validator` missing/type/multiple/no-match and ordered guard tests; kernel decision errors, first match, all/any failure, skip, await, cancel/reconcile, unsafe cancellation-region and unavailable-output tests |
| Same local/remote errors and immutable versions | `tls_definition_diagnostic_matrix_authorization_and_immutable_binding_contract` compares complete reports for 18 cases over actual HTTPS/PostgreSQL, checks publication conflicts before first start and concurrent publication; `https-cli.py` compares actual CLI output bytes and exits |
| Schema, semantics, three development examples and checker | `schemas/workflow-v1.schema.json`, [IR semantics](definition-semantics.md), [kernel semantics](kernel-semantics.md), review/parallel/bounded-repair examples and `workflow validate`; generated-schema equality runs through Cargo and Bazel |

These are definition and control-flow acceptance checks. Persistent waits,
deadlines, event CAS and checkpoint recovery are tested in the kernel and RunStore;
the existing multi-process HTTPS fixture also compares the real builtin workflow
against local execution after scheduler loss. External effects, sandboxing, live
model providers and the remaining cluster service requirements retain their own
issues and acceptance criteria.

## Local and remote validation

```sh
workflow validate examples/review.yaml
workflow remote validate author-client.json examples/review.yaml
```

The remote command reads the local file and sends its bytes, format and diagnostic
label to `validate_definition`. The server never opens that label as a path.
Definition-maintainer and runner credentials may validate; validation does not
publish a bundle, create a run or authorize execution. Audit records use the fixed
resource `definition`, without source text, file labels or diagnostics.

Both commands emit `{valid,digest,diagnostics}` with identical formatting. Exit 0
means valid, exit 1 means an invalid definition, and exit 2 means usage, file,
authentication or transport failure. A transport failure emits a fixed message on
stderr and no report. It must not be counted as either successful validation or a
detected definition error. Successful static checking still does not resolve a
complete execution bundle or prove arbitrary business termination.

Source text is limited to 1 MiB, diagnostic labels to 1024 UTF-8 bytes, and reports
to 256 diagnostics and 1 MiB of compact JSON. The final count slot is a
`diagnostic_limit` marker. Oversized encoded reports are replaced by one such
diagnostic. Either limit rejects the definition and omits its digest. CLI pretty
printing can add whitespace beyond the compact report bound. CLI inputs must be
regular UTF-8 files; oversized byte input is reported before UTF-8 decoding.

HTTP separately limits the complete request envelope to 2 MiB. JSON escaping can
make a source within the compiler limit exceed this wire limit; that request is a
transport/admission failure, not a compiler report. The integration test covers
this distinction and encoded diagnostic expansion. No parity claim applies to a
request that cannot be delivered or authenticated.

Remote publication recompiles the complete bundle and freezes all workflow,
capability and gate/model/effect-policy identities in the same transaction as the
publication allowlist. Matching content is idempotent. Changed content under an
existing ID/version returns `binding_conflict`, including before any run starts.
All keys use a shared sorted lock order; rejected batches roll back partial
bindings. Starts recheck those identities. Existing installations need no schema
change: pre-existing run bindings remain authoritative; earlier digest-only
publications acquire their version bindings when republished or first started.

Revisioned draft editing and history remain in the local DefinitionRegistry.
The remote API provides validation, immutable bundle publication and execution;
it does not expose shared draft CRUD, withdrawal or an authoring UI. A draft
revision conflict returns `revision_conflict`; immutable publication/run binding
conflicts return `binding_conflict`.

## Reproduce the measurements

From the repository root, with Python 3 and a built CLI:

```sh
cargo build -p workflow-cli --locked
python3 examples/validation/baseline.py target/debug/workflow > baseline.json
```

The deterministic baseline reports the binary SHA-256, Git head/dirty state,
environment, raw command outputs and timing samples. Its default 10 repetitions
each create a fresh registry, publish v1, apply the committed two-field patch,
check its exact diff and stale-edit rejection, publish v2 and reread unchanged v1.
Timing includes CLI startup, parsing and SQLite commit; it excludes compilation,
setup and human authoring. Min and nearest-rank p95 are observations without an
acceptance threshold or claimed productivity improvement.

The five seeded invalid graphs give the definition-time detected/invalid counts
and false accepts. This fixed corpus is a regression baseline, not a production
defect-detection rate. Five replay scenarios declare 29 required node-instance
steps, separately assert expected skips, and report omitted/mismatched required
steps. Task command counts and cancellation/reconciliation are checked as well.
Events contain supplied host facts: zero omitted replay steps does not establish
zero omitted real-world work or independent provider quality.

For remote checks, use a disposable PostgreSQL database in
`WORKFLOW_TEST_POSTGRES` and OpenSSL on PATH:

```sh
cargo test -p workflow-service --locked -- --ignored --nocapture
python3 examples/validation/https-cli.py target/debug/workflow > https-cli.json
```

The Rust matrix prints 18 cases, 16 invalid definitions, detected count, false
accepts, parity mismatches and elapsed time. Its single repetition is a correctness
baseline, not throughput. The CLI fixture starts a separate API process with
temporary certificates/credentials, compares JSON/YAML valid and invalid reports
for both allowed roles, and checks unauthorized, missing-file and stopped-server
failures. It creates a unique tenant in the disposable database. Mandatory CI runs
the Rust matrix and both Python scripts; retain raw output with the checked commit.
