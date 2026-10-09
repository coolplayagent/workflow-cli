#!/usr/bin/env python3
"""Allocate a real attempt workspace, run validation, capture typed evidence and pass two gates.
Usage: python3 examples/workspaces/validate-isolated.py /absolute/workflow /new/output
Creates an isolated fixture repository; never edits the caller's repository.
"""
import json
from pathlib import Path
import shutil
import subprocess
import sys

repo = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
output = Path(sys.argv[2]).resolve()
output.mkdir()


def save(name, value):
    path = output / name
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    return path


def call(*args, expected=0):
    p = subprocess.run([binary, *map(str, args)], capture_output=True, text=True, cwd=output)
    if p.returncode != expected:
        raise RuntimeError(f"workflow {args}: {p.stdout}\n{p.stderr}")
    return json.loads(p.stdout)["result"]


source_repo = output / "source"
source_repo.mkdir()
input_commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
pinned = json.loads(subprocess.check_output(
    ["git", "show", f"{input_commit}:examples/execution/valid-start.json"], cwd=repo, text=True))
(source_repo / "definition.json").write_text(pinned["inputs"]["document"], encoding="utf-8")


def git(*args):
    return subprocess.check_output([
        "git", "-c", "user.name=Workspace Example", "-c", "user.email=workspace@example.invalid",
        "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args,
    ], cwd=source_repo, text=True).strip()


git("init", "--initial-branch=main")
git("add", "definition.json")
git("commit", "-m", "Pinned inspection input")
revision = git("rev-parse", "HEAD")
source = {"repository": "isolated-example-source", "revision": revision}
start = json.loads((repo / "examples/gates/guarded-start.json").read_text(encoding="utf-8"))
start["run_id"] = "workspace-validation"
# Inputs are fixed from the exact committed object, not the editable source checkout.
start["inputs"]["format"] = "json"
for g in start["bundle"]["postconditions"]:
    g["repository"]["value"], g["revision"]["value"] = source["repository"], revision
# Preserve exact file bytes (including whitespace) for the worker's input digest.
start["inputs"]["document"] = subprocess.check_output(
    ["git", "show", f"{revision}:definition.json"], cwd=source_repo).decode("utf-8")
db, artifacts, workspaces = output / "runs.db", output / "artifacts", output / "workspaces"
call("run", "init", db)
call("artifact", "init", artifacts)
call("workspace", "init", workspaces)
call("run", "start", db, save("start.json", start))
lease = save("lease.json", call("run", "acquire", db, save("lease-request.json", {
    "run_id": start["run_id"], "owner": "workspace-example", "acquisition_id": "workspace-1", "ttl_ms": 300000,
})))
attempt = call("run", "claim", db, lease)["attempt"]
request = save("request.json", attempt["request"])
grant = save("grant.json", attempt["grant"])
ty = start["bundle"]["postconditions"][0]["policy"]["requirements"][0]["report_type"]
outputs = save("outputs.json", [{"path": "validation-report.json", "artifact_type": ty}])
spec = save("workspace-spec.json", call("workspace", "prepare", request, save("source.json", source),
                                       save("input-refs.json", []), outputs))
reference = call("workspace", "checkout", workspaces, source["repository"], source_repo, spec)
save("workspace-reference.json", reference)
workspace_id = reference["workspace_id"]
path = Path(call("workspace", "path", workspaces, workspace_id)["path"])
assert call("workspace", "verify-clean", workspaces, workspace_id)["clean"] is True
assert (path / "definition.json").read_text(encoding="utf-8") == attempt["request"]["inputs"]["document"]
# This actual builtin checks the immutable inline bytes proven equal to the allocated file.
p = subprocess.run([binary, "worker", "dispatch", str(request), str(grant)], capture_output=True, text=True, check=True)
result = json.loads(p.stdout)
assert result["outcome"]["outputs"]["valid"] is True
(path / "validation-report.json").write_text(json.dumps({"valid": result["outcome"]["outputs"]["valid"]}), encoding="utf-8")
capture = call("workspace", "capture", workspaces, workspace_id, artifacts)
save("capture.json", capture)
# Strict cleanliness includes generated outputs. This snapshot intentionally contains a new report.
assert capture["observation"]["clean"] is False
assert call("workspace", "verify-clean", workspaces, workspace_id, expected=1)["clean"] is False
result["outcome"]["evidence"] = [{"artifact_id": r["artifact_id"], "digest": r["digest"]}
                                  for r in [*capture["files"], capture["manifest"]]]
settled = call("run", "--artifacts", artifacts, "finish", db, lease, attempt["attempt_id"], save("result.json", result))
assert settled["snapshot"]["status"] == "running"
call("run", "--artifacts", artifacts, "release", db, lease)
driven = call("run", "--artifacts", artifacts, "drive", db, start["run_id"], "workspace-gates", "10")
assert driven["executed_tasks"] == 0 and driven["snapshot"]["status"] == "succeeded"
save("run-after-gates.json", driven)
save("verified-run.json", call("run", "--artifacts", artifacts, "verify", db, start["run_id"]))
# A later file change is visible and cannot alter the already retained capture.
(path / "definition.json").write_text("changed after completed run\n", encoding="utf-8")
changed = call("workspace", "observe", workspaces, workspace_id)
assert changed["tree_digest"] != capture["observation"]["tree_digest"]
save("later-observation.json", changed)
relocated = output / "relocated-workspaces"
shutil.move(workspaces, relocated)
assert call("workspace", "show", relocated, workspace_id) == reference
assert call("workspace", "observe", relocated, workspace_id) == changed
summary = {"run_id": start["run_id"], "database": str(db), "artifact_store": str(artifacts),
           "workspace_store": str(relocated), "workspace_id": workspace_id,
           "source_revision": revision, "input_fixture_revision": input_commit,
           "run_status": driven["snapshot"]["status"], "repeated_tasks": driven["executed_tasks"],
           "later_change_detected": True, "portable_reference_preserved": True}
save("summary.json", summary)
print(json.dumps(summary, indent=2))
