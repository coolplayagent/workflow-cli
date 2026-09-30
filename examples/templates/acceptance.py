#!/usr/bin/env python3
"""Execute the same three SDLC definitions locally and through TLS/PostgreSQL.

The adapters run real Python tests in isolated directories and persist typed
workspace/report artifacts. Delivery uses a disposable provider with durable
idempotency receipts. No public repository or production release is changed.
Full mode requires WORKFLOW_TEST_POSTGRES and takes at least five minutes for
the definitions' actual durable approval deadlines; no shortened test versions.
"""
import argparse
import copy
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Lock, Thread

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("template_generator", Path(__file__).with_name("generate.py"))
definitions = importlib.util.module_from_spec(spec)
spec.loader.exec_module(definitions)
digest = definitions.digest


def now():
    return int(time.time() * 1000)


class Provider(BaseHTTPRequestHandler):
    lock = Lock()
    receipts, subjects, scenarios = {}, {}, {}
    writes, compensations = 0, 0

    def log_message(self, *_):
        pass

    def do_POST(self):
        attempt = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        cls, intent = type(self), attempt["intent"]
        assert self.headers["Authorization"] == "Bearer template-fixture-key"
        key = intent["operation_key"]
        with cls.lock:
            if key in cls.receipts:
                observation = {"status": "applied", "receipt": cls.receipts[key]}
            elif attempt["kind"] == "query":
                observation = {"status": "absent"}
            else:
                release = None
                if "compensates" in intent:
                    original = intent["compensates"]["receipt"]
                    assert cls.receipts[intent["compensates"]["operation_key"]] == original
                    resource = original["resource_id"]
                    assert intent["inputs"]["resource_id"] == resource
                    (cls.directory / (resource + ".deployment")).unlink()
                    outputs = {"rolled_back": True}
                    cls.compensations += 1
                else:
                    actual = copy.deepcopy(cls.subjects[intent["run_id"]])
                    actual["source_revision"]["revision"] = cls.git("rev-parse", "HEAD")
                    assert actual == intent["release"]["subject"]
                    assert now() < attempt["deadline_unix_ms"]
                    resource = "delivery-" + key[7:23]
                    outputs = {"resource_id": resource}
                    release = {"subject": actual, "target_check": intent["release"]["policy"]["target_check"], "authorization_digest": digest(attempt["release"]), "observed_at_unix_ms": now()}
                    if intent["capability"]["capability"]["id"] == "delivery.publish":
                        (cls.directory / (resource + ".deployment")).write_text("unhealthy" if cls.scenarios[intent["run_id"]] == "failed" else "healthy")
                    else:
                        (cls.directory / (resource + ".pr")).write_text(json.dumps({"state": "open", "merged": False, "subject": actual}))
                    cls.writes += 1
                receipt = {"operation_key": key, "intent_digest": digest(intent), "target": intent["policy"]["target"], "resource_id": resource, "provider_receipt": "fixture-" + key[7:], "outputs": outputs}
                if release:
                    receipt["release"] = release
                path = cls.directory / (key[7:] + ".receipt")
                with path.open("x") as handle:
                    json.dump(receipt, handle)
                    handle.flush()
                    os.fsync(handle.fileno())
                cls.receipts[key] = json.loads(path.read_text())
                observation = {"status": "applied", "receipt": cls.receipts[key]}
        data = json.dumps({"request_digest": digest(attempt), "observation": observation}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Harness:
    def __init__(self, binary, directory, output):
        self.binary, self.temp, self.output = binary, directory, output
        self.clients, self.credentials = {}, {}
        self.serial = 0
        self.child = None
        self.source = directory / "source"
        self.source.mkdir()
        self.git("init", "--initial-branch=main")
        (self.source / "calc.py").write_text("def add(a, b):\n    return a - b\n")
        self.git("add", ".")
        self.git("commit", "-m", "reproducible defect baseline")
        self.revision = self.git("rev-parse", "HEAD")
        Provider.directory, Provider.git = directory, self.git
        self.gateway = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        Thread(target=self.gateway.serve_forever, daemon=True).start()
        os.environ["WORKFLOW_TEMPLATE_FIXTURE_KEY"] = "template-fixture-key"

    def git(self, *command):
        return subprocess.check_output(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", *command], cwd=self.source, stderr=subprocess.DEVNULL, text=True).strip()

    def save(self, value, private=False):
        self.serial += 1
        path = self.temp / ("input-" + str(self.serial) + ".json")
        path.write_text(value if isinstance(value, str) else json.dumps(value))
        if private:
            path.chmod(0o600)
        return str(path)

    def run(self, *command, code=0):
        result = subprocess.run([str(self.binary), *map(str, command)], cwd=ROOT, capture_output=True, text=True, timeout=60)
        assert result.returncode == code, (command[:3], result.returncode, code, result.stdout, result.stderr)
        return json.loads(result.stdout or result.stderr)

    def remote(self, actor, operation, code=0):
        result = self.run("remote", "call", self.clients[actor], self.save({"protocol_version": 1, "request_id": "template-acceptance", "operation": operation}), code=code)
        return result.get("value", result) if code == 0 else result

    def issue(self, name, role, rules=None):
        path = self.temp / (name + ".token")
        provision = self.save({"actor": name, "role": role, "capabilities": rules or [], "ttl_ms": 3600000})
        self.credentials[name] = self.run("service", "issue", self.server, self.admin, provision, path)["credential_id"]
        self.clients[name] = self.save({"endpoint": self.endpoint, "ca_file": str(self.temp / "ca.pem"), "credential": {"type": "file", "path": str(path)}, "timeout_ms": 30000})

    def shared(self, templates):
        def openssl(*args):
            subprocess.run(["openssl", *map(str, args)], check=True, capture_output=True, timeout=20)
        temp = self.temp
        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", temp / "key.pem", "-out", temp / "ca.pem", "-days", "1", "-subj", "/CN=localhost")
        openssl("req", "-new", "-key", temp / "key.pem", "-out", temp / "server.csr", "-subj", "/CN=localhost")
        extension = self.save("subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
        openssl("x509", "-req", "-in", temp / "server.csr", "-CA", temp / "ca.pem", "-CAkey", temp / "key.pem", "-CAcreateserial", "-out", temp / "server.pem", "-days", "1", "-extfile", extension)
        (temp / "key.pem").chmod(0o600)
        self.server = self.save({"listen": "127.0.0.1:0", "certificate_file": str(temp / "server.pem"), "private_key": {"type": "file", "path": str(temp / "key.pem")}, "database": {"connection": {"type": "file", "path": self.save(os.environ["WORKFLOW_TEST_POSTGRES"], True)}, "transport": {"type": "local"}}, "max_connections": 8, "max_operations": 4})
        self.run("service", "init-artifacts", self.server)
        self.run("service", "init-effects", self.server)
        self.tenant = "templates-" + uuid.uuid4().hex
        self.run("service", "bootstrap", self.server, self.tenant, "project", "operator", temp / "admin.token")
        self.admin = self.save({"type": "file", "path": str(temp / "admin.token")})
        self.child = subprocess.Popen([str(self.binary), "service", "serve", self.server], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        with selectors.DefaultSelector() as selector:
            selector.register(self.child.stdout, selectors.EVENT_READ)
            assert selector.select(20), "TLS service did not bind"
        port = json.loads(self.child.stdout.readline())["bound_address"].rsplit(":", 1)[1]
        self.endpoint = f"https://localhost:{port}/v1/operations"
        for role in ["definition_maintainer", "runner", "scheduler", "viewer"]:
            self.issue(role, role)
        for name in ["process-owner", "delivery-reviewer", "unlisted-reviewer"]:
            self.issue(name, "approver")
        caps, effects = {}, {}
        for template in templates.values():
            bundle = template["bundle"]
            for c in bundle["capabilities"]:
                caps[c["capability"]["id"]] = c
            for e in bundle["effect_bindings"]:
                node = next(n for n in bundle["workflows"][0]["nodes"] if n["id"] == e["node_id"])
                effects[node["kind"]["capability"]["id"]] = e["policy"]
        rules = []
        for name, cap in caps.items():
            rule = {**cap["capability"], "contract_digest": digest(cap)}
            if name in effects:
                rule["effect"] = {"policy": effects[name]}
            elif name != "delivery.query":
                rule["artifacts"] = {"inputs": {k: definitions.WORKSPACE for k in ["context", "candidate"] if k in cap["inputs"]}, "output": {"types": [definitions.WORKSPACE, definitions.REPORT], "repository_input": "repository", "revision_input": "revision"}}
            rules.append(rule)
        self.issue("worker", "worker", rules)
        self.run("service", "configure-template-owners", self.server, self.save({"tenant": self.tenant, "project": "project", "expected_revision": None, "policy": {"owners": {"sdlc-owner": ["process-owner"]}}}))

    def close(self):
        self.gateway.shutdown()
        self.gateway.server_close()
        if self.child:
            self.child.send_signal(signal.SIGTERM)
            self.child.communicate(timeout=15)


class Case:
    def __init__(self, h, template, mode, scenario):
        self.h, self.template, self.mode, self.scenario = h, template, mode, scenario
        self.kind = template["identity"]["id"].split(".")[-1]
        self.id = f"{self.kind}-{mode}-{scenario}"
        self.rounds, self.artifacts = 0, []
        self.wait_deadline = None
        instance = json.loads((ROOT / f"examples/templates/{self.kind}-{mode}.json").read_text())
        instance.update(run_id=self.id, started_at_unix_ms=now())
        instance["parameters"]["revision"] = h.revision
        self.instance = instance
        before = Provider.writes
        self.plan = h.run("template", "plan", h.save(template), h.save(instance))
        assert Provider.writes == before
        self.store, self.db = h.temp / (self.id + "-artifacts"), h.temp / (self.id + ".db")
        if mode == "shared":
            h.remote("definition_maintainer", {"type": "publish", "bundle": template["bundle"]})
            self.snapshot = h.remote("runner", {"type": "start", "request": self.plan["request"]})["snapshot"]
        else:
            h.run("artifact", "init", self.store)
            h.run("run", "init", self.db)
            self.snapshot = self.local("start", h.save(self.plan["request"]))["snapshot"]
        Provider.scenarios[self.id] = scenario
        self.acquire()
        effects = template["bundle"]["effect_bindings"]
        self.bindings = h.save([{"schema_version": 1, "target": effects[0]["policy"]["target"], "call_identity": effects[0]["policy"]["call_identity"], "capability": c, "endpoint": f"http://127.0.0.1:{h.gateway.server_port}", "api_key_env": "WORKFLOW_TEMPLATE_FIXTURE_KEY", "allow_loopback_http": True} for c in template["bundle"]["capabilities"] if c["effects"]["type"] == "write"])

    def local(self, command, *args):
        return self.h.run("run", "--artifacts", self.store, command, self.db, *args)["result"]

    def acquire(self):
        request = {"run_id": self.id, "acquisition_id": "acquire-" + uuid.uuid4().hex, "ttl_ms": 120000}
        self.lease = self.h.remote("scheduler", {"type": "acquire", **request}) if self.mode == "shared" else self.local("acquire", self.h.save({**request, "owner": "fixture"}))

    def release(self):
        if self.mode == "shared":
            self.h.remote("scheduler", {"type": "release", "lease": self.lease})
        else:
            self.local("release", self.h.save(self.lease))

    def state(self):
        return self.h.remote("viewer", {"type": "get", "run_id": self.id}) if self.mode == "shared" else self.local("status", self.id)

    def upload(self, request, assignment, ty, payload, parents):
        h = self.h
        if self.mode == "shared":
            reference = h.run("remote", "artifact-upload", h.clients["worker"], assignment, "artifact-" + str(h.serial), h.save(ty), h.save(payload))
        else:
            source = {k: request["inputs"][k] for k in ["repository", "revision"]}
            spec = h.run("artifact", "prepare", h.save(request), h.save(ty), h.save(source), h.save(parents))["result"]
            reference = h.run("artifact", "put", self.store, h.save(spec), h.save(payload))["result"]
        link = {k: reference[k] for k in ["artifact_id", "digest"]}
        (h.output / (link["digest"][7:] + ".artifact.json")).write_text(json.dumps({"reference": reference, "payload": payload}, indent=2) + "\n")
        self.artifacts.append(link["digest"])
        return link

    def download(self, link, assignment):
        h = self.h
        path = h.temp / ("download-" + str(h.serial) + ".json")
        if self.mode == "shared":
            h.run("remote", "artifact-download", h.clients["worker"], h.save({"artifact": link, "assignment_id": assignment, "ttl_ms": 30000}), path)
        else:
            h.run("artifact", "export", self.store, link["artifact_id"], path)
        return json.loads(path.read_text())

    def execute(self, task, assignment=None):
        h, request = self.h, task["request"]
        cap, inputs = request["capability"]["id"], request["inputs"]
        evidence = []
        parents = [inputs[k] for k in ["context", "candidate"] if k in inputs]
        if cap in ["sdlc.diagnose", "sdlc.design", "sdlc.prepare-release"]:
            payload = {"purpose": cap, "files": json.dumps({"calc.py": h.git("show", h.revision + ":calc.py"), "design.md": "Implement integer addition; verify positive, negative and zero cases."}), "notes": inputs["request"]}
            link = self.upload(request, assignment, definitions.WORKSPACE, payload, [])
            outputs, evidence = {"context": link}, [link]
        elif cap == "sdlc.propose":
            context = self.download(inputs["context"], assignment)
            self.rounds += 1
            if self.rounds == 2:
                assert inputs["feedback"] and "AssertionError" in inputs["feedback"]
                assert context["purpose"] == "candidate"
            bad = (self.scenario == "rework" and self.rounds == 1) or (self.scenario == "failed" and self.kind != "release")
            files = json.loads(context["files"])
            files["calc.py"] = "def add(a, b):\n    return a " + ("-" if bad else "+") + " b\n"
            files["test_calc.py"] = "from calc import add\nassert add(2, 3) == 5\nassert add(-2, 3) == 1\nassert add(0, 0) == 0\nprint('3 candidate tests passed')\n"
            payload = {"purpose": "candidate", "files": json.dumps(files), "notes": inputs["feedback"] or "Initial implementation"}
            link = self.upload(request, assignment, definitions.WORKSPACE, payload, parents)
            outputs, evidence = {"candidate": link}, [link]
        else:
            workspace = self.download(inputs["candidate"], assignment)
            with tempfile.TemporaryDirectory(prefix="candidate-", dir=h.temp) as directory:
                folder = Path(directory)
                for name, text in json.loads(workspace["files"]).items():
                    assert Path(name).name == name
                    (folder / name).write_text(text)
                if cap == "delivery.health":
                    healthy = (Provider.directory / (inputs["resource_id"] + ".deployment")).read_text() == "healthy"
                    (folder / "health.txt").write_text("healthy" if healthy else "unhealthy")
                    (folder / "test_health.py").write_text("from pathlib import Path\nassert Path('health.txt').read_text() == 'healthy'\nprint('deployment smoke test passed')\n")
                    command = "test_health.py"
                else:
                    command = "test_calc.py"
                tested = subprocess.run([sys.executable, command], cwd=folder, text=True, capture_output=True, timeout=10)
            passed, diagnostics = tested.returncode == 0, tested.stdout + tested.stderr
            report = {"passed": passed, "command": "python3 " + command, "output": diagnostics}
            evidence.append(self.upload(request, assignment, definitions.REPORT, report, parents))
            outputs = {"passed": passed, "diagnostics": diagnostics}
            if cap in ["sdlc.verify", "delivery.health"]:
                outputs["target_artifacts"] = [inputs["candidate"]]
            if cap == "sdlc.verify":
                subject = {"source_revision": {k: inputs[k] for k in ["repository", "revision"]}, "input_digest": digest(inputs), "artifacts": [inputs["candidate"]]}
                target = {"run_id": self.id, "run_digest": self.snapshot["run_digest"], "action": json.loads(inputs["action_json"]), **subject}
                reviewed = h.run("gate", "review-digest", h.save({"policy": json.loads(inputs["gate_policy_json"]), "target": target, "evidence": []}))["result"]["review_digest"]
                outputs.update(delivery=subject, review_digest=reviewed)
                Provider.subjects[self.id] = subject
        result = {"protocol_version": request["protocol_version"], "request_digest": digest(request), "completed_at_unix_ms": now(), "outcome": {"status": "succeeded", "outputs": outputs, "evidence": evidence}}
        if self.mode == "shared":
            h.remote("worker", {"type": "finish", "assignment_id": assignment, "result": result})
        else:
            self.local("finish", h.save(self.lease), task["attempt_id"], h.save(result))

    def drive(self, expiring=False):
        h = self.h
        for _ in range(160):
            snapshot = self.state()
            if snapshot["status"] != "running":
                return self.complete(snapshot)
            managed = {e["node_id"] for e in self.template["bundle"]["effect_bindings"]}
            if any(name in managed and node["state"]["state"] == "task_ready" for frame in snapshot["frames"].values() if frame["workflow"] == self.template["bundle"]["root"] for name, node in frame["nodes"].items()):
                if self.mode == "shared":
                    effect = h.remote("scheduler", {"type": "dispatch_effect", "lease": self.lease, "worker_id": h.credentials["worker"]})
                    if effect["type"] == "call":
                        h.run("remote", "work-effects", h.clients["worker"], self.bindings, 1, 20)
                    elif effect["type"] == "idle":
                        # Approval can make the write ready while a preceding
                        # wait-delivery receipt still heads the durable outbox.
                        handled = h.remote("scheduler", {"type": "dispatch", "lease": self.lease, "worker_id": h.credentials["worker"]})
                        assert handled["type"] == "handled", (self.id, handled)
                    else:
                        assert effect["type"] == "handled", (self.id, effect)
                else:
                    self.release()
                    self.local("drive-effects", self.id, "fixture-effects", 1, self.bindings)
                    self.acquire()
                continue
            if self.mode == "shared":
                claimed = h.remote("scheduler", {"type": "dispatch", "lease": self.lease, "worker_id": h.credentials["worker"]})
            else:
                claimed = self.local("claim", h.save(self.lease))
            if claimed["type"] == "task":
                if self.mode == "shared":
                    assignment = claimed["assignment_id"]
                    self.execute(h.remote("worker", {"type": "assignment", "assignment_id": assignment}), assignment)
                else:
                    self.execute(claimed["attempt"])
                continue
            if claimed["type"] == "handled":
                continue
            waits = h.remote("viewer", {"type": "waits", "run_id": self.id, "after": 0, "limit": 100}) if self.mode == "shared" else self.local("waits", self.id, 0, 100)
            if waits["items"]:
                wait = waits["items"][0]
                if self.scenario == "timed_out":
                    if not expiring:
                        self.wait_deadline = wait["deadline_unix_ms"]
                        self.release()
                        return None
                    if self.mode == "shared":
                        h.remote("scheduler", {"type": "tick", "lease": self.lease})
                    else:
                        self.local("tick-due", h.save(self.lease))
                    continue
                message = {"schema_version": 1, "message_id": "delivery-reviewed", "source": "delivery-reviewer", "target": wait["target"], "correlation_id": wait["correlation_id"], "decision": "reject" if self.scenario == "rejected" else "approve", "reason": "Fixture reviewer inspected exact candidate and test reports", "outputs": {}, "expires_at_unix_ms": now() + 60000}
                submission = {"schema_version": 1, "run_id": self.id, "run_digest": self.snapshot["run_digest"], "message": message}
                if self.mode == "shared":
                    h.remote("delivery-reviewer", {"type": "approve", "request": submission})
                else:
                    self.local("receive", h.save(submission))
                continue
            raise AssertionError((self.id, "unexpected idle run", snapshot))
        raise AssertionError((self.id, "execution did not reach a bounded outcome"))

    def complete(self, snapshot):
        h = self.h
        accepted = self.scenario in ["success", "rework"]
        assert snapshot["status"] == ("succeeded" if accepted else "cancelled" if self.scenario == "rejected" else "failed"), (self.id, snapshot)
        manifest = h.remote("viewer", {"type": "acceptance", "run_id": self.id}) if self.mode == "shared" else self.local("acceptance", self.id)
        assert manifest["status"] == ("accepted" if accepted else "incomplete"), (self.id, manifest)
        assert manifest["bundle_digest"] == self.plan["bundle_digest"]
        assert digest({k: v for k, v in manifest.items() if k != "digest"}) == manifest["digest"]
        assert manifest["artifacts"] and self.artifacts
        if accepted:
            assert manifest["approvals"] and len(manifest["checks"]) >= 2
            assert self.rounds == (2 if self.scenario == "rework" else 1)
        if self.scenario in ["rejected", "timed_out"]:
            assert not manifest["effects"]
        if self.kind == "release" and self.scenario == "failed":
            assert len(manifest["effects"]) == 2
            assert any(e.get("compensated_by") for e in manifest["effects"])
        (h.output / (self.id + ".acceptance.json")).write_text(json.dumps(manifest, indent=2) + "\n")
        result = {"scenario": self.scenario, "mode": self.mode, "run_digest": manifest["run_digest"], "bundle_digest": manifest["bundle_digest"], "report_digest": manifest["digest"], "artifacts": sorted(set(self.artifacts)), "terminal_status": snapshot["status"], "accepted": accepted, "repair_rounds": self.rounds}
        print(json.dumps({"case": self.id, "status": "pass", "rounds": self.rounds, "artifacts": len(self.artifacts)}), flush=True)
        self.release()
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--local-only", action="store_true", help="Local development check; cannot satisfy publication regressions")
    parser.add_argument("--quick", action="store_true", help="Skip real deadline waits and publication; development only")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    templates = {k: json.loads((ROOT / f"examples/templates/{k}.json").read_text()) for k in ["defect", "feature", "release"]}
    with tempfile.TemporaryDirectory(prefix="workflow-r13-") as directory:
        h = Harness(args.binary.resolve(), Path(directory), args.output)
        try:
            if not args.local_only:
                h.shared(templates)
            results, pending = {k: [] for k in templates}, []
            modes = ["local"] if args.local_only else ["shared", "local"]
            if not args.quick:
                for mode in modes:
                    for template in templates.values():
                        case = Case(h, template, mode, "timed_out")
                        assert case.drive() is None
                        pending.append(case)
            for mode in modes:
                for kind, template in templates.items():
                    for scenario in ["success", "rework", "rejected", "failed"]:
                        case = Case(h, template, mode, scenario)
                        result = case.drive()
                        assert result is not None
                        results[kind].append(result)
            if pending:
                deadline = max(c.wait_deadline for c in pending)
                while now() <= deadline:
                    print(json.dumps({"waiting_for_real_approval_deadlines_ms": deadline - now()}), flush=True)
                    time.sleep(min(30, max(0.01, (deadline - now() + 100) / 1000)))
                for case in pending:
                    case.acquire()
                    results[case.kind].append(case.drive(expiring=True))
            if not args.quick and not args.local_only:
                catalog = h.temp / "templates.db"
                h.run("template", "init", catalog, ROOT / "examples/templates/owners.json")
                for kind, template in templates.items():
                    validated = h.run("template", "validate", h.save(template))
                    candidate = {"template": template, "proposed_by": "definition_maintainer", "reason": "Publish portable SDLC fixture after executing the complete local and TLS regression matrix", "compatibility": "Initial immutable template and shared implementation-attempt 1.0.0; existing runs retain their bundle", "regressions": {"template_digest": validated["template_digest"], "cases": results[kind]}}
                    candidate_path = args.output / (kind + ".candidate.json")
                    candidate_path.write_text(json.dumps(candidate, indent=2) + "\n")
                    key = h.run("template", "propose", catalog, candidate_path, "definition_maintainer")["candidate_digest"]
                    h.run("template", "publish", catalog, key, code=1)
                    h.run("template", "review", catalog, key, "definition_maintainer", "approve", "Self approval must fail", code=1)
                    h.run("template", "review", catalog, key, "process-owner", "approve", "Fixture owner reviewed all ten real reports")
                    publication = h.run("template", "publish", catalog, key)
                    shared_key = h.remote("definition_maintainer", {"type": "template_propose", "candidate": candidate})
                    assert shared_key == key
                    h.remote("definition_maintainer", {"type": "template_publish", "digest": key}, code=2)
                    h.remote("unlisted-reviewer", {"type": "template_review", "digest": key, "decision": "approve", "reason": "Unlisted owner must fail"}, code=2)
                    h.remote("process-owner", {"type": "template_review", "digest": key, "decision": "approve", "reason": "Fixture owner reviewed all ten real reports"})
                    shared_publication = h.remote("definition_maintainer", {"type": "template_publish", "digest": key})
                    assert shared_publication["candidate"] == publication["candidate"]
                    instance = json.loads((ROOT / f"examples/templates/{kind}-shared.json").read_text())
                    instantiated = h.run("remote", "template-plan", h.clients["runner"], template["identity"]["id"], "1.0.0", h.save(instance))
                    assert instantiated["publication_digest"] == shared_publication["digest"]
                    (args.output / (kind + ".publication.json")).write_text(json.dumps(shared_publication, indent=2) + "\n")
            summary = {"status": "pass", "complete_matrix": not args.quick and not args.local_only, "cases": sum(len(v) for v in results.values()), "provider_writes": Provider.writes, "compensations": Provider.compensations}
            (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
            print(json.dumps(summary), flush=True)
        finally:
            h.close()


if __name__ == "__main__":
    main()
