#!/usr/bin/env python3
"""Run a real built-in worker, publish its typed report and commit verified evidence.
Usage: python3 examples/artifacts/record-validation.py /absolute/path/to/workflow /new/output/dir
The output directory must not exist. Source revision identifies the committed input definition.
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
    result = subprocess.run([binary, *map(str, args)], check=False, capture_output=True, text=True, cwd=output)
    if result.returncode:
        raise RuntimeError(f"workflow {args}: {result.stdout}\n{result.stderr}")
    return json.loads(result.stdout)


def save(name, value):
    path = output / name
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    return path


revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
# Pin the actual inspected input to that commit, even if the caller has local edits.
start = json.loads(subprocess.check_output(
    ["git", "show", f"{revision}:examples/execution/valid-start.json"], cwd=repo, text=True))
start["run_id"] = "artifact-validation"
start_path = save("start.json", start)
database = output / "runs.db"
artifacts = output / "artifacts"
call("run", "init", database)
call("artifact", "init", "artifacts")
call("run", "start", database, start_path)
lease_request = save("lease-request.json", {
    "run_id": start["run_id"], "owner": "artifact-example",
    "acquisition_id": "example-1", "ttl_ms": 300000,
})
lease = save("lease.json", call("run", "acquire", database, lease_request)["result"])
claimed = call("run", "claim", database, lease)["result"]["attempt"]
request = save("request.json", claimed["request"])
grant = save("grant.json", claimed["grant"])
result = call("worker", "dispatch", request, grant)
outputs = result["outcome"]["outputs"]
assert outputs["valid"] is True
payload = save("validation-report.json", {
    "valid": outputs["valid"], "definition_digest": outputs["digest"],
    "diagnostics_count": len(outputs["diagnostics"]), "request_digest": result["request_digest"],
})
source = save("source.json", {"repository": "coolplayagent/workflow-cli", "revision": revision})
inputs = save("input-refs.json", [])
type_file = repo / "examples/artifacts/validation-report-type.json"
spec = save("publish.json", call("artifact", "prepare", request, type_file, source, inputs)["result"])
reference = call("artifact", "put", artifacts, spec, payload)["result"]
ref_file = save("reference.json", reference)
call("artifact", "verify", artifacts, reference["artifact_id"], type_file)
result["outcome"]["evidence"] = [{"artifact_id": reference["artifact_id"], "digest": reference["digest"]}]
result_file = save("result.json", result)
committed = call("run", "--artifacts", artifacts, "finish", database, lease,
                 claimed["attempt_id"], result_file)
assert committed["result"]["snapshot"]["status"] == "succeeded"
call("run", "--artifacts", artifacts, "release", database, lease)
verified = call("run", "--artifacts", artifacts, "verify", database, start["run_id"])
# Export and import preserve the manifest/reference, with no machine-specific path.
exported = output / "exported-report.json"
call("artifact", "export", artifacts, reference["artifact_id"], exported)
relocated = output / "relocated-artifacts"
call("artifact", "init", relocated)
assert call("artifact", "import", relocated, ref_file, exported)["result"] == reference
call("run", "--artifacts", relocated, "verify", database, start["run_id"])
summary = {"database": str(database), "artifact_store": str(artifacts), "run_id": start["run_id"],
           "reference": reference, "committed": committed, "verified": verified,
           "relocated_store": str(relocated)}
save("summary.json", summary)
print(json.dumps(summary, indent=2))
