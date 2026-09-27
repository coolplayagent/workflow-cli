# Attempt workspaces and captured outputs

`workflow-workspaces` defines portable allocation, observation and output contracts.
`workflow-workspace-local` implements a Linux adapter using Git object reads,
independent files and an immutable SQLite allocation catalog. Both have explicit
Bazel `rust_library` targets. Kernel, worker and artifact contracts remain separate.

The allocation is bound to one run/node-instance/attempt, its exact worker request
and input digests, a fixed source commit, input artifact links, declared typed
outputs and an explicit merge policy. It is a host-managed building block for R07.
The run executor does not automatically allocate workspaces or authenticate these
host claims in this increment.

## Run the real example

```sh
cargo build --locked --bin workflow
python3 examples/workspaces/validate-isolated.py "$PWD/target/debug/workflow" /tmp/new-workspace-demo
```

Use a new directory. The example creates its own Git repository from a committed
input fixture, starts a gated run and claims its real worker request. It allocates
a workspace and proves its definition file equals that request's immutable inline
input. It invokes the actual validation capability, writes its typed report in
the workspace, captures it, then settles the report and output manifest under the
run lease. Both existing postconditions pass without invoking the worker again.

The example then changes a file and observes a different tree digest. It also
moves the workspace store and checks that the portable reference and observation
remain identical. The caller's repository is only read. Saved requests,
references, captures, observations and run verification expose the full flow.

## CLI and source binding

```sh
workflow workspace init /path/to/workspaces
workflow workspace prepare request.json source.json input-refs.json outputs.json
workflow workspace checkout /path/to/workspaces repository-id /path/to/repository spec.json
workflow workspace show /path/to/workspaces <workspace-id>
workflow workspace path /path/to/workspaces <workspace-id>
workflow workspace observe /path/to/workspaces <workspace-id>
workflow workspace verify-clean /path/to/workspaces <workspace-id>
workflow workspace capture /path/to/workspaces <workspace-id> /path/to/artifacts
workflow workspace cleanup-orphans /path/to/workspaces
workflow schema workspace-checkout
workflow schema workspace-ref
workflow schema workspace-observation
workflow schema workspace-output
```

Inputs are raw JSON; save `prepare`'s `result` as the checkout spec. `source.json`
has `repository` and a full lowercase Git commit `revision`. `input-refs.json` is
an array of exact artifact links. `outputs.json` declares exact paths and complete
artifact types, for example:

```json
[{"path":"report.txt","artifact_type":{"identity":{"id":"example.report","version":"1.0.0"},"content":{"format":"utf8"}}}]
```

`prepare` binds the supplied workflow request. The host must check that it is the
actual prepared attempt under the current lease before dispatching work. The CLI
repository ID/path pair is an explicit host mapping; the adapter cannot prove
ownership of a repository name or authenticate a remote source. Input artifact
identities are frozen in the spec; `capture` verifies them through the artifact
store before publishing dependent outputs. Checkout currently materializes Git
files, not input artifact downloads.

