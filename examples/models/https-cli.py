#!/usr/bin/env python3
"""Exercise remote model CLI bindings with two wire fixtures and disposable PostgreSQL.

Requires WORKFLOW_TEST_POSTGRES and OpenSSL. No production model key is used.
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread
import uuid

ROOT = Path(__file__).resolve().parents[2]


class Provider(BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        openai = self.path == "/openai"
        assert self.headers["Authorization" if openai else "x-api-key"] == (
            "Bearer fixture-key" if openai else "fixture-key")
        context = json.loads(body["input"] if openai else body["messages"][0]["content"])
        if context["events"]:
            action = {"type": "complete", "summary": "Return observed validation",
                      "outputs": context["events"][-1]["response"]["result"]["outcome"]["outputs"]}
        else:
            action = {"type": "call", "summary": "Inspect definition",
                      "capability": context["policy"]["tools"][0]["capability"],
                      "inputs": context["inputs"]}
        text = json.dumps({"protocol_version": 1, "action": action})
        if openai:
            reply = {"id": "resp_fixture", "model": "fixture-v1", "status": "completed",
                     "output": [{"type": "message", "role": "assistant", "status": "completed",
                                 "content": [{"type": "output_text", "text": text}]}]}
        else:
            reply = {"id": "msg_fixture", "model": "fixture-v1", "type": "message", "role": "assistant",
                     "stop_reason": "end_turn", "content": [{"type": "text", "text": text}]}
        data = json.dumps(reply).encode()
        self.calls.append(self.path)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    binary = parser.parse_args().binary.resolve()
    environment = {**os.environ, "WORKFLOW_MODEL_FIXTURE_KEY": "fixture-key"}
    with tempfile.TemporaryDirectory(prefix="workflow-r02-cli-") as directory:
        temp = Path(directory)

        def save(name, value, private=False):
            path = temp / name
            path.write_text(value if isinstance(value, str) else json.dumps(value))
            if private:
                path.chmod(0o600)
            return str(path)

        def run(*command):
            result = subprocess.run([str(binary), *map(str, command)], cwd=ROOT,
                                    env=environment, capture_output=True, text=True, timeout=40)
            assert result.returncode == 0, (command[:2], result.returncode)
            return json.loads(result.stdout)

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
        admin = str(temp / "admin")
        run("service", "bootstrap", server, "r02-cli-" + uuid.uuid4().hex, "project", "operator", admin)
        admin_ref = save("admin-ref.json", {"type": "file", "path": admin})
        start = json.loads((ROOT / "examples/models/start.json").read_text())
        bundle = save("bundle.json", start["bundle"])
        checked = run("model", "check-policy", "examples/models/policy.json")["result"]
        policy = checked["binding"]
        # Derive contract identity using the project's canonical codec, not another JSON implementation.
        tokens, credentials = {}, {}
        task = start["bundle"]["model_policies"][0]["task"]
        rule = {"id": task["capability"]["id"], "version": task["capability"]["version"],
                "contract_digest": checked["task_contract_digest"], "model_policy": policy}
        for role in ["definition_maintainer", "runner", "scheduler", "worker"]:
            tokens[role] = str(temp / role)
            provision = save(role + "-provision.json", {"actor": role, "role": role,
                            "capabilities": [rule] if role == "worker" else [], "ttl_ms": 300000})
            credentials[role] = run("service", "issue", server, admin_ref, provision, tokens[role])["credential_id"]
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

            def call(role, operation):
                request = save("request.json", {"protocol_version": 1, "request_id": "example", "operation": operation})
                return run("remote", "call", clients[role], request)

            call("definition_maintainer", {"type": "publish", "bundle": start["bundle"]})
            outputs = []
            for name, provider_name in [("openai", "openai_responses"), ("anthropic", "anthropic_messages")]:
                start["run_id"] = name
                call("runner", {"type": "start", "request": start})
                lease = call("scheduler", {"type": "acquire", "run_id": name, "acquisition_id": name, "ttl_ms": 120000})["value"]
                call("scheduler", {"type": "dispatch", "lease": lease, "worker_id": credentials["worker"]})
                bindings = save("bindings.json", [{"policy": policy, "http": {
                    "schema_version": 1, "provider": provider_name, "model": "fixture",
                    "endpoint": f"http://127.0.0.1:{provider.server_port}/{name}",
                    "api_key_env": "WORKFLOW_MODEL_FIXTURE_KEY", "allow_loopback_http": True}}])
                report = run("remote", "work-models", clients["worker"], bundle, bindings, "1", "20")
                assert report == {"settled_tasks": 1, "rejected_tasks": 0, "fenced": 0}
                snapshot = call("runner", {"type": "get", "run_id": name})["value"]
                assert snapshot["status"] == "succeeded"
                outputs.append(snapshot["frames"]["1"]["nodes"]["inspect"]["outputs"])
            assert outputs[0] == outputs[1]
            assert Provider.calls == ["/openai", "/openai", "/anthropic", "/anthropic"]
            child.send_signal(signal.SIGTERM)
            child.communicate(timeout=20)
            assert child.returncode == 0
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            provider.shutdown()
            provider.server_close()
            thread.join(timeout=5)
    print(json.dumps({"status": "pass", "providers": 2, "provider_calls": 4,
                      "business_definition_changes": 0,
                      "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}, indent=2))


if __name__ == "__main__":
    main()
