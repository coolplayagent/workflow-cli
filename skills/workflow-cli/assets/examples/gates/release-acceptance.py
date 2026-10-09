#!/usr/bin/env python3
"""R03 real CLI acceptance matrix, optionally against authenticated HTTPS/PG.

The fixture worker validates a definition read from an actual Git commit. The
gateway owns a durable release receipt and independently observes its target.
It publishes no real release. --https requires disposable WORKFLOW_TEST_POSTGRES.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread, Lock

ROOT = Path(__file__).resolve().parents[2]


def digest(value):
    return "sha256:" + hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()


def now():
    return int(time.time() * 1000)


class Gateway(BaseHTTPRequestHandler):
    writes = 0
    requests = 0
    receipts = {}
    lock = Lock()

    def log_message(self, *_):
        pass

    def do_POST(self):
        attempt = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        cls = type(self)
        assert self.headers["Authorization"] == "Bearer release-fixture-key"
        intent = attempt["intent"]
        with cls.lock:
            cls.requests += 1
            key = intent["operation_key"]
            if key in cls.receipts:
                observation = {"status": "applied", "receipt": cls.receipts[key]}
            elif attempt["kind"] == "query":
                observation = {"status": "absent"}
            else:
                # The source checkout belongs to this disposable gateway. Target
                # changes use the same fixture lock as its compare-and-publish.
                actual = copy.deepcopy(cls.subject)
                actual["source_revision"]["revision"] = cls.git("rev-parse", "HEAD")
                if actual != intent["release"]["subject"] or now() >= attempt["deadline_unix_ms"]:
                    observation = {"status": "not_applied", "code": "rejected", "class": "business_rejected", "message": "Current candidate differs from checked delivery"}
                else:
                    receipt = {"operation_key": key, "intent_digest": digest(intent), "target": intent["policy"]["target"],
                               "resource_id": "release-" + key[7:19], "provider_receipt": "fixture-durable-" + key[7:19],
                               "outputs": {"release_id": "release-" + key[7:19]},
                               "release": {"subject": actual, "target_check": intent["release"]["policy"]["target_check"],
                                           "authorization_digest": digest(attempt["release"]), "observed_at_unix_ms": now()}}
                    # Persist before replying; a subsequent query reads this receipt.
                    destination = cls.directory / (key[7:] + ".json")
                    with destination.open("x") as out:
                        json.dump(receipt, out)
                        out.flush()
                        os.fsync(out.fileno())
                    cls.receipts[key] = json.loads(destination.read_text())
                    cls.writes += 1
                    observation = {"status": "applied", "receipt": receipt}
        data = json.dumps({"request_digest": digest(attempt), "observation": observation}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--https", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="workflow-r03-") as directory:
        temp = Path(directory)

        def save(name, value, private=False):
            path = temp / name
            path.write_text(value if isinstance(value, str) else json.dumps(value))
            if private:
                path.chmod(0o600)
            return str(path)

        def run(*command, code=0):
            result = subprocess.run([str(binary), *map(str, command)], cwd=ROOT, capture_output=True, text=True, timeout=60)
            assert result.returncode == code, (command[:3], result.returncode, code, result.stdout, result.stderr)
            return json.loads(result.stdout or result.stderr)

        source = temp / "source"
        source.mkdir()

        def git(*command):
            return subprocess.check_output(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", *command], cwd=source, stderr=subprocess.DEVNULL, text=True).strip()

        git("init", "--initial-branch=main")
        document = json.loads((ROOT / "examples/gates/protected-release.json").read_text())["inputs"]["document"]
        (source / "candidate.json").write_text(document)
        git("add", "candidate.json")
        git("commit", "-m", "checked candidate")
        base_revision = git("rev-parse", "HEAD")
        Gateway.git = staticmethod(git)
        Gateway.directory = temp
        gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
        Thread(target=gateway.serve_forever, daemon=True).start()
        os.environ["WORKFLOW_RELEASE_FIXTURE_KEY"] = "release-fixture-key"
        child = None
        clients, credentials = {}, {}
        server = None
        try:
            if args.https:
                def openssl(*command):
                    subprocess.run(["openssl", *command], check=True, capture_output=True, timeout=20)
                openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(temp / "key.pem"), "-out", str(temp / "ca.pem"), "-days", "1", "-subj", "/CN=localhost")
                openssl("req", "-new", "-key", str(temp / "key.pem"), "-out", str(temp / "server.csr"), "-subj", "/CN=localhost")
                ext = save("server.ext", "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
                openssl("x509", "-req", "-in", str(temp / "server.csr"), "-CA", str(temp / "ca.pem"), "-CAkey", str(temp / "key.pem"), "-CAcreateserial", "-out", str(temp / "server.pem"), "-days", "1", "-extfile", ext)
                (temp / "key.pem").chmod(0o600)
                server = save("server.json", {"listen": "127.0.0.1:0", "certificate_file": str(temp / "server.pem"), "private_key": {"type": "file", "path": str(temp / "key.pem")},
                                            "database": {"connection": {"type": "file", "path": save("database", os.environ["WORKFLOW_TEST_POSTGRES"], True)}, "transport": {"type": "local"}}, "max_connections": 8, "max_operations": 4})
                run("service", "init-artifacts", server)
                run("service", "init-effects", server)
                run("service", "bootstrap", server, "r03-" + uuid.uuid4().hex, "project", "operator", temp / "administrator")
                admin = save("admin-ref.json", {"type": "file", "path": str(temp / "administrator")})
                child = subprocess.Popen([str(binary), "service", "serve", server], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout, selectors.EVENT_READ)
                    assert selector.select(20), "service did not bind"
                port = json.loads(child.stdout.readline())["bound_address"].rsplit(":", 1)[1]

                def issue(name, role, rules=None):
                    token = str(temp / (name + ".token"))
                    credentials[name] = run("service", "issue", server, admin, save(name + "-provision.json", {"actor": name, "role": role, "capabilities": rules or [], "ttl_ms": 300000}), token)["credential_id"]
                    clients[name] = save(name + "-client.json", {"endpoint": f"https://localhost:{port}/v1/operations", "ca_file": str(temp / "ca.pem"), "credential": {"type": "file", "path": token}, "timeout_ms": 20000})

                for role in ["definition_maintainer", "runner", "scheduler", "viewer"]:
                    issue(role, role)
                for actor in ["reviewer", "incident-commander"]:
                    issue(actor, "approver")

            def remote(role, operation, code=0):
                reply = run("remote", "call", clients[role], save("request.json", {"protocol_version": 1, "request_id": "acceptance-case", "operation": operation}), code=code)
                return reply["value"] if code == 0 else reply

            completed = []
            for case in ["pass", "workspace-pass", "approved", "missing", "fake-report", "old-target", "expired", "provider-change", "dirty-workspace", "exception"]:
                git("checkout", "--detach", base_revision)
                # Restore only files owned by this fixture between matrix cases.
                (source / "candidate.json").write_text(document)
                if case in ["fake-report", "exception"]:
                    (source / "candidate.json").write_text("{}")
                    git("add", "candidate.json")
                    git("commit", "-m", "invalid candidate for failure and exception contracts")
                revision = git("rev-parse", "HEAD")
                start = json.loads((ROOT / "examples/gates/protected-release.json").read_text())
                bundle, inputs = start["bundle"], start["inputs"]
                flow = bundle["workflows"][0]
                flow["id"] = "release-" + case
                bundle["root"]["id"] = flow["id"]
                start.update(run_id=flow["id"], started_at_unix_ms=now())
                checker = bundle["capabilities"][0]
                checker["capability"] = {"id": "fixture.source-validate", "version": "1.0.0"}
                checker["skill"] = None
                checker["usage"] = "Fixture worker invokes the real validator on a Git-bound definition and returns its typed result."
                field = {"required": True, "value_type": {"type": "string"}}
                for name, value in [("repository", "release-fixture"), ("revision", revision)]:
                    flow["inputs"][name] = checker["inputs"][name] = copy.deepcopy(field)
                    flow["nodes"][0]["inputs"][name] = copy.deepcopy(field)
                    flow["nodes"][0]["bindings"][name] = {"source": "workflow_input", "field": name}
                    inputs[name] = value
                flow["nodes"][0]["kind"]["capability"] = checker["capability"]
                inputs["document"] = git("show", revision + ":candidate.json")
                task_inputs = {name: inputs[name] for name in checker["inputs"]}
                subject = {"source_revision": {"repository": inputs["repository"], "revision": revision}, "input_digest": digest(task_inputs), "artifacts": []}
                inputs["delivery"] = copy.deepcopy(subject)
                if case == "old-target":
                    inputs["delivery"]["source_revision"]["revision"] = "b" * 40
                for gate in bundle["postconditions"]:
                    gate["workflow"] = bundle["root"]
                    gate["policy"]["identity"]["id"] = "checks-" + case
                    gate["repository"]["value"] = inputs["repository"]
                    gate["revision"]["value"] = revision
                    requirement = gate["policy"]["requirements"][0]
                    requirement.update(capability=checker["capability"], contract_digest=digest(checker), max_age_ms=3000 if case == "expired" else 120000)
                effect = bundle["effect_bindings"][0]
                effect["workflow"] = bundle["root"]
                if case in ["workspace-pass", "dirty-workspace"]:
                    effect["release"]["target_check"] = {"type": "observe_then_reconcile", "remaining_race": "Local file observation and provider write cannot share one atomic transaction; query the operation key after ambiguity."}
                if case in ["approved", "exception"]:
                    gate = bundle["postconditions"][0]
                    target = {"run_id": start["run_id"], "run_digest": "sha256:" + "a" * 64, "action": gate["action"], **subject}
                    inputs["review_digest"] = run("gate", "review-digest", save("review-request.json", {"policy": gate["policy"], "target": target, "evidence": []}))["result"]["review_digest"]
                    flow["inputs"]["review_digest"] = copy.deepcopy(field)
                    flow["entry"] = "review"
                    flow["nodes"] += [{"id": "review", "kind": {"type": "wait", "event": "review", "timeout_ms": 120000}, "inputs": {"review_digest": field}, "bindings": {"review_digest": {"source": "workflow_input", "field": "review_digest"}}}, {"id": "declined", "kind": {"type": "terminal", "outcome": "failed"}}]
                    for route, to in [("accepted", "inspect"), ("rejected", "declined"), ("timed_out", "declined")]:
                        flow["edges"].append({"id": "review-" + route, "from": "review", "to": to, "route": {"type": route}})
                    bundle["wait_policies"] = [{"workflow": bundle["root"], "node_id": "review", "policy": {"identity": {"id": "review-" + case, "version": "1.0.0"}, "kind": "human_approval", "responders": ["reviewer"], "subjects": {"review_digest": "digest"}, "max_validity_ms": 120000,
                                               "exception": {"identity": {"id": "incident", "version": "1.0.0"}, "responders": ["incident-commander"], "codes": ["restore-service"]}}}]
                    approval = {"node_id": "review", "subject_field": "review_digest"}
                    if case == "exception":
                        for gate in bundle["postconditions"]:
                            gate["exception"] = approval
                    else:
                        effect["release"]["approvals"] = [{"gate_node": "inspect", "approval": approval}]
                run("kernel", "check", save("bundle.json", bundle))
                artifacts, db = temp / (case + "-artifacts"), temp / (case + ".db")

                def local(operation, *rest, code=0):
                    return run("run", "--artifacts", artifacts, operation, db, *rest, code=code)

                if args.https:
                    rules = []
                    for cap in [checker, bundle["capabilities"][1]]:
                        rule = {**cap["capability"], "contract_digest": digest(cap)}
                        if cap == checker:
                            rule["artifacts"] = {"inputs": {}, "output": {"types": [bundle["postconditions"][0]["policy"]["requirements"][0]["report_type"]], "repository_input": "repository", "revision_input": "revision"}}
                        else:
                            rule["effect"] = {"policy": effect["policy"]}
                        rules.append(rule)
                    issue("worker-" + case, "worker", rules)
                    worker = "worker-" + case
                    remote("definition_maintainer", {"type": "publish", "bundle": bundle})
                    state = remote("runner", {"type": "start", "request": start})["snapshot"]
                else:
                    run("artifact", "init", artifacts)
                    run("run", "init", db)
                    state = local("start", save("start.json", start))["result"]["snapshot"]
                if case in ["approved", "exception"]:
                    wait = (remote("viewer", {"type": "waits", "run_id": start["run_id"], "after": 0, "limit": 100}) if args.https else local("waits", start["run_id"], 0, 100)["result"])["items"][0]
                    actor = "incident-commander" if case == "exception" else "reviewer"
                    message = {"schema_version": 1, "message_id": "reviewed", "source": actor, "target": wait["target"], "correlation_id": wait["correlation_id"], "decision": "approve", "reason": "Reviewed this delivery; incident scope requires service restoration", "outputs": {}, "expires_at_unix_ms": now() + 100000}
                    if case == "exception":
                        message["exception"] = {"policy": {"id": "incident", "version": "1.0.0"}, "code": "restore-service"}
                    submission = {"schema_version": 1, "run_id": start["run_id"], "run_digest": state["run_digest"], "message": message}
                    if args.https:
                        remote(actor, {"type": "approve", "request": submission})
                    else:
                        local("receive", save("signal.json", submission))
                if args.https:
                    lease = remote("scheduler", {"type": "acquire", "run_id": start["run_id"], "acquisition_id": case, "ttl_ms": 120000})
                    for _ in range(8):
                        dispatched = remote("scheduler", {"type": "dispatch", "lease": lease, "worker_id": credentials[worker]})
                        if dispatched["type"] == "task":
                            assignment = dispatched["assignment_id"]
                            task = remote(worker, {"type": "assignment", "assignment_id": assignment})
                            break
                    else:
                        raise AssertionError("task not dispatched")
                else:
                    lease = local("acquire", save("lease-request.json", {"run_id": start["run_id"], "owner": "fixture", "acquisition_id": case, "ttl_ms": 120000}))["result"]
                    for _ in range(8):
                        dispatched = local("claim", save("lease.json", lease))["result"]
                        if dispatched["type"] == "task":
                            task = dispatched["attempt"]
                            break
                    else:
                        raise AssertionError("task not claimed")
                # Real validator result; report text cannot replace its Boolean.
                validation = run("validate", save("candidate.json", task["request"]["inputs"]["document"]), code=1 if case in ["fake-report", "exception"] else 0)
                valid = validation["valid"]
                evidence = []
                if case != "missing":
                    report_type = bundle["postconditions"][0]["policy"]["requirements"][0]["report_type"]
                    payload = save("report.json", {"valid": True})
                    if args.https:
                        reference = run("remote", "artifact-upload", clients[worker], assignment, "report", save("type.json", report_type), payload)
                    else:
                        spec = run("artifact", "prepare", save("task-request.json", task["request"]), save("type.json", report_type), save("source.json", subject["source_revision"]), save("refs.json", []))["result"]
                        reference = run("artifact", "put", artifacts, save("spec.json", spec), payload)["result"]
                    evidence.append({k: reference[k] for k in ["artifact_id", "digest"]})
                result = {"protocol_version": task["request"]["protocol_version"], "request_digest": digest(task["request"]), "completed_at_unix_ms": now(), "outcome": {"status": "succeeded", "outputs": {"valid": valid, "diagnostics": []}, "evidence": evidence}}
                if args.https:
                    remote(worker, {"type": "finish", "assignment_id": assignment, "result": result})
                    remote("scheduler", {"type": "dispatch", "lease": lease, "worker_id": credentials[worker]})
                else:
                    local("finish", save("lease.json", lease), task["attempt_id"], save("result.json", result))
                    local("claim", save("lease.json", lease))
                if case == "expired":
                    time.sleep(3.1)
                if case == "provider-change":
                    with Gateway.lock:
                        (source / "next.txt").write_text("new untested source")
                        git("add", "next.txt")
                        git("commit", "-m", "untested candidate")
                if case == "dirty-workspace":
                    (source / "candidate.json").write_text("uncommitted change")
                    git("update-index", "--assume-unchanged", "candidate.json")
                Gateway.subject = subject
                before = Gateway.writes
                binding = {"schema_version": 1, "target": effect["policy"]["target"], "call_identity": effect["policy"]["call_identity"], "capability": bundle["capabilities"][1], "endpoint": f"http://127.0.0.1:{gateway.server_port}", "api_key_env": "WORKFLOW_RELEASE_FIXTURE_KEY", "allow_loopback_http": True}
                if case in ["workspace-pass", "dirty-workspace"]:
                    binding["workspace"] = {"repository": subject["source_revision"]["repository"], "path": str(source)}
                bindings = save("bindings.json", [binding])
                if args.https:
                    dispatched = remote("scheduler", {"type": "dispatch_effect", "lease": lease, "worker_id": credentials[worker]}, code=2 if case in ["old-target", "expired"] else 0)
                    if case not in ["old-target", "expired", "missing", "fake-report"]:
                        assert dispatched["type"] == "call", dispatched
                        run("remote", "work-effects", clients[worker], bindings, 1, 20)
                        if case in ["pass", "workspace-pass", "approved", "exception"]:
                            remote("scheduler", {"type": "dispatch", "lease": lease, "worker_id": credentials[worker]})
                    manifest = remote("viewer", {"type": "acceptance", "run_id": start["run_id"]})
                    output = temp / (case + "-acceptance.json")
                    run("remote", "acceptance", clients["viewer"], start["run_id"], output)
                    assert json.loads(output.read_text()) == manifest
                    assert output.stat().st_mode & 0o777 == 0o600
                else:
                    local("release", save("lease.json", lease))
                    local("drive-effects", start["run_id"], "effect-driver", 10, bindings, code=1 if case in ["old-target", "expired"] else 0)
                    manifest = local("acceptance", start["run_id"])["result"]
                passed = case in ["pass", "workspace-pass", "approved", "exception"]
                assert Gateway.writes - before == int(passed), case
                assert manifest["status"] == ("accepted_with_exceptions" if case == "exception" else "accepted" if passed else "incomplete"), (case, manifest["status"])
                assert digest({k: v for k, v in manifest.items() if k != "digest"}) == manifest["digest"]
                if case == "exception":
                    assert len(manifest["approvals"]) == 1
                    assert all(c["evaluation"]["decision"]["verdict"] == "FAIL" and c["evaluation"]["exception"]["actor"] == "incident-commander" for c in manifest["checks"])
                if case == "dirty-workspace":
                    git("update-index", "--no-assume-unchanged", "candidate.json")
                completed.append(case)
            print(json.dumps({"status": "pass", "https": args.https, "cases": completed, "provider_writes": Gateway.writes, "unauthorized_writes": 0}))
        finally:
            gateway.shutdown()
            gateway.server_close()
            if child:
                if child.poll() is None:
                    child.send_signal(signal.SIGTERM)
                child.communicate(timeout=20)


if __name__ == "__main__":
    main()
