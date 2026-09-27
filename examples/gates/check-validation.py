#!/usr/bin/env python3
"""Check actual settled validation evidence; requires a new output directory.
Usage: python3 examples/gates/check-validation.py /absolute/workflow /new/output
"""
import json
from pathlib import Path
import subprocess
import sys

repo = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
output = Path(sys.argv[2]).resolve()
subprocess.run([sys.executable, str(repo / "examples/artifacts/record-validation.py"), binary,
                str(output)], check=True, stdout=subprocess.DEVNULL)


def load(name):
    return json.loads((output / name).read_text(encoding="utf-8"))


def save(name, value):
    path = output / name
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    return path


def call(*args, expected=0):
    process = subprocess.run([binary, *map(str, args)], capture_output=True, text=True, cwd=output)
    if process.returncode != expected:
        raise RuntimeError(f"workflow {args}: {process.stdout}\n{process.stderr}")
    return json.loads(process.stdout)


summary = load("summary.json")
request = load("request.json")
reference = summary["reference"]
policy = {
    "schema_version": 1,
    "identity": {"id": "definition.quality", "version": "1.0.0"},
    "requirements": [{
        "id": "valid-definition", "node_id": request["scope"]["node_id"],
        "capability": request["capability"], "contract_digest": request["contract_digest"],
        "report_type": reference["manifest"]["spec"]["artifact_type"],
        "pass_field": "valid", "max_age_ms": 60000,
    }],
}
# This example is the policy owner; normal callers must use the host-approved policy.
target = {
    "run_id": summary["run_id"],
    "run_digest": summary["committed"]["result"]["snapshot"]["run_digest"],
    "action": {"id": "definition.publish", "version": "1.0.0"},
    "source_revision": reference["manifest"]["spec"]["source_revision"],
    "input_digest": request["input_digest"], "artifacts": [],
}
gate = {"policy": policy, "target": target, "evidence": [{
    "requirement_id": "valid-definition",
    "report": {"artifact_id": reference["artifact_id"], "digest": reference["digest"]},
}]}
gate_path = save("gate-request.json", gate)
database, artifacts = summary["database"], summary["artifact_store"]
passed = call("gate", "evaluate", database, artifacts, gate_path)["result"]
assert passed["verdict"] == "PASS"
decision = save("gate-decision.json", passed)
fresh = call("gate", "revalidate", database, artifacts, gate_path, decision)["result"]
assert fresh["verdict"] == "PASS"
save("revalidated-decision.json", fresh)
# A new revision without new evidence must fail even though the old run succeeded.
gate["target"]["source_revision"]["revision"] = "0" * 40
changed = save("changed-target.json", gate)
unknown = call("gate", "evaluate", database, artifacts, changed, expected=1)["result"]
assert unknown["verdict"] == "UNKNOWN"
assert unknown["checks"][0]["reason"] == "target_mismatch"
save("changed-target-decision.json", unknown)
rejected = call("gate", "revalidate", database, artifacts, changed, decision, expected=1)
assert rejected["ok"] is False
save("rejected-old-decision.json", rejected)
print(json.dumps({"output": str(output), "original": passed["verdict"],
                  "changed_target": unknown["verdict"], "old_decision_rejected": True}, indent=2))