The Git adapter reads commit, tree and blob objects using
[`git cat-file --batch`](https://git-scm.com/docs/git-cat-file). It verifies their
object hashes and walks the committed tree itself. Full SHA-1 and SHA-256 commit
IDs are supported; abbreviations, tags and noncommit objects are rejected. It does
not use the source checkout's dirty files or index. Replacement objects, lazy
network fetching and Git protocols are disabled. No shell command is assembled
from file names; checkout hooks, smudge/text conversion and attribute filters are
not executed. The Git binary and local object database mapping are host tools.

Only regular Git files and their executable bit are supported. Symlinks,
submodules, non-UTF8 paths, traversal, `.git` entries, control characters and
unsupported portable path shapes reject the entire allocation. These limitations
are explicit errors, not omitted files. The workspace is an export of committed
files and has no `.git` directory. Tools requiring Git metadata need a later
workspace adapter or an explicit host binding.

## Identity and parallel work

The workspace ID derives from run ID, node instance and attempt ID. Its manifest
digest additionally binds the request/input digests, source revision, input
artifacts, output declarations, baseline files, Git tree and observed environment
(OS, architecture, Git version). File entries contain path, byte count, SHA-256
content digest and executable bit. `workspace://<id>` contains no native path.
`workspace path` resolves the host directory separately.

Each allocation contains independently created files. Two attempts do not share
writable inodes, a source worktree or an index. Reusing the same attempt with a
different contract is a conflict. An exact retry returns the original allocation
and preserves any edits, including after a lost reply. It never resets a working
directory. The current adapter records `merge_policy: explicit`: edits remain
proposals until a separate authorized merge produces a new revision and new
evidence. There is no automatic writeback to the source repository.

Directory separation is not an OS process sandbox. A host running arbitrary
commands must separately restrict their filesystem, network, credentials and
shared resources. Writable artifact/catalog directories must remain under host
control. This adapter does not enforce resource locks outside the workspace or
prove the environment of a later external command.

## Observation and capture

`observe` reads every regular file using descriptor-relative, no-follow operations;
it includes ignored/untracked files and checks executable changes. It rejects
symlinks, special files and shared hard links. Two scans must agree, and each file
must retain its metadata through its read. The output gives the full file list,
tree digest, added/modified/deleted paths and whether each path is a declared
output. It is deterministic content data without an authority token or timestamp.

`clean` means that the entire observed file tree equals the allocated baseline.
Generated reports are therefore changes too. `observe` exits 0 for a completed
inspection, even when dirty; `verify-clean` exits 1 when dirty. Neither command
advances a run, exempts output paths from source checking, or authorizes a write.
A host must bind an observation to its own current time and intended action when
using it in a decision.

`capture` reads only the declared deliverable paths, checks their exact types and
publishes them as retained artifacts. File artifacts retain the original input
artifact dependencies. The output manifest includes the workspace ID/digest,
baseline and observed tree digests, cleanliness and each path/executable bit with
its exact artifact link. Its direct lineage includes the input artifacts and
captured files. The source revision remains the allocated base commit; changed
files do not invent a new Git commit.

After reading outputs, capture rescans the workspace. A changed file tree, missing
output, wrong type, invalid input dependency or storage error produces no successful
capture response. A failure may leave already published file artifacts, which
remain retained; the final output manifest is published only after validation and
the response budget check. The immutable capture stays unchanged when the working
files later change. Submit the actual file/manifest references with the worker
result before fenced settlement. Publishing after settlement cannot append
attachments to that accepted result or make new evidence eligible retroactively.

Filesystem observation is not an atomic compare-and-swap with an external action.
The host must serialize cooperating writers during a capture. A hostile or
uncoordinated process can change files after a read, and equal scans do not prove
that no intermediate change occurred. The next R03 integration must bind actual
execution and gate consumption to current observations; the current gate executor
still uses its frozen target and accepted evidence, without automatic workspace
inspection.

## Durability, retention and budgets

Publication creates a private staging directory, writes and syncs each file,
checks the baseline, then renames the tree without replacing an existing path.
Only afterward does one SQLite transaction commit its immutable manifest and
catalog integrity head. No allocation is returned before commit. A crash leaves
an unregistered staging/published tree or one complete allocation. Reopening
validates all catalog entries and their chain; it does not treat a directory alone
as a successful allocation. The integrity head and entries are read from one
SQLite snapshot, so a concurrent allocation cannot mix catalog generations.

`cleanup-orphans` holds the same catalog write lock as allocation and capture.
It removes only owned staging directories and trees absent from the committed
catalog. Committed workspaces and edits are retained. Invalid directory entries
or a corrupt catalog stop cleanup. It is not a retention deletion API.

| Budget | Limit |
| --- | --- |
| Spec/reference/CLI JSON | 2 MiB |
| Files / directories | 4,096 each, including the root directory |
| One file / full file tree | 64 MiB / 256 MiB |
| Relative path | 1,024 UTF-8 bytes, 32 components, 255 bytes per component |
| Input artifact links / declared output files | 64 / 64 |
| Allocations / catalog documents | 1,000 / 64 MiB |
| Git commit / tree metadata | 1 MiB / 4 MiB |
| One Git command / stderr | 60 seconds / 16 KiB |

The local adapter requires Linux, `/proc`, Git and a filesystem supporting the
used descriptor, sync and rename operations. Workspace catalog schema 1 is
separate from run storage schema 4 and artifact catalog schema 1. Opening a missing
store does not initialize it; foreign schemas/directories are refused. Failure is
reported without claiming an allocation or capture.

Cargo/Bazel tests cover actual Git objects, dirty sources, replacement refs and
filters, corrupted object bytes, SHA-256 repositories, path/type/budget rejection,
isolation, idempotent retries, modification/deletion/mode detection, unsafe file
kinds, captured lineage and upstream corruption, SQLite capacity exhaustion,
independent process races, concurrent catalog snapshots, and termination
before/after publication and commit.
This establishes the tested deterministic and process-crash behavior, not power-loss
recovery, production throughput or business benefit. R07 remains open for automatic
executor binding, sandboxed command execution, shared-resource coordination,
explicit merge/revalidation nodes and remote workspace/storage authorization.
