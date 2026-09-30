#!/usr/bin/env python3
"""Real PostgreSQL dump/restore, HTTPS and post-backup effect reconciliation.

Requires WORKFLOW_TEST_POSTGRES pointing at the specified disposable container.
Creates and drops two uniquely named fixture databases. Provider state remains
outside both databases. No production service or provider is used.
"""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile
from threading import Thread
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]


def digest(value):
    # These fixtures contain integer numbers and ASCII strings. Every digest is
    # accepted again by the Rust authority; this is not a general JSON codec.
    return "sha256:" + hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


class Gateway(BaseHTTPRequestHandler):
    receipts = {}
    calls = []

    def log_message(self, *_):
        pass

    def do_POST(self):
        assert self.headers["Authorization"] == "Bearer fixture-effect-key"
        attempt = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        intent = attempt["intent"]
        key = intent["operation_key"]
        self.calls.append(attempt["kind"])
        if attempt["kind"] == "write":
            self.receipts.setdefault(key, {
                "operation_key": key, "intent_digest": digest(intent), "target": intent["policy"]["target"],
                "resource_id": "fixture-release", "provider_receipt": "provider-commit-1",
                "outputs": {"release_id": "fixture-release"}})
        receipt = self.receipts.get(key)
        observation = {"status": "applied", "receipt": receipt} if receipt else {"status": "absent"}
        data = json.dumps({"request_digest": digest(attempt), "observation": observation}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--container-engine", choices=["docker", "podman"], default="docker")
    parser.add_argument("--container", required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    suffix = uuid.uuid4().hex
    source, destination = "workflow_source_" + suffix, "workflow_restored_" + suffix
    databases, children = [], []
    environment = {**os.environ, "WORKFLOW_EFFECT_FIXTURE_KEY": "fixture-effect-key"}
    stats = {}

    def pg(*command, data=None):
        result = subprocess.run([args.container_engine, "exec", "-i", args.container, *command],
                                input=data, capture_output=True, timeout=90)
        assert result.returncode == 0, (command[0], result.returncode)
        return result.stdout

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="workflow-r04-database-") as directory:
            temp = Path(directory)

            def save(name, value, private=False):
                path = temp / name
                path.write_text(value if isinstance(value, str) else json.dumps(value))
                if private:
                    path.chmod(0o600)
                return str(path)

            def run(*command, code=0):
                result = subprocess.run([str(binary), *map(str, command)], cwd=ROOT, env=environment,
                                        capture_output=True, text=True, timeout=60)
                assert result.returncode == code, (command[:2], result.returncode, code)
                return json.loads(result.stdout if code == 0 else result.stderr)

            def openssl(*command):
                subprocess.run(["openssl", *command], check=True, capture_output=True, timeout=20)

            openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(temp / "key.pem"),
                    "-out", str(temp / "ca.pem"), "-days", "1", "-subj", "/CN=localhost")
            openssl("req", "-new", "-key", str(temp / "key.pem"), "-out", str(temp / "server.csr"), "-subj", "/CN=localhost")
            extension = save("server.ext", "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
            openssl("x509", "-req", "-in", str(temp / "server.csr"), "-CA", str(temp / "ca.pem"),
                    "-CAkey", str(temp / "key.pem"), "-CAcreateserial", "-out", str(temp / "server.pem"),
                    "-days", "1", "-extfile", extension)
            (temp / "key.pem").chmod(0o600)

            def server_binding(database):
                connection = save(database + "-connection", os.environ["WORKFLOW_TEST_POSTGRES"] + " dbname=" + database, private=True)
                return save(database + "-server.json", {
                    "listen": "127.0.0.1:0", "certificate_file": str(temp / "server.pem"),
                    "private_key": {"type": "file", "path": str(temp / "key.pem")},
                    "database": {"connection": {"type": "file", "path": connection}, "transport": {"type": "local"}},
                    "max_connections": 8, "max_operations": 4})

            def serve(binding):
                child = subprocess.Popen([str(binary), "service", "serve", binding],
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                children.append(child)
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout, selectors.EVENT_READ)
                    assert selector.select(20), "service did not bind"
                return child, json.loads(child.stdout.readline())["bound_address"].rsplit(":", 1)[1]

            def client(name, port, token):
                return save(name + "-client.json", {
                    "endpoint": f"https://localhost:{port}/v1/operations", "ca_file": str(temp / "ca.pem"),
                    "credential": {"type": "file", "path": token}, "timeout_ms": 30000})

            def call(binding, operation, code=0):
                path = save("operation.json", {"protocol_version": 1, "request_id": "recovery-example", "operation": operation})
                return run("remote", "call", binding, path, code=code)

            def provision(binding, admin, label, role, capabilities):
                token = str(temp / label)
                request = save(label + "-request.json", {"actor": label, "role": role, "capabilities": capabilities, "ttl_ms": 600000})
                info = run("service", "issue", binding, admin, request, token)
                return token, info["credential_id"]

            for name in [source, destination]:
                pg("createdb", "-U", "postgres", name)
                databases.append(name)
            source_binding, restored_binding = server_binding(source), server_binding(destination)
            source_admin = str(temp / "source-admin")
            run("service", "bootstrap", source_binding, "fixture", "project", "operator", source_admin)
            admin_ref = save("source-admin-ref.json", {"type": "file", "path": source_admin})
            source_child, source_port = serve(source_binding)
            tokens, clients, ids = {}, {}, {}
            for role in ["definition_maintainer", "runner", "scheduler", "viewer"]:
                tokens[role], ids[role] = provision(source_binding, admin_ref, "source-" + role, role, [])
                clients[role] = client(role, source_port, tokens[role])

            # A completed report carries a real assignment-derived artifact.
            report = json.loads((ROOT / "examples/artifacts/shared-report-start.json").read_text())
            cap = report["bundle"]["capabilities"][0]
            artifact_type = {"identity": {"id": "restore-report", "version": "1.0.0"}, "content": {"format": "utf8"}}
            rule = {**cap["capability"], "contract_digest": digest(cap), "artifacts": {
                "inputs": {}, "output": {"types": [artifact_type], "repository_input": "repository", "revision_input": "revision"}}}
            reporter, reporter_id = provision(source_binding, admin_ref, "reporter", "worker", [rule])
            reporter_client = client("reporter", source_port, reporter)
            call(clients["definition_maintainer"], {"type": "publish", "bundle": report["bundle"]})
            call(clients["runner"], {"type": "start", "request": report})
            lease = call(clients["scheduler"], {"type": "acquire", "run_id": report["run_id"], "acquisition_id": "report", "ttl_ms": 120000})["value"]
            assignment = call(clients["scheduler"], {"type": "dispatch", "lease": lease, "worker_id": reporter_id})["value"]["assignment_id"]
            task = call(reporter_client, {"type": "assignment", "assignment_id": assignment})["value"]
            payload = "committed artifact from the fixture report worker"
            upload = call(reporter_client, {"type": "artifact_begin", "request": {
                "request_id": "report", "assignment_id": assignment, "artifact_type": artifact_type,
                "bytes": len(payload), "content_digest": "sha256:" + hashlib.sha256(payload.encode()).hexdigest()}})["value"]
            call(reporter_client, {"type": "artifact_put", "upload_id": upload["upload_id"], "offset": 0, "content": list(payload.encode())})
            artifact = call(reporter_client, {"type": "artifact_complete", "upload_id": upload["upload_id"]})["value"]
            link = {key: artifact[key] for key in ["artifact_id", "digest"]}
            completion = {"protocol_version": 1, "request_digest": digest(task["request"]),
                          "completed_at_unix_ms": int(time.time() * 1000),
                          "outcome": {"status": "succeeded", "outputs": {"ok": True}, "evidence": [link]}}
            call(reporter_client, {"type": "finish", "assignment_id": assignment, "result": completion})
            committed = call(clients["viewer"], {"type": "get", "run_id": report["run_id"]})["value"]
            assert committed["status"] == "succeeded"

            effect = json.loads((ROOT / "examples/runs/effect-release.json").read_text())
            call(clients["definition_maintainer"], {"type": "publish", "bundle": effect["bundle"]})
            call(clients["runner"], {"type": "start", "request": effect})
            policy = effect["bundle"]["effect_bindings"][0]["policy"]
            effect_cap = effect["bundle"]["capabilities"][0]
            effect_rule = {**effect_cap["capability"], "contract_digest": digest(effect_cap), "effect": {"policy": policy}}
            worker, worker_id = provision(source_binding, admin_ref, "effect-worker", "worker", [effect_rule])
            worker_client = client("effect-worker", source_port, worker)
            baseline = call(clients["viewer"], {"type": "get", "run_id": effect["run_id"]})["value"]
            started = time.monotonic()
            archive = pg("pg_dump", "-U", "postgres", "-Fc", source)
            stats["dump_ms"] = round((time.monotonic() - started) * 1000)
            backup_digest = "sha256:" + hashlib.sha256(archive).hexdigest()
            stats["backup_bytes"] = len(archive)

            # The source really applies a write AFTER the database snapshot.
            # Its original intent and external provider receipt survive outside it.
            lease = call(clients["scheduler"], {"type": "acquire", "run_id": effect["run_id"], "acquisition_id": "post-backup", "ttl_ms": 120000})["value"]
            call(clients["scheduler"], {"type": "dispatch_effect", "lease": lease, "worker_id": worker_id})
            effect_bindings = save("effects.json", [{"schema_version": 1, "target": policy["target"],
                "call_identity": policy["call_identity"], "capability": effect_cap,
                "endpoint": f"http://127.0.0.1:{gateway.server_port}", "api_key_env": "WORKFLOW_EFFECT_FIXTURE_KEY", "allow_loopback_http": True}])
            run("remote", "work-effects", worker_client, effect_bindings, "1", "20")
            effects = call(clients["viewer"], {"type": "effects", "run_id": effect["run_id"], "after": 0, "limit": 100})["value"]["items"]
            original = effects[0]["intent"]
            receipt = Gateway.receipts[original["operation_key"]]
            assert Gateway.calls == ["write"]
            source_child.send_signal(signal.SIGTERM)
            source_child.communicate(timeout=20)
            assert source_child.returncode == 0

            started = time.monotonic()
            pg("pg_restore", "-U", "postgres", "--single-transaction", "--exit-on-error", "--no-owner", "--no-privileges", "-d", destination, data=archive)
            request = save("restore.json", {"database": destination, "backup_digest": backup_digest,
                           "actor": "recovery-operator", "reason": "offline fixture restore after source retirement"})
            output = temp / "restored-administrators.json"
            summary = run("service", "fence-restored", restored_binding, request, output)
            stats["restore_to_held_ms"] = round((time.monotonic() - started) * 1000)
            assert summary["runs"] == 2 and summary["scopes"] == 1
            assert output.stat().st_mode & 0o777 == 0o600
            recovered = json.loads(output.read_text())
            secret = recovered["administrators"][0]["secret"]
            assert secret not in json.dumps(summary)
            new_admin = save("new-admin", secret, private=True)
            new_admin_ref = save("new-admin-ref.json", {"type": "file", "path": new_admin})
            restored_child, port = serve(restored_binding)
            revoked = client("revoked", port, tokens["viewer"])
            assert call(revoked, {"type": "get", "run_id": report["run_id"]}, code=2)["code"] == "unauthorized"
            new_clients, new_ids = {}, {}
            for role in ["viewer", "recovery", "scheduler"]:
                token, new_ids[role] = provision(restored_binding, new_admin_ref, "restored-" + role, role, [])
                new_clients[role] = client("restored-" + role, port, token)
            assert call(new_clients["viewer"], {"type": "get", "run_id": report["run_id"]})["value"] == committed
            download = call(new_clients["viewer"], {"type": "artifact_grant", "artifact": link, "assignment_id": None, "ttl_ms": 60000})["value"]
            chunk = call(new_clients["viewer"], {"type": "artifact_get", "download_id": download["download_id"], "offset": 0})["value"]
            assert bytes(chunk["content"]).decode() == payload
            held = call(new_clients["viewer"], {"type": "get", "run_id": effect["run_id"]})["value"]
            assert held["pause"] and held["frames"] == baseline["frames"]
            barrier = call(new_clients["viewer"], {"type": "recovery_barrier", "run_id": effect["run_id"]})["value"]
            assert barrier["backup_digest"] == backup_digest
            call(new_clients["recovery"], {"type": "control", "request": {"run_id": effect["run_id"], "event_id": "restore-resume",
                 "expected_revision": held["revision"], "control": {"type": "resume", "reason": "provider reconciliation only"}}})
            new_worker, new_worker_id = provision(restored_binding, new_admin_ref, "restored-effect-worker", "worker", [effect_rule])
            lease = call(new_clients["scheduler"], {"type": "acquire", "run_id": effect["run_id"], "acquisition_id": "held", "ttl_ms": 120000})["value"]
            blocked = call(new_clients["scheduler"], {"type": "dispatch_effect", "lease": lease, "worker_id": new_worker_id}, code=2)
            assert blocked["code"] == "recovery_required"
            call(new_clients["scheduler"], {"type": "release", "lease": lease})
            lease = call(new_clients["recovery"], {"type": "acquire", "run_id": effect["run_id"], "acquisition_id": "import", "ttl_ms": 120000})["value"]
            imported = {"intent": original, "resolution": {"resolution_id": "provider-reconciled", "actor": "untrusted-payload-actor",
                        "reason": "original source intent and provider record recovered", "evidence": "fixture gateway retained provider-commit-1",
                        "outcome": {"status": "applied", "receipt": receipt}}}
            op = {"type": "import_restored_effect", "lease": lease, "request": imported}
            assert call(new_clients["viewer"], op, code=2)["code"] == "unauthorized"
            before = call(new_clients["viewer"], {"type": "get", "run_id": effect["run_id"]})["value"]
            tampered = json.loads(json.dumps(op))
            tampered["request"]["intent"]["inputs"]["release_name"] = "another-target"
            assert call(new_clients["recovery"], tampered, code=2)["code"] == "invalid_request"
            assert call(new_clients["viewer"], {"type": "get", "run_id": effect["run_id"]})["value"] == before
            call(new_clients["recovery"], op)
            assert call(new_clients["recovery"], op)["value"]["transition"]["duplicate"]
            state = call(new_clients["viewer"], {"type": "get", "run_id": effect["run_id"]})["value"]
            assert state["status"] == "succeeded" and Gateway.calls == ["write"]
            call(new_clients["recovery"], {"type": "acknowledge_recovery", "run_id": effect["run_id"], "request": {
                "resolution_id": "completed-provider-audit", "no_missing_effect_intents": True, "generation": barrier["generation"],
                "backup_digest": backup_digest, "actor": "payload-actor", "reason": "fixture source stopped and sole write imported",
                "evidence": "source process terminated; gateway contains exactly provider-commit-1"}})
            assert call(new_clients["viewer"], {"type": "recovery_barrier", "run_id": effect["run_id"]})["value"] is None
            image = pg("psql", "-U", "postgres", "-d", destination, "-At", "-c",
                       "SELECT convert_from(image,'UTF8') FROM workflow_authority.runs WHERE run_id='demo-effect-release'")
            assert b"restored-recovery" in image and b"untrusted-payload-actor" not in image
            restored_child.send_signal(signal.SIGTERM)
            restored_child.communicate(timeout=20)
            assert restored_child.returncode == 0
    finally:
        for child in children:
            if child.poll() is None:
                child.kill()
                child.wait()
        gateway.shutdown()
        gateway.server_close()
        thread.join(timeout=5)
        for database in reversed(databases):
            pg("dropdb", "-U", "postgres", "--force", database)
    print(json.dumps({"status": "pass", "snapshot_runs": 2, "retained_artifacts": 1,
                      "provider_writes": Gateway.calls.count("write"), "post_backup_effects_imported": 1,
                      "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), **stats}, indent=2))


if __name__ == "__main__":
    main()
