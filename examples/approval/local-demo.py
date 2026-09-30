#!/usr/bin/env python3
"""Exercise R06 with isolated state and a new CLI process for every operation."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix="workflow-approval-") as directory:
        root = Path(directory)
        db = root / "runs.db"

        def call(*argv):
            process = subprocess.run([str(binary), *map(str, argv)], capture_output=True,
                                     text=True, check=True, timeout=30)
            report = json.loads(process.stdout)
            assert report["ok"], report
            return report["result"]

        def save(name, value):
            path = root / name
            path.write_text(json.dumps(value))
            return path

        call("run", "init", db)
        for decision, expected in [("approve", "succeeded"), ("reject", "cancelled"),
                                   ("request_changes", "cancelled")]:
            request = json.loads(Path(__file__).with_name("protected-start.json").read_text())
            request["run_id"] = "review-" + decision.replace("_", "-")
            request["started_at_unix_ms"] = int(time.time() * 1000)
            run_id = request["run_id"]
            call("run", "start", db, save("start.json", request))
            call("run", "drive", db, run_id, "operator", 100)
            wait = call("run", "waits", db, run_id, 0, 100)["items"][0]
            before = call("run", "status", db, run_id)
            assert wait["subjects"]["review_digest"] == request["inputs"]["review_digest"]
            assert wait["policy"]["kind"] == "human_approval"
            idle = call("run", "drive", db, run_id, "next-session", 100)
            assert idle["executed_tasks"] == 0
            assert call("run", "status", db, run_id) == before
            signal = dict(schema_version=1, run_id=run_id, run_digest=before["run_digest"],
                          message=dict(schema_version=1, message_id="response", source="reviewer",
                                       target=wait["target"], correlation_id=wait["correlation_id"],
                                       decision=decision, reason="Explicit fixture decision",
                                       outputs={}, expires_at_unix_ms=wait["deadline_unix_ms"]))
            forged = copy.deepcopy(signal)
            forged["message"].update(message_id="forged", source="model-text")
            denied = call("run", "receive", db, save("forged.json", forged))
            assert denied["entry"]["status"]["reason"] == "responder_not_allowed"
            response = save("response.json", signal)
            receipt = call("run", "receive", db, response)
            assert receipt["entry"]["status"]["status"] == "applied"
            retry = call("run", "receive", db, response)
            assert retry["duplicate"] and retry["entry"] == receipt["entry"]
            final = call("run", "status", db, run_id)
            assert final["status"] == expected
            call("run", "verify", db, run_id)
            results.append(dict(decision=decision, status=expected, duplicate_preserved=True,
                                idle_executed_tasks=idle["executed_tasks"],
                                subject=wait["subjects"]["review_digest"]))
    print(json.dumps(dict(status="pass", storage="sqlite", independent_cli_sessions=True,
                         binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                         cases=results), indent=2))


if __name__ == "__main__":
    main()
