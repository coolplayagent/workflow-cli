#!/usr/bin/env python3
"""Real S3 contract and CLI migration of an accepted, gated run's evidence.
Requires WORKFLOW_TEST_POSTGRES pointing only at a disposable PostgreSQL server.
Pinned MinIO is an isolated interoperability fixture, not a deployment recommendation.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
import uuid

MINIO_VERSION = "RELEASE.2025-04-22T22-12-26Z"
MINIO_SHA256 = "53e2a2cb16c5366ea6fbbc479c19ddb4c6a0948273e752f740fb1fbf27bb817c"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("workflow")
    parser.add_argument("--minio")
    args = parser.parse_args()
    assert os.environ.get("WORKFLOW_TEST_POSTGRES"), "disposable PostgreSQL required"
    binary = str(Path(args.workflow).resolve())
    repo = Path(__file__).resolve().parents[2]
    with tempfile.TemporaryDirectory(prefix="workflow-object-contract-") as directory:
        root = Path(directory)
        minio = Path(args.minio).resolve() if args.minio else root / "minio"
        if not args.minio:
            url = f"https://github.com/minio/minio/releases/download/{MINIO_VERSION}/minio.linux-amd64.{MINIO_VERSION}"
            subprocess.run(["curl", "--fail", "--location", "--retry", "3", "--max-time", "120", "--output", str(minio), url], check=True)
        assert hashlib.sha256(minio.read_bytes()).hexdigest() == MINIO_SHA256, "fixture binary digest mismatch"
        minio.chmod(0o700)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        endpoint = f"http://127.0.0.1:{port}"
        env = dict(os.environ, MINIO_ROOT_USER="workflowfixture", MINIO_ROOT_PASSWORD="workflow-fixture-secret", MINIO_BROWSER="off", WORKFLOW_TEST_S3=endpoint)
        log = (root / "minio.log").open("w")
        server = subprocess.Popen([str(minio), "server", str(root / "data"), "--address", f"127.0.0.1:{port}"], env=env, stdout=log, stderr=log)
        try:
            for _ in range(100):
                assert server.poll() is None, (root / "minio.log").read_text()
                try:
                    with urllib.request.urlopen(endpoint + "/minio/health/ready", timeout=1):
                        break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError("MinIO did not become ready")
            subprocess.run(["curl", "--fail", "--silent", "--show-error", "--max-time", "10", "--aws-sigv4", "aws:amz:us-east-1:s3",
                            "--user", "workflowfixture:workflow-fixture-secret", "-X", "PUT", endpoint + "/workflow-artifacts"], check=True)
            subprocess.run(["cargo", "test", "-p", "workflow-artifact-s3", "--locked", "--", "--ignored", "--nocapture", "--test-threads=1"], cwd=repo, env=env, check=True)
            demo = root / "demo"
            subprocess.run(["python3", str(repo / "examples/workspaces/execute-isolated.py"), binary, str(demo)], check=True)

            def save(name, value):
                path = root / name
                path.write_text(json.dumps(value))
                return path

            def call(*args):
                result = subprocess.run([binary, *map(str, args)], capture_output=True, text=True, env=env)
                assert result.returncode == 0, (args, result.stdout, result.stderr)
                return json.loads(result.stdout)["result"]

            env.update(WORKFLOW_S3_FIXTURE_ACCESS="workflowfixture", WORKFLOW_S3_FIXTURE_SECRET="workflow-fixture-secret")
            binding = save("binding.json", {"namespace": f"cli-{uuid.uuid4().hex}", "database": {
                "connection": {"type": "environment", "name": "WORKFLOW_TEST_POSTGRES"}, "transport": {"type": "local"}},
                "object": {"endpoint": endpoint, "bucket": "workflow-artifacts", "prefix": "cli-fixtures", "region": "us-east-1", "allow_http_loopback": True},
                "access_key": {"type": "environment", "name": "WORKFLOW_S3_FIXTURE_ACCESS"},
                "secret_key": {"type": "environment", "name": "WORKFLOW_S3_FIXTURE_SECRET"}})
            call("artifact", "s3-init", binding)
            local = demo / "artifacts"
            records = [json.loads(row[0]) for row in sqlite3.connect(local / "catalog.sqlite").execute("SELECT document FROM artifacts ORDER BY sequence")]
            for reference in records:
                actual = call("artifact", "s3-import", binding, local, reference["artifact_id"])
                assert actual["artifact"] == reference
            for run in ["original", "revalidated"]:
                expected = call("run", "--artifacts", local, "status", demo / "runs.db", run)
                assert call("run", "--object-artifacts", binding, "status", demo / "runs.db", run) == expected
                call("run", "--object-artifacts", binding, "verify", demo / "runs.db", run)
            restored = root / "restored-artifacts"
            call("artifact", "init", restored)
            for reference in records:
                assert call("artifact", "s3-export", binding, reference["artifact_id"], restored)["artifact"] == reference
            ticket = root / "private-download.json"
            result = call("artifact", "s3-download-grant", binding, records[-1]["artifact_id"], 30, ticket)
            assert "url" not in result and ticket.stat().st_mode & 0o077 == 0
            grant = json.loads(ticket.read_text())
            with urllib.request.urlopen(grant["url"], timeout=10) as response:
                payload = response.read()
            assert "sha256:" + hashlib.sha256(payload).hexdigest() == records[-1]["manifest"]["content_digest"]
            print(json.dumps({"binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(), "minio_sha256": MINIO_SHA256,
                              "accepted_artifacts_roundtripped": len(records), "runs_verified_with_object_reader": 2, "private_scoped_download": True}, indent=2))
        finally:
            server.terminate()
            server.wait(timeout=15)
            log.close()


if __name__ == "__main__":
    main()
