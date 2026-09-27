#!/usr/bin/env python3
"""Run real validation, settle a typed report, then enforce two frozen postconditions.
Usage: python3 examples/gates/drive-guarded.py /absolute/workflow /new/output
The source revision identifies the committed inspected definition, not a workspace audit.
"""
import json
from pathlib import Path
import subprocess
import sys

repo = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
output = Path(sys.argv[2]).resolve()
output.mkdir()


def call(*args):
    process = subprocess.run([binary, *map(str, args)], capture_output=True, text=True, cwd=output)
    if process.returncode:
        raise RuntimeError(f"workflow {args}: {process.stdout}\n{process.stderr}")
    return json.loads(process.stdout)


def save(name, value):
    path = output / name
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    return path


revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
inspected = json.loads(subprocess.check_output(
    ["git", "show", f"{revision}:examples/execution/valid-start.json"], cwd=repo, text=True))
start = json.loads((repo / "examples/gates/guarded-start.json").read_text(encoding="utf-8"))
start["inputs"] = inspected["inputs"]
source = {"repository": "coolplayagent/workflow-cli", "revision": revision}
for gate in start["bundle"]["postconditions"]:
    gate["repository"]["value"] = source["repository"]
    gate["revision"]["value"] = revision
start_path = save("start.json", start)
call("kernel", "check", save("bundle.json", start["bundle"]))
database, artifacts = output / "runs.db", output / "artifacts"
call("run", "init", database)
call("artifact", "init", artifacts)
call("run", "start", database, start_path)
lease_request = save("lease-request.json", {
    "run_id": start["run_id"], "owner": "guarded-example",
    "acquisition_id": "example-1", "ttl_ms": 300000,
})
lease = save("lease.json", call("run", "acquire", database, lease_request)["result"])
attempt = call("run", "claim", database, lease)["result"]["attempt"]
request = save("request.json", attempt["request"])
grant = save("grant.json", attempt["grant"])
result = call("worker", "dispatch", request, grant)
assert result["outcome"]["outputs"]["valid"] is True
payload = save("report.json", {"valid": result["outcome"]["outputs"]["valid"]})
type_file = save("report-type.json", start["bundle"]["postconditions"][0]["policy"]["requirements"][0]["report_type"])
spec = save("publish.json", call("artifact", "prepare", request, type_file,
                                 save("source.json", source), save("input-refs.json", []))["result"])
reference = call("artifact", "put", artifacts, spec, payload)["result"]
save("reference.json", reference)
result["outcome"]["evidence"] = [{"artifact_id": reference["artifact_id"], "digest": reference["digest"]}]
settled = call("run", "--artifacts", artifacts, "finish", database, lease,
               attempt["attempt_id"], save("result.json", result))["result"]
assert settled["snapshot"]["status"] == "running"
assert settled["snapshot"]["frames"]["1"]["nodes"]["inspect"]["state"]["state"] == "checking_gate"
assert settled["snapshot"]["frames"]["1"]["nodes"]["route"]["state"]["state"] == "pending"
save("settled-before-gates.json", settled)
call("run", "--artifacts", artifacts, "release", database, lease)
# Each command budget is one: neither task observation nor its PASS completes the run.
first = call("run", "--artifacts", artifacts, "drive", database, start["run_id"], "gate-driver", "1")["result"]
assert first["executed_tasks"] == 0 and first["processed_commands"] == 1
assert first["snapshot"]["status"] == "running"
assert first["snapshot"]["frames"]["1"]["nodes"]["accepted"]["state"]["state"] == "checking_gate"
save("task-gate.json", first)
last = call("run", "--artifacts", artifacts, "drive", database, start["run_id"], "gate-driver", "1")["result"]
assert last["executed_tasks"] == 0 and last["processed_commands"] == 1
assert last["snapshot"]["status"] == "succeeded"
for node in ["inspect", "accepted"]:
    assert last["snapshot"]["frames"]["1"]["nodes"][node]["gate_decision"]["verdict"] == "PASS"
save("terminal-gate.json", last)
history = call("run", "--artifacts", artifacts, "history", database, start["run_id"], "0", "100")["result"]
assert sum(e["event"]["kind"]["type"] == "gate_evaluated" for e in history["items"]) == 2
save("history.json", history)
save("verified.json", call("run", "--artifacts", artifacts, "verify", database, start["run_id"]))
summary = {"database": str(database), "artifact_store": str(artifacts), "run_id": start["run_id"],
           "source_revision": revision, "after_task": settled["snapshot"]["status"],
           "after_task_gate": first["snapshot"]["status"], "after_terminal_gate": last["snapshot"]["status"],
           "gate_decisions": 2, "repeated_tasks": first["executed_tasks"] + last["executed_tasks"]}
save("summary.json", summary)
print(json.dumps(summary, indent=2))
