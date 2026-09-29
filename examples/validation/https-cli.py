#!/usr/bin/env python3
"""R01 executable CLI parity against real TLS and disposable PostgreSQL.

Requires OpenSSL and WORKFLOW_TEST_POSTGRES. Creates one unique fixture tenant.
Never use a production database. Certificates and credentials are temporary.
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
import uuid

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    database = os.environ["WORKFLOW_TEST_POSTGRES"]
    checks = []
    with tempfile.TemporaryDirectory(prefix="workflow-r01-https-cli-") as directory:
        temp = Path(directory)

        def save(name, value, private=False):
            path = temp / name
            path.write_text(value if isinstance(value, str) else json.dumps(value))
            if private:
                path.chmod(0o600)
            return str(path)

        def run(*command, code=0):
            result = subprocess.run([str(binary), *map(str, command)], cwd=ROOT,
                                    capture_output=True, text=True, timeout=30)
            # Never include child output or secret-bearing bindings in failures.
            assert result.returncode == code, (command[:2], result.returncode, code)
            return result

        def openssl(*command):
            subprocess.run(["openssl", *command], check=True, capture_output=True, timeout=20)

        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout",
                str(temp / "key.pem"), "-out", str(temp / "ca.pem"), "-days", "1",
                "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost")
        openssl("req", "-new", "-key", str(temp / "key.pem"), "-out",
                str(temp / "server.csr"), "-subj", "/CN=localhost")
        extension = save("server.ext", "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
        openssl("x509", "-req", "-in", str(temp / "server.csr"), "-CA", str(temp / "ca.pem"),
                "-CAkey", str(temp / "key.pem"), "-CAcreateserial", "-out", str(temp / "server.pem"),
                "-days", "1", "-extfile", extension)
        (temp / "key.pem").chmod(0o600)
        database_file = save("database", database, private=True)
        server = save("server.json", {
            "listen": "127.0.0.1:0", "certificate_file": str(temp / "server.pem"),
            "private_key": {"type": "file", "path": str(temp / "key.pem")},
            "database": {"connection": {"type": "file", "path": database_file}, "transport": {"type": "local"}},
            "max_connections": 8, "max_operations": 4})
        admin = str(temp / "admin")
        run("service", "bootstrap", server, "r01-cli-" + uuid.uuid4().hex, "project", "operator", admin)
        admin_ref = save("admin-ref.json", {"type": "file", "path": admin})
        tokens = {}
        for role in ["definition_maintainer", "runner", "viewer"]:
            tokens[role] = str(temp / role)
            provision = save(role + "-provision.json", {"actor": role, "role": role,
                            "capabilities": [], "ttl_ms": 120000})
            run("service", "issue", server, admin_ref, provision, tokens[role])
        child = subprocess.Popen([str(binary), "service", "serve", server],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                assert selector.select(20), "service failed to bind within observation ceiling"
            port = json.loads(child.stdout.readline())["bound_address"].rsplit(":", 1)[1]
            clients = {role: save(role + "-client.json", {
                "endpoint": f"https://localhost:{port}/v1/operations", "ca_file": str(temp / "ca.pem"),
                "credential": {"type": "file", "path": token}, "timeout_ms": 10000})
                for role, token in tokens.items()}
            invalid = json.loads((ROOT / "examples/review.json").read_text())
            invalid["edges"][0]["to"] = "missing"
            invalid_file = save("invalid.json", invalid)
            malformed = save("malformed.yaml", "nodes: [")
            for path, code in [("examples/review.json", 0), ("examples/review.yaml", 0),
                               (invalid_file, 1), (malformed, 1)]:
                local = run("validate", path, code=code)
                for role in ["definition_maintainer", "runner"]:
                    remote = run("remote", "validate", clients[role], path, code=code)
                    assert remote.stdout == local.stdout and remote.stderr == local.stderr
                checks.append({"case": Path(path).name, "exit": code, "byte_equal_reports": True})
            denied = run("remote", "validate", clients["viewer"], "examples/review.json", code=2)
            assert not denied.stdout and denied.stderr.strip() == "remote validation could not be confirmed"
            checks.append({"case": "unauthorized", "exit": 2, "report_absent": True})
            missing = run("remote", "validate", clients["runner"], str(temp / "missing.json"), code=2)
            assert json.loads(missing.stdout)["diagnostics"][0]["code"] == "io_error"
            checks.append({"case": "missing-file", "exit": 2})
            child.send_signal(signal.SIGTERM)
            out, err = child.communicate(timeout=20)
            assert child.returncode == 0 and json.loads(out)["stopped"]
            assert all(Path(token).read_text() not in out + err for token in tokens.values())
            unavailable = run("remote", "validate", clients["runner"], "examples/review.json", code=2)
            assert not unavailable.stdout
            checks.append({"case": "unavailable", "exit": 2, "report_absent": True})
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
    print(json.dumps({"status": "pass", "transport": "https", "storage": "postgres",
                      "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                      "checks": checks}, indent=2))


if __name__ == "__main__":
    main()
