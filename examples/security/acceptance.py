#!/usr/bin/env python3
"""R14 admission: real CLI, TLS, PostgreSQL and rotating provider leases.

Run only with disposable WORKFLOW_TEST_POSTGRES. Uses generated fixture credentials.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread

ROOT = Path(__file__).resolve().parents[2]


class Provider(BaseHTTPRequestHandler):
    calls = []
    denied = 0
    mode = "good"
    active = "provider-first-private-fixture-key"
    expiry = 0
    rotate = False

    def log_message(self, *_):
        pass

    @classmethod
    def provision(cls, key):
        cls.active = key
        now = int(time.time() * 1000)
        cls.expiry = now + 120000
        envelope = {"schema_version": 1, "principal": cls.principal,
                    "audience": cls.audience, "not_before_unix_ms": now,
                    "expires_at_unix_ms": cls.expiry, "secret": key}
        temporary = cls.lease_path.with_suffix(".next")
        with temporary.open("x") as out:
            temporary.chmod(0o600)
            json.dump(envelope, out)
            out.flush()
            os.fsync(out.fileno())
        temporary.replace(cls.lease_path)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        cls = type(self)
        if (self.headers.get("Authorization") != "Bearer " + cls.active
                or int(time.time() * 1000) >= cls.expiry):
            cls.denied += 1
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        context = json.loads(json.loads(raw)["input"])
        if cls.mode == "escalate":
            action = {"type": "call", "summary": "Untrusted escalation request",
                      "capability": {"id": "admin.publish", "version": "1.0.0"}, "inputs": {}}
        elif cls.mode == "approval":
            action = {"type": "approve", "actor": "administrator", "decision": "approve"}
        elif cls.mode == "graph":
            action = {"type": "replace_graph", "edges": []}
        elif cls.mode == "echo":
            action = {"type": "complete", "outputs": {}, "summary": cls.active}
        elif context["events"]:
            action = {"type": "complete", "summary": "Return observed validation",
                      "outputs": context["events"][-1]["response"]["result"]["outcome"]["outputs"]}
        else:
            action = {"type": "call", "summary": "Inspect definition",
                      "capability": context["policy"]["tools"][0]["capability"], "inputs": context["inputs"]}
        cls.calls.append(cls.mode)
        if cls.rotate:
            cls.rotate = False
            cls.provision("provider-rotated-private-fixture-key")
        reply = {"id": "resp_fixture", "model": "fixture-v1", "status": "completed",
                 "output": [{"type": "message", "role": "assistant", "status": "completed",
                             "content": [{"type": "output_text", "text": json.dumps({"protocol_version": 1, "action": action})}]}]}
        data = json.dumps(reply).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    binary = parser.parse_args().binary.resolve()
    transcripts = []
    with tempfile.TemporaryDirectory(prefix="workflow-r14-cli-") as directory:
        temp = Path(directory)

        def save(name, value, private=False):
            path = temp / name
            path.write_text(value if isinstance(value, str) else json.dumps(value))
            if private:
                path.chmod(0o600)
            return str(path)

        def run(*command, code=0):
            result = subprocess.run([str(binary), *map(str, command)], cwd=ROOT,
                                    capture_output=True, text=True, timeout=40)
            transcripts.append(result.stdout + result.stderr)
            assert result.returncode == code, (command[:2], result.returncode, code)
            return json.loads(result.stdout or result.stderr)

        def openssl(*command):
            subprocess.run(["openssl", *command], check=True, capture_output=True, timeout=20)

        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(temp / "key.pem"),
                "-out", str(temp / "ca.pem"), "-days", "1", "-subj", "/CN=localhost")
        openssl("req", "-new", "-key", str(temp / "key.pem"), "-out", str(temp / "server.csr"),
                "-subj", "/CN=localhost")
        extension = save("server.ext", "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
        openssl("x509", "-req", "-in", str(temp / "server.csr"), "-CA", str(temp / "ca.pem"),
                "-CAkey", str(temp / "key.pem"), "-CAcreateserial", "-out", str(temp / "server.pem"),
                "-days", "1", "-extfile", extension)
        (temp / "key.pem").chmod(0o600)
        database = save("database", os.environ["WORKFLOW_TEST_POSTGRES"], private=True)
        server = save("server.json", {
            "listen": "127.0.0.1:0", "certificate_file": str(temp / "server.pem"),
            "private_key": {"type": "file", "path": str(temp / "key.pem")},
            "database": {"connection": {"type": "file", "path": database}, "transport": {"type": "local"}},
            "max_connections": 8, "max_operations": 4})
        tenant = "r14-cli-" + uuid.uuid4().hex
        tokens, credentials, admin_refs = {}, {}, {}
        for prefix in ["", "other-"]:
            admin = str(temp / (prefix + "admin"))
            run("service", "bootstrap", server, tenant + ("-other" if prefix else ""), "project", "operator", admin)
            tokens[prefix + "administrator"] = admin
            admin_refs[prefix] = save(prefix + "admin-ref.json", {"type": "file", "path": admin})
        start = json.loads((ROOT / "examples/models/start.json").read_text())
        bundle = save("bundle.json", start["bundle"])
        checked = run("model", "check-policy", "examples/models/policy.json")["result"]
        policy = checked["binding"]
        task = start["bundle"]["model_policies"][0]["task"]
        rule = {"id": task["capability"]["id"], "version": task["capability"]["version"],
                "contract_digest": checked["task_contract_digest"], "model_policy": policy}
        for prefix in ["", "other-"]:
            for role in ["definition_maintainer", "runner", "scheduler", "worker", "viewer", "recovery"]:
                name = prefix + role
                tokens[name] = str(temp / name)
                provision = save(name + "-provision.json", {"actor": role, "role": role,
                                "capabilities": [rule] if role == "worker" else [], "ttl_ms": 300000})
                credentials[name] = run("service", "issue", server, admin_refs[prefix], provision, tokens[name])["credential_id"]
        child = subprocess.Popen([str(binary), "service", "serve", server],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                assert selector.select(20), "service did not bind"
            port = json.loads(child.stdout.readline())["bound_address"].rsplit(":", 1)[1]
            clients = {role: save(role + "-client.json", {
                "endpoint": f"https://localhost:{port}/v1/operations", "ca_file": str(temp / "ca.pem"),
                "credential": {"type": "file", "path": token}, "timeout_ms": 20000})
                for role, token in tokens.items()}

            def call(role, operation, code=0):
                request = save("request.json", {"protocol_version": 1, "request_id": "example", "operation": operation})
                return run("remote", "call", clients[role], request, code=code)

            call("definition_maintainer", {"type": "publish", "bundle": start["bundle"]})
            call("other-definition_maintainer", {"type": "publish", "bundle": start["bundle"]})
            call("other-runner", {"type": "start", "request": {**start, "run_id": "foreign-only"}})
            for op in [{"type": "get", "run_id": "foreign-only"},
                       {"type": "history", "run_id": "foreign-only", "after": 0, "limit": 100}]:
                call("runner", op, code=2)
            Provider.principal = {"tenant": tenant, "project": "project", "actor": "worker"}
            Provider.lease_path = temp / "provider-lease.json"
            Provider.audience = f"http://127.0.0.1:{provider.server_port}/openai"
            Provider.provision(Provider.active)
            http = {"schema_version": 1, "provider": "openai_responses", "model": "fixture",
                    "endpoint": Provider.audience, "allow_loopback_http": True,
                    "credential": {"path": str(Provider.lease_path), "principal": Provider.principal}}
            bindings = save("bindings.json", [{"policy": policy, "http": http}])
            binding_digest = run("model", "describe-binding", save("http.json", http))["result"]["binding_digest"]

            def start_task(name):
                call("runner", {"type": "start", "request": {**start, "run_id": name}})
                lease = call("scheduler", {"type": "acquire", "run_id": name, "acquisition_id": name, "ttl_ms": 120000})["value"]
                dispatch = call("scheduler", {"type": "dispatch", "lease": lease, "worker_id": credentials["worker"]})["value"]
                return lease, dispatch["assignment_id"]

            _, assignment = start_task("rotating")
            for principal in [{**Provider.principal, "tenant": tenant + "-other"},
                              {**Provider.principal, "project": "other"},
                              {**Provider.principal, "actor": "other"}]:
                wrong = {**http, "credential": {"path": str(Provider.lease_path), "principal": principal}}
                run("remote", "work-models", clients["worker"], bundle,
                    save("wrong-binding.json", [{"policy": policy, "http": wrong}]), "1", "20", code=2)
            call("other-worker", {"type": "assignment", "assignment_id": assignment}, code=2)
            assert Provider.calls == []
            # Two model calls in one worker process straddle atomic broker rotation.
            Provider.rotate = True
            report = run("remote", "work-models", clients["worker"], bundle, bindings, "1", "20")
            assert report == {"settled_tasks": 1, "rejected_tasks": 0, "fenced": 0}
            snapshot = call("runner", {"type": "get", "run_id": "rotating"})["value"]
            assert snapshot["status"] == "succeeded"
            original_bundle_digest = snapshot["bundle_digest"]
            assert Provider.calls == ["good", "good"]
            assert run("model", "describe-binding", save("http.json", http))["result"]["binding_digest"] == binding_digest
            old_request = urllib.request.Request(Provider.audience, data=b"{}", headers={"Authorization": "Bearer provider-first-private-fixture-key"})
            try:
                urllib.request.urlopen(old_request, timeout=5)
                raise AssertionError("retired gateway key accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 403
            for mode in ["escalate", "approval", "graph", "echo"]:
                Provider.mode = mode
                start_task(mode)
                run("remote", "work-models", clients["worker"], bundle, bindings, "1", "20")
                state = call("runner", {"type": "get", "run_id": mode})["value"]
                assert state["status"] == "failed", mode
                assert state["bundle_digest"] == original_bundle_digest
                assert list(state["frames"]["1"]["nodes"]) == list(snapshot["frames"]["1"]["nodes"])
                call("runner", {"type": "history", "run_id": mode, "after": 0, "limit": 100})
            reflected = {**start, "run_id": "reflect-current-bearer",
                         "inputs": {**start["inputs"], "document": Path(tokens["runner"]).read_text()}}
            call("runner", {"type": "start", "request": reflected}, code=2)
            call("runner", {"type": "get", "run_id": "reflect-current-bearer"}, code=2)
            for role in ["worker", "viewer", "runner", "scheduler", "definition_maintainer"]:
                target = temp / ("denied-" + role + ".json")
                run("remote", "audit-export", clients[role], target, code=2)
                assert not target.exists()
            for role in ["administrator", "recovery", "other-administrator"]:
                target = temp / ("audit-" + role + ".json")
                report = run("remote", "audit-export", clients[role], target)
                archive = json.loads(target.read_text())
                assert target.stat().st_mode & 0o777 == 0o600
                assert archive["tenant"] == (tenant + "-other" if role.startswith("other-") else tenant)
                normalized = [archive[k] for k in ["schema_version", "tenant", "project", "exported_by", "entries"]]
                expected = "sha256:" + hashlib.sha256(json.dumps(normalized, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()
                assert archive["digest"] == report["digest"] == expected
                transcripts.append(target.read_text())
                before = target.read_bytes()
                run("remote", "audit-export", clients[role], target, code=2)
                assert target.read_bytes() == before
            child.send_signal(signal.SIGTERM)
            out, err = child.communicate(timeout=20)
            transcripts.extend([out, err])
            assert child.returncode == 0
            for secret in [Path(token).read_text() for token in tokens.values()] + [
                    "provider-first-private-fixture-key", "provider-rotated-private-fixture-key"]:
                assert all(secret not in value for value in transcripts), "credential leaked in output or export"
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            provider.shutdown()
            provider.server_close()
            thread.join(timeout=5)
    print(json.dumps({"status": "pass", "authenticated_tenants": 2,
                      "provider_calls": len(Provider.calls), "retired_key_denials": Provider.denied,
                      "rotation_inside_one_worker_process": True, "audit_export_roles": 3,
                      "untrusted_output_modes_blocked": 4, "business_definition_changes": 0,
                      "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}, indent=2))


if __name__ == "__main__":
    main()
