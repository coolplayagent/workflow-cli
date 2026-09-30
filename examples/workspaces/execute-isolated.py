#!/usr/bin/env python3
"""Actual fenced task -> isolated checkout -> typed capture -> durable gates.
Creates only a disposable fixture repository. Optional output directory is new.
"""
import copy
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile


def main():
    binary = str(Path(sys.argv[1]).resolve())
    temporary = tempfile.TemporaryDirectory(prefix="workflow-workspace-execution-") if len(sys.argv) < 3 else None
    root = Path(temporary.name) if temporary else Path(sys.argv[2]).resolve()
    if not temporary:
        root.mkdir()
    repo = Path(__file__).resolve().parents[2]
    fixture = json.loads((repo / "examples/gates/guarded-start.json").read_text())

    def save(name, value):
        path = root / name
        path.write_text(json.dumps(value))
        return path

    def call(*args, expected=0):
        result = subprocess.run([binary, *map(str, args)], capture_output=True, text=True)
        assert result.returncode == expected, (args, result.stdout, result.stderr)
        return json.loads(result.stdout)

    source = root / "source"
    source.mkdir()
    (source / "definition.json").write_text(fixture["inputs"]["document"])

    def git(*args):
        return subprocess.check_output(["git", "-c", "user.name=Workspace Acceptance",
                                       "-c", "user.email=workspace@example.invalid", "-c", "commit.gpgsign=false",
                                       "-c", "core.hooksPath=/dev/null", *args], cwd=source, text=True).strip()

    git("init", "--initial-branch=main")
    git("add", ".")
    git("commit", "-m", "First immutable source")
    first_revision = git("rev-parse", "HEAD")
    artifacts, workspaces, db = root / "artifacts", root / "workspaces", root / "runs.db"
    call("artifact", "init", artifacts)
    call("workspace", "init", workspaces)
    call("run", "init", db)
    report_type = fixture["bundle"]["postconditions"][0]["policy"]["requirements"][0]["report_type"]

    def binding(revision, repository_path=source):
        return {"workspace_store": str(workspaces), "repository_path": str(repository_path),
                "source_revision": {"repository": "workspace-fixture", "revision": revision},
                "capabilities": [{"capability": {"id": "workflow.validate", "version": "1.0.0"},
                                  "inline_files": {"document": "definition.json"},
                                  "reports": [{"path": "validation.json", "artifact_type": report_type, "fields": ["valid"]}]}]}

    def start(name, revision, document):
        request = copy.deepcopy(fixture)
        request["run_id"] = name
        request["inputs"]["document"] = document
        for gate in request["bundle"]["postconditions"]:
            gate["repository"]["value"] = "workspace-fixture"
            gate["revision"]["value"] = revision
        call("run", "start", db, save(name + ".json", request))

    start("original", first_revision, fixture["inputs"]["document"])
    initial = call("run", "--artifacts", artifacts, "drive-workspaces", db, "original", "executor", 10,
                   save("first-binding.json", binding(first_revision)))["result"]
    assert initial["snapshot"]["status"] == "succeeded" and initial["executed_tasks"] == 1
    first_snapshot = call("run", "--artifacts", artifacts, "status", db, "original")["result"]
    # A new committed source has a different workflow ID and input digest.
    document = json.loads(fixture["inputs"]["document"])
    document["id"] = "revalidated-requirements"
    new_bytes = json.dumps(document, indent=2) + "\n"
    # Two real task attempts retain independent repair proposals. The fixture
    # host supplies the edits; the builtin validates the declared source inputs.
    repair_db, repair_store, repair_artifacts = root / "repairs.db", root / "repairs", root / "repair-artifacts"
    call("run", "init", repair_db)
    call("workspace", "init", repair_store)
    call("artifact", "init", repair_artifacts)
    repair = copy.deepcopy(fixture)
    repair["run_id"] = "parallel-repairs"
    repair["bundle"]["postconditions"] = []
    graph = json.loads((repo / "examples/parallel-tests.json").read_text())
    graph["id"] = "inspect-definition"
    graph["inputs"] = copy.deepcopy(fixture["bundle"]["workflows"][0]["inputs"])
    template = fixture["bundle"]["workflows"][0]["nodes"][0]
    for index, node in enumerate(graph["nodes"]):
        if node["kind"]["type"] == "task":
            replacement = copy.deepcopy(template)
            replacement["id"] = node["id"]
            graph["nodes"][index] = replacement
    merge_node = copy.deepcopy(template)
    merge_node["id"] = "merge"
    merge_node["bindings"]["document"] = {"source": "literal", "value": new_bytes}
    graph["nodes"].insert(-1, merge_node)
    graph["edges"][-1]["to"] = "merge"
    graph["edges"].append({"id": "merge-done", "from": "merge", "to": "done", "route": {"type": "next"}})
    repair["bundle"]["workflows"] = [graph]
    call("run", "start", repair_db, save("repair-start.json", repair))
    lease = save("repair-lease.json", call("run", "acquire", repair_db, save("repair-lease-request.json", {
        "run_id": "parallel-repairs", "owner": "fixture-editor", "acquisition_id": "repair-lease", "ttl_ms": 300000}))["result"])
    source_ref = save("merge-source.json", {"repository": "workspace-fixture", "revision": first_revision})
    outputs = save("repair-outputs.json", [{"path": "definition.json", "artifact_type": {
        "identity": {"id": "fixture.source", "version": "1.0.0"}, "content": {"format": "utf8"}}}])
    proposals, paths, attempts = [], [], []

    def dispatch(attempt):
        request = save("dispatch-request.json", attempt["request"])
        grant = save("dispatch-grant.json", attempt["grant"])
        return call("worker", "dispatch", request, grant)

    for index in range(2):
        attempt = call("run", "--artifacts", repair_artifacts, "claim", repair_db, lease)["result"]["attempt"]
        attempts.append(attempt["attempt_id"])
        request = save(f"repair-request-{index}.json", attempt["request"])
        spec = call("workspace", "prepare", request, source_ref, save("empty-inputs.json", []), outputs)["result"]
        reference = call("workspace", "checkout", repair_store, "workspace-fixture", source,
                         save(f"repair-spec-{index}.json", spec))["result"]
        path = Path(call("workspace", "path", repair_store, reference["workspace_id"])["result"]["path"])
        paths.append(path)
        assert (path / "definition.json").read_text() == attempt["request"]["inputs"]["document"]
        result = dispatch(attempt)
        proposal_doc = copy.deepcopy(document)
        proposal_doc["id"] = "alternative-repair" if index == 0 else document["id"]
        (path / "definition.json").write_text(json.dumps(proposal_doc, indent=2) + "\n")
        proposal = call("workspace", "seal", repair_store, reference["workspace_id"], repair_artifacts,
                        save(f"repair-summary-{index}.json", f"Fixture repair {index}: review independently"))["result"]
        proposals.append({"artifact_id": proposal["artifact_id"], "digest": proposal["digest"]})
        result["outcome"]["evidence"].append(proposals[-1])
        call("run", "--artifacts", repair_artifacts, "finish", repair_db, lease, attempt["attempt_id"],
             save(f"repair-result-{index}.json", result))
    assert len(set(attempts)) == len(set(paths)) == 2
    assert (paths[0] / "definition.json").read_bytes() != (paths[1] / "definition.json").read_bytes()
    proposals_file = save("proposals.json", sorted(proposals, key=lambda p: p["artifact_id"]))
    unresolved = call("workspace", "merge-plan", repair_artifacts, "workspace-fixture", source, source_ref,
                      proposals_file, save("no-resolutions.json", {}))["result"]
    assert [c["path"] for c in unresolved["conflicts"]] == ["definition.json"]
    merge_attempt = call("run", "--artifacts", repair_artifacts, "claim", repair_db, lease)["result"]["attempt"]
    merge_request = save("merge-request.json", merge_attempt["request"])
    summary = save("merge-summary.json", "Select the reviewed second repair and require new revision validation")
    call("workspace", "merge-apply", repair_artifacts, "workspace-fixture", source,
         save("conflicting-plan.json", unresolved), merge_request, summary, root / "refused.git", expected=1)
    assert not (root / "refused.git").exists()
    plan = call("workspace", "merge-plan", repair_artifacts, "workspace-fixture", source, source_ref,
                proposals_file, save("resolutions.json", {"definition.json": proposals[1]["artifact_id"]}))["result"]
    assert plan["requires_revalidation"] and not plan["conflicts"]
    # Mutable edits after sealing cannot become part of the selected proposal.
    (paths[1] / "definition.json").write_text("unreviewed later edit")
    merged_repo = root / "merged.git"
    merged = call("workspace", "merge-apply", repair_artifacts, "workspace-fixture", source,
                  save("merge-plan.json", plan), merge_request, summary, merged_repo)["result"]
    new_revision = merged["commit"]["source_revision"]["revision"]
    assert merged["commit"]["requires_revalidation"] and new_revision != first_revision
    actual = subprocess.check_output(["git", "-C", str(merged_repo), "show", f"{new_revision}:definition.json"]).decode()
    assert actual == new_bytes == merge_attempt["request"]["inputs"]["document"]
    result = dispatch(merge_attempt)
    result["outcome"]["evidence"].append({k: merged["artifact"][k] for k in ["artifact_id", "digest"]})
    call("run", "--artifacts", repair_artifacts, "finish", repair_db, lease, merge_attempt["attempt_id"],
         save("merge-result.json", result))
    assert call("run", "--artifacts", repair_artifacts, "status", repair_db, "parallel-repairs")["result"]["status"] == "succeeded"
    assert git("rev-parse", "HEAD") == first_revision
    assert (source / "definition.json").read_text() == fixture["inputs"]["document"]
    start("mismatched", new_revision, new_bytes)
    refused = call("run", "--artifacts", artifacts, "drive-workspaces", db, "mismatched", "executor", 10,
                   root / "first-binding.json", expected=1)
    assert "differs from committed workspace bytes" in refused["error"]["message"]
    start("revalidated", new_revision, new_bytes)
    fresh = call("run", "--artifacts", artifacts, "drive-workspaces", db, "revalidated", "executor", 10,
                 save("new-binding.json", binding(new_revision, merged_repo)))["result"]
    assert fresh["snapshot"]["status"] == "succeeded" and fresh["executed_tasks"] == 1
    assert call("run", "--artifacts", artifacts, "status", db, "original")["result"] == first_snapshot
    rows = sqlite3.connect(workspaces / "catalog.sqlite").execute("SELECT document FROM workspaces").fetchall()
    references = [json.loads(row[0]) for row in rows]
    assert {r["manifest"]["spec"]["producer"]["run_id"] for r in references} == {"original", "mismatched", "revalidated"}
    assert len({r["workspace_id"] for r in references}) == 3
    for ref in references:
        assert ref["manifest"]["environment"]["tools"]["workflow-binary"] == "sha256:" + hashlib.sha256(Path(binary).read_bytes()).hexdigest()
        run = ref["manifest"]["spec"]["producer"]["run_id"]
        path = Path(call("workspace", "path", workspaces, ref["workspace_id"])["result"]["path"])
        if run == "mismatched":
            assert not (path / "validation.json").exists()
        else:
            assert json.loads((path / "validation.json").read_text()) == {"valid": True}
    print(json.dumps({"executed_tasks": 5, "isolated_attempts": 5, "conflicting_repairs": 2, "merged_revision_revalidated": True, "stale_source_rejected": True,
                      "first_revision": first_revision, "revalidated_revision": new_revision,
                      "original_history_preserved": True}, indent=2))
    if temporary:
        temporary.cleanup()


if __name__ == "__main__":
    main()
