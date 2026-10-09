# Long-running agent execution, v0.2.0

The requested delivery closes four P1 findings from the 2026-10-09 review of
`06e4ce7`. Completion includes a reviewed pull request, a tested main commit,
a versioned release archive and the matching GitHub Pages documentation.

## Acceptance contract

1. **History and recovery:** ordinary recovery must not replay every checkpoint
   prefix. Provide verified state checkpoints, bounded incremental recovery and
   explicit full-history verification. Retain corruption, deduplication, gate and
   effect proofs. Expose approaching history limits and a fenced, traceable
   continuation into a successor run. Measure release-build history scaling and
   exercise old-store upgrade, backup and shared-image compatibility.
2. **Long activities:** separate the activity deadline from the ownership lease.
   Renew ownership while work runs, persist observable progress, fence stale
   completion, and stop cancellable child processes on cancellation or lost
   ownership. The daemon must keep admitting due work while a different run has
   a slow activity. Test execution beyond an initial lease, takeover, cancellation,
   draining and timer responsiveness; do not claim arbitrary Rust code is killable.
3. **Provider recovery:** distinguish temporary HTTP/network failures, rate limits,
   authentication failures, refusal and malformed output. Freeze bounded retry
   policy with the model policy; persist retry eligibility, attempts and admitted
   calls across restart. Respect Retry-After within policy/deadline bounds. A
   temporary provider outage must not permanently fail an otherwise recoverable
   workflow. Preserve legacy policy digests and behavior for existing runs.
4. **Agent continuation:** checkpoint model/tool observations before subsequent
   calls and resume verified progress without repeating acknowledged tools.
   Bind checkpoints to exact task inputs, policy, provider binding and ownership;
   account for uncertain in-flight calls without inventing observations. Provide
   typed feedback between loop iterations and a versioned handoff containing the
   objective, plan, verified facts, artifact references, failures and remaining
   work. Reject stale, foreign or tampered continuation state. Exercise process
   death and recovery, and prove a later repair consumes the earlier diagnostic.

The changes must preserve existing approvals, postconditions, external effect
reconciliation, immutable identities and explicit storage upgrades. Compatibility
tests and documentation must describe any new protocol or storage boundary.
Increasing a timeout or limit alone does not satisfy the corresponding item.

## Executable delivery process

`examples/maintenance/long-running-p1.json` records verified milestones as trusted
host event waits. It does not pretend to execute a code editor, reviewer, GitHub
API or provider. Each accepted signal must contain the actual source revision and
the digest/location of retained validation evidence. Only the actual checks and
remote observations authorize milestone advancement; workflow success alone is
not independent proof of those checks.

The stages are history, activities, provider recovery, agent continuation, full
quality gate, PR review/CI, merged-main CI, release verification and Pages
verification. Failed verification remains pending while fixes are made. Rejection
or deadline exhaustion terminates the delivery as failed. Runtime state and raw
logs live outside the source checkout.

## Required delivery checks

- Targeted regressions for all four acceptance items, including failure paths.
- Cargo format, Clippy, workspace tests and Bazel tests under existing policy.
- Unfiltered Qualitygate full check bound to the delivered snapshot.
- Documentation/resource/schema consistency and extracted-skill verification.
- Required GitHub CI, including real PostgreSQL/TLS/object-store acceptance.
- PR feedback resolved on the remote head; main CI successful at the release SHA.
- Release asset checksums, installed CLI version and deployed Pages version.

Online model quality, production capacity and monetary accuracy when a provider
omits usage remain outside this change. Tests use explicit deterministic provider
fixtures and report that boundary.
