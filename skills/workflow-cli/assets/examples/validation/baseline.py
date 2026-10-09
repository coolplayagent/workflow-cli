#!/usr/bin/env python3
"""Deterministic R01 CLI baseline; supplied replay events are simulation facts."""
import argparse
from collections import Counter
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--repetitions", type=int, default=10)
    args = parser.parse_args()
    if not 1 <= args.repetitions <= 1000:
        parser.error("repetitions must be 1..1000")
    binary = args.binary.resolve()
    records = []

    def run(*command, code=0, raw=False):
        started = time.perf_counter_ns()
        proc = subprocess.run([str(binary), *map(str, command)], cwd=ROOT,
                              capture_output=True, text=True, timeout=30)
        elapsed = (time.perf_counter_ns() - started) / 1_000_000
        records.append({"command": list(map(str, command)), "exit": proc.returncode,
                        "elapsed_ms": elapsed, "stdout": proc.stdout,
                        "stderr": proc.stderr})
        assert proc.returncode == code, records[-1]
        return (proc.stdout if raw else json.loads(proc.stdout)), elapsed

    report = {"schema_version": 1, "seed": "fixed-fixtures-v1",
              "repetitions": args.repetitions,
              "environment": {"platform": platform.platform(),
                              "machine": platform.machine(), "cpus": os.cpu_count(),
                              "python": platform.python_version()},
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "git_head": subprocess.check_output(
                  ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "git_dirty": bool(subprocess.check_output(
                  ["git", "status", "--porcelain"], cwd=ROOT)),
              "limits": ["CLI edit latency includes process startup and SQLite commit; excludes build and human authoring.",
                         "Replay steps are declared control-flow obligations with supplied host results, not real provider execution.",
                         "The seeded invalid corpus is not an estimate of production defect detection."]}
    with tempfile.TemporaryDirectory(prefix="workflow-r01-baseline-") as directory:
        temp = Path(directory)
        roundtrips = []
        for name in ["review", "parallel-tests", "bounded-repair", "repair-round"]:
            source = ROOT / "examples" / (name + ".json")
            original, _ = run("validate", source)
            canonical, _ = run("export", source, "json", raw=True)
            yaml, _ = run("export", source, "yaml", raw=True)
            path = temp / (name + ".yaml")
            path.write_text(yaml)
            received, _ = run("validate", path)
            exported, _ = run("export", path, "json", raw=True)
            assert received["digest"] == original["digest"]
            assert json.loads(canonical) == json.loads(exported)
            roundtrips.append({"fixture": name, "digest": original["digest"],
                               "semantic_and_id_parity": True})
        report["roundtrips"] = roundtrips
        base = json.loads((ROOT / "examples/review.json").read_text())
        defects = []
        for name, expected in [("unreachable", "unreachable_node"),
                               ("dangling", "dangling_edge"),
                               ("cycle", "implicit_cycle"),
                               ("loop", "unbounded_loop"),
                               ("input", "input_type")]:
            value = json.loads(json.dumps(base))
            if name == "unreachable":
                value["nodes"].append({"id": "orphan", "kind": {
                    "type": "terminal", "outcome": "failed"}})
            elif name == "dangling":
                value["edges"][0]["to"] = "missing"
            elif name == "cycle":
                value["edges"][0]["to"] = value["edges"][0]["from"]
            elif name == "loop":
                value["nodes"][0]["kind"] = {"type": "loop", "body": {
                    "id": "repair", "version": "1.0.0"}, "max_iterations": 0,
                    "deadline_ms": 0}
            else:
                value["nodes"][0]["inputs"] = {"count": {
                    "required": True, "value_type": {"type": "integer"}}}
                value["nodes"][0]["bindings"] = {"count": {
                    "source": "literal", "value": "three"}}
            path = temp / (name + ".json")
            path.write_text(json.dumps(value))
            result, _ = run("validate", path, code=1)
            assert not result["valid"] and result["digest"] is None
            assert expected in [d["code"] for d in result["diagnostics"]]
            defects.append({"case": name, "expected_code": expected, "detected": True})
        report["definition_errors"] = {"cases": defects, "invalid": len(defects),
                                       "detected_before_execution": len(defects),
                                       "false_accepts": 0}
        samples = []
        for repetition in range(args.repetitions):
            db = temp / f"author-{repetition}.sqlite"
            draft, _ = run("draft", "create", db, "review", "examples/review.yaml")
            assert draft["draft"]["revision"] == 1
            old, _ = run("draft", "publish", db, "review", 1)
            edited, elapsed = run("draft", "edit", db, "review",
                                  "examples/registry/review.patch.json")
            assert edited["draft"]["revision"] == 2
            samples.append(elapsed)
            diff, _ = run("draft", "diff", db, "review", 1, 2)
            changes = {c["path"]: c for c in diff["diff"]["changes"]}
            assert set(changes) == {"/version", "/nodes/review/kind/timeout_ms"}
            assert changes["/version"]["before"] == "1.0.0"
            assert changes["/version"]["after"] == "2.0.0"
            assert changes["/nodes/review/kind/timeout_ms"]["before"] == 86400000
            assert changes["/nodes/review/kind/timeout_ms"]["after"] == 43200000
            stale, _ = run("draft", "edit", db, "review",
                           "examples/registry/review.patch.json", code=1)
            assert stale["error"]["code"] == "revision_conflict"
            run("draft", "publish", db, "review", 2)
            unchanged, _ = run("release", "get", db, "requirements-review", "1.0.0")
            assert unchanged["publication"] == old["publication"]
        report["edit_latency_ms"] = {"samples": samples, "min": min(samples),
                                      "p95_nearest_rank": sorted(samples)[math.ceil(.95 * len(samples)) - 1]}

    # Independent expected paths/states; skipped branches are not required work.
    plans = {
        "review-approved": ("succeeded", [{"review": "succeeded", "implement": "succeeded", "done": "succeeded", "declined": "skipped", "expired": "skipped"}]),
        "review-rejected": ("cancelled", [{"review": "succeeded", "implement": "skipped", "done": "skipped", "declined": "cancelled", "expired": "skipped"}]),
        "parallel-all": ("succeeded", [{"split": "succeeded", "unit": "succeeded", "integration": "succeeded", "join": "succeeded", "done": "succeeded"}]),
        "parallel-any-cancel": ("succeeded", [{"split": "succeeded", "unit": "succeeded", "integration": "cancelled", "join": "succeeded", "done": "succeeded"}]),
        "repair-third-round": ("succeeded", [
            {"repair": "succeeded", "done": "succeeded", "exhausted": "skipped"},
            *[{"fix": "succeeded", "test": "succeeded", "choose": "succeeded", "retry": "failed", "done": "skipped"} for _ in range(2)],
            {"fix": "succeeded", "test": "succeeded", "choose": "succeeded", "retry": "skipped", "done": "succeeded"}])}
    scenarios = []
    for name, (status, expected_frames) in plans.items():
        result, _ = run("kernel", "replay", f"examples/kernel/{name}.json")
        snapshot = result["snapshot"]
        assert snapshot["status"] == status
        frames = [f for _, f in sorted(snapshot["frames"].items(), key=lambda p: int(p[0]))]
        assert len(frames) == len(expected_frames)
        required = omitted = 0
        instances = []
        for frame, expected in zip(frames, expected_frames):
            actual = {n: v["state"]["state"] for n, v in frame["nodes"].items()}
            required += sum(s != "skipped" for s in expected.values())
            omitted += sum(actual.get(n) != s for n, s in expected.items() if s != "skipped")
            assert actual == expected, (name, actual, expected)
            instances.extend(n["instance_id"] for n in frame["nodes"].values())
        assert len(instances) == len(set(instances))
        commands = [c for t in result["transitions"] for c in t["commands"]]
        executed = Counter(c["node_id"] for c in commands if c["type"] == "execute_task")
        expected_tasks = {"review-approved": {"implement": 1}, "review-rejected": {},
                          "parallel-all": {"unit": 1, "integration": 1},
                          "parallel-any-cancel": {"unit": 1, "integration": 1},
                          "repair-third-round": {"fix": 3, "test": 3}}[name]
        assert executed == expected_tasks
        if name == "parallel-any-cancel":
            loser = frames[0]["nodes"]["integration"]["instance_id"]
            assert [c["type"] for c in commands if c.get("instance_id") == loser] == ["execute_task", "cancel_task", "reconcile_task"]
        if name == "repair-third-round":
            assert [f["status"] for f in frames[1:]] == ["failed", "failed", "succeeded"]
        scenarios.append({"name": name, "status": status, "required_steps": required,
                          "omitted_steps": omitted, "task_commands": dict(executed)})
    report["scenarios"] = scenarios
    report["required_steps"] = sum(s["required_steps"] for s in scenarios)
    report["omitted_steps"] = sum(s["omitted_steps"] for s in scenarios)
    report["commands"] = records
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
