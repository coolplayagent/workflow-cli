#!/usr/bin/env python3
"""R09 managed worker SIGTERM/drain, shared model pool and rolling-version CLI contract.

Requires disposable WORKFLOW_TEST_POSTGRES and OpenSSL. The provider is a local
fixture; credentials, certificates and process state live in a temporary directory.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from http.server import ThreadingHTTPServer
from threading import Event, Thread

ROOT = Path(__file__).resolve().parents[2]
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("security_fixture", ROOT / "examples/security/acceptance.py")
SECURITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SECURITY)


class Provider(SECURITY.Provider):
    entered = Event()
    release = Event()

    def do_POST(self):
        self.entered.set()
        assert self.release.wait(30), "fixture provider was never released"
        super().do_POST()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--legacy-binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    children, transcripts = [], []
    provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    Thread(target=provider.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="workflow-r09-cli-") as directory:
        temp = Path(directory)

        def save(name, value, private=False):
            path = temp / name
            path.write_text(value if isinstance(value, str) else json.dumps(value))
            if private:
                path.chmod(0o600)
            return str(path)

        def run(*command, code=0):
            r = subprocess.run([str(binary), *map(str, command)], cwd=ROOT,
                               capture_output=True, text=True, timeout=40)
            transcripts.append(r.stdout + r.stderr)
            assert r.returncode == code, (command[:2], r.returncode, code)
            return json.loads(r.stdout or r.stderr)

        def openssl(*command):
            subprocess.run(["openssl", *command], check=True, capture_output=True, timeout=20)

        def wait(check, seconds=20):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if check():
                    return
                time.sleep(0.1)
            raise AssertionError("fixture observation deadline exceeded")

        def spawn(*command):
            p = subprocess.Popen([str(binary), *map(str, command)], cwd=ROOT,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            children.append(p)
            return p

        try:
            openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(temp / "key.pem"),
                    "-out", str(temp / "ca.pem"), "-days", "1", "-subj", "/CN=localhost")
            openssl("req", "-new", "-key", str(temp / "key.pem"), "-out", str(temp / "server.csr"), "-subj", "/CN=localhost")
            extension = save("server.ext", "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
            openssl("x509", "-req", "-in", str(temp / "server.csr"), "-CA", str(temp / "ca.pem"),
                    "-CAkey", str(temp / "key.pem"), "-CAcreateserial", "-out", str(temp / "server.pem"),
                    "-days", "1", "-extfile", extension)
            (temp / "key.pem").chmod(0o600)
            database = save("database", os.environ["WORKFLOW_TEST_POSTGRES"], private=True)
            server = save("server.json", {"listen": "127.0.0.1:0", "certificate_file": str(temp / "server.pem"),
                "private_key": {"type": "file", "path": str(temp / "key.pem")},
                "database": {"connection": {"type": "file", "path": database}, "transport": {"type": "local"}},
                "max_connections": 8, "max_operations": 4})
            tenant = "r09-cli-" + uuid.uuid4().hex
            admin = str(temp / "administrator")
            run("service", "bootstrap", server, tenant, "project", "operator", admin)
            admin_ref = save("admin-ref.json", {"type": "file", "path": admin})
            start = json.loads((ROOT / "examples/models/start.json").read_text())
            checked = run("model", "check-policy", "examples/models/policy.json")["result"]
            binding = checked["binding"]
            rule = {**start["bundle"]["model_policies"][0]["task"]["capability"],
                    "contract_digest": checked["task_contract_digest"], "model_policy": binding}
            tokens, ids = {"administrator": admin}, {}
            for label in ["definition_maintainer", "runner", "scheduler", "worker", "next"]:
                role = "worker" if label == "next" else label
                tokens[label] = str(temp / label)
                request = save(label + "-provision.json", {"actor": label, "role": role,
                    "capabilities": [rule] if role == "worker" else [], "ttl_ms": 600000})
                ids[label] = run("service", "issue", server, admin_ref, request, tokens[label])["credential_id"]
            policy = json.loads((ROOT / "examples/cluster/policy.json").read_text())
            policy["model_pools"] = {"sop.inspect-policy@1.0.0": "provider-fixture"}
            policy["model_limits"] = {"provider-fixture": {"concurrent": 1, "per_minute": 10}}

            def configure(revision):
                return run("service", "configure-scheduling", server, save("configuration.json", {
                    "tenant": tenant, "expected_revision": revision, "policy": policy}))

            assert configure(None)["revision"] == 1
            if args.legacy_binary:
                output = temp / "legacy-credential"
                legacy = subprocess.run([str(args.legacy_binary.resolve()), "service", "issue", server,
                    admin_ref, str(temp / "runner-provision.json"), str(output)],
                    capture_output=True, text=True, timeout=30)
                assert legacy.returncode == 2 and not output.exists(), "legacy API did not reject access schema 3"
            api = spawn("service", "serve", server)
            with selectors.DefaultSelector() as selector:
                selector.register(api.stdout, selectors.EVENT_READ)
                assert selector.select(20), "service failed to bind"
            port = json.loads(api.stdout.readline())["bound_address"].rsplit(":", 1)[1]
            clients = {label: save(label + "-client.json", {
                "endpoint": f"https://localhost:{port}/v1/operations", "ca_file": str(temp / "ca.pem"),
                "credential": {"type": "file", "path": token}, "timeout_ms": 10000}) for label, token in tokens.items()}

            def call(label, operation, code=0):
                return run("remote", "call", clients[label], save("request.json", {
                    "protocol_version": 1, "request_id": "cluster-contract", "operation": operation}), code=code)

            def status(label):
                return call("scheduler", {"type": "worker_status", "worker_id": ids[label]})["value"]

            def worker(label, version):
                Provider.principal = {"tenant": tenant, "project": "project", "actor": label}
                Provider.lease_path = temp / (label + "-provider-lease.json")
                Provider.audience = f"http://127.0.0.1:{provider.server_port}/openai"
                Provider.provision("provider-cluster-fixture-key")
                http = {"schema_version": 1, "provider": "openai_responses", "model": "fixture",
                    "endpoint": Provider.audience, "allow_loopback_http": True,
                    "credential": {"path": str(Provider.lease_path), "principal": Provider.principal}}
                config = save(label + "-managed.json", {"runtime_version": version,
                    "heartbeat_interval_ms": 1000, "drain_timeout_ms": 30000,
                    "models": {"bundle": save("bundle.json", start["bundle"]),
                               "bindings": save(label + "-model-bindings.json", [{"policy": binding, "http": http}])}})
                # Registration gives a deterministic observation point; managed
                # execution still owns its subsequent independent heartbeats.
                call(label, {"type": "worker_heartbeat", "runtime_version": version, "drain": False})
                return spawn("remote", "managed-work", clients[label], config, "10000", "40")

            call("definition_maintainer", {"type": "publish", "bundle": start["bundle"]})
            leases = []
            for name in ["first", "second"]:
                call("runner", {"type": "start", "request": {**start, "run_id": name}})
                leases.append(call("scheduler", {"type": "acquire", "run_id": name,
                    "acquisition_id": name, "ttl_ms": 120000})["value"])
            first = worker("worker", "1.0.0")

            def dispatch(lease, label):
                return call("scheduler", {"type": "dispatch_routed", "lease": lease,
                    "worker_ids": [ids[label]], "effects": False})["value"]

            assert dispatch(leases[0], "worker")["type"] == "task"
            assert Provider.entered.wait(20), "provider did not start"
            # The model pool binds exact frozen policy identities, independent of
            # which API dispatch operation or worker is used.
            call("scheduler", {"type": "dispatch", "lease": leases[1], "worker_id": ids["worker"]}, code=2)
            first.send_signal(signal.SIGTERM)
            wait(lambda: status("worker")["draining"])
            assert first.poll() is None and status("worker")["active_assignments"] == 1
            assert dispatch(leases[1], "worker")["type"] == "deferred"
            Provider.release.set()
            out, err = first.communicate(timeout=30)
            transcripts.extend([out, err])
            assert first.returncode == 0 and json.loads(out) == {"completed": 1, "failed": 0, "fenced": 0, "drained": True}
            assert status("worker")["active_assignments"] == 0
            assert call("worker", {"type": "worker_heartbeat", "runtime_version": "1.0.0", "drain": False})["value"]["draining"]
            policy["allowed_worker_versions"] = ["2.0.0"]
            policy["model_limits"]["provider-fixture"]["per_minute"] = 1
            assert configure(1)["revision"] == 2
            call("worker", {"type": "worker_heartbeat", "runtime_version": "1.0.0", "drain": False}, code=2)
            successor = worker("next", "2.0.0")
            assert dispatch(leases[1], "next")["type"] == "deferred", "completion must retain model rate token"
            policy["model_limits"]["provider-fixture"]["per_minute"] = 10
            assert configure(2)["revision"] == 3
            assert dispatch(leases[1], "next")["type"] == "task"
            wait(lambda: call("runner", {"type": "get", "run_id": "second"})["value"]["status"] == "succeeded")
            successor.send_signal(signal.SIGTERM)
            out, err = successor.communicate(timeout=30)
            transcripts.extend([out, err])
            assert successor.returncode == 0 and json.loads(out)["drained"]
            assert call("runner", {"type": "get", "run_id": "first"})["value"]["status"] == "succeeded"
            assert len(Provider.calls) == 4
            for secret in [Path(p).read_text() for p in tokens.values()] + ["provider-cluster-fixture-key"]:
                assert all(secret not in text for text in transcripts), "credential leaked"
        finally:
            Provider.release.set()
            for child in reversed(children):
                if child.poll() is None:
                    child.kill()
                child.communicate(timeout=10)
            provider.shutdown()
    print(json.dumps({"status": "pass", "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "sigterm_in_flight": True, "drain_acknowledged": True, "model_pool_concurrency_and_rate": True,
        "rolling_version": "1.0.0 -> 2.0.0", "provider_calls": len(Provider.calls),
        "legacy_api_rejected": bool(args.legacy_binary)}, indent=2))


if __name__ == "__main__":
    main()
