# R08 local acceptance

This matrix records the supported local Linux product boundary. It does not stand
in for R09 cluster tests, R10 live ownership handoff or R15 model value evaluation.

| R08 acceptance | Result checker / observed evidence |
| --- | --- |
| Fresh environment, no remote service, deterministic branch/loop/parallel/human wait | `examples/execution/offline-demo.py` checks two real builtin results, explicit operator choice, exact business terminal, replay and service stop. Passed approval/rejection on the host and approval in a fresh Ubuntu 24.04 container with `--network none`. |
| Kill process; recover without repeating committed tasks | `daemon_recovers_parallel_loop_and_branch_work_then_wakes_on_callback_and_timeout` kills/restarts the daemon at a persisted wait and asserts exactly two original Finished records, then checks callback/timeout terminals and replay. Existing RunStore tests cover kills inside commit boundaries. |
| Two CLI resumes cannot create two legitimate owners | `concurrent_cli_resume_and_acquire_have_only_one_cas_winner_and_owner` starts two independent CLI test processes for each race; exactly one resume CAS and one lease acquisition wins. |
| Daemon status/stop; no false timer service when absent | Live Unix IPC, generation-bound drain and private lock; actual kill and SIGSTOP tests distinguish stopped from unreachable. Old stop requests cannot affect restarted instances. |
| Moved backup preserves artifacts/history/waits; confirmed effects do not repeat | `workflow-backup-local` relocation and independent durable provider tests cover full artifact/gate/Inbox/registry data and post-snapshot effects with retained and missing original intent. One provider object and one write remain after query/import recovery. |
| Disk-full / missing backup data report errors without false success | `sqlite_disk_full_rolls_back_without_confirming_start_or_transition` forces SQLITE_FULL via page limits and verifies rollback. Backup tests force an actual child-process file quota failure and remove/corrupt required files. These are distinct injected failure models, not an assertion of arbitrary full-volume behavior. |
| Publish supported platform/filesystem and boundaries | Ubuntu 24.04.4, Linux 7.0.0-31-generic x86_64, ext4; separate Ubuntu 24.04 container, same host kernel/ext4 bind storage. Process kill, suspension, SQLite rollback and archive relocation are covered; arbitrary power loss and unbacked disk destruction are not. |

The tested binary uses Rust 1.97.1 and requires compatible glibc (this host build
requires GLIBC_2.39). The older Debian 12 image rejected that binary at startup;
it is not listed as supported for this artifact. The successful Ubuntu image
identity is `sha256:69f919497cada9a8611f3de61df902d9f94bf20f84552ef5bd93125c38608ef9`.
The runtime image was prepared before the network-disabled execution. No cloud
credentials, model call or remote database participated in that execution.

All stores remain explicit local paths. The daemon reloads durable state on each
scan and never removes active data. Retention is currently keep-all with explicit
size/admission limits; there is no automatic compression/deletion. Export uses the
verified archive format, carrying immutable definitions and effect identities.
Restore pauses/fences ownership and requires provider/source audit before writes;
exporting is not permission to run two active copies.

Raw gate reports, independent environment output, binary/image identities and
review/CI evidence are retained in `.qualitygate/mr-33/` and `.qualitygate/mr-34/`
(the former is in the backup worktree). The small example's elapsed measurement
starts after initialization and binary preparation; it is not installation time,
production throughput, end-to-end business RTO or a model-benefit estimate.
