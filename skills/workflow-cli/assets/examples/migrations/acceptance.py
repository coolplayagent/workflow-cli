#!/usr/bin/env python3
"""R11 real CLI sessions: locked versions, reviewed migration, storage recovery.

--https additionally requires disposable WORKFLOW_TEST_POSTGRES and OpenSSL.
--legacy-binary tests an actual retained schema-10/11 CLI for backup recovery; otherwise
the storage fixture reconstructs a legacy replay checkpoint and removes newer journals.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import sqlite3
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--https", action="store_true")
    parser.add_argument("--legacy-binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    legacy = args.legacy_binary.resolve() if args.legacy_binary else None
    with tempfile.TemporaryDirectory(prefix="workflow-r11-") as directory:
        temp = Path(directory)

        def save(name, value, private=False):
            path = temp / name
            path.write_text(value if isinstance(value, str) else json.dumps(value))
            if private:
                path.chmod(0o600)
            return str(path)

        def run(*command, code=0, executable=binary):
            p = subprocess.run([str(executable), *map(str, command)], cwd=ROOT,
                               capture_output=True, text=True, timeout=40)
            assert p.returncode == code, (command[:2], p.returncode, code)
            return json.loads(p.stdout or p.stderr)

        def local(*command, **kw):
            return run("run", *command, **kw)["result"]

        def fixture(version, run_id):
            r = json.loads((ROOT / f"examples/migrations/v{version}-start.json").read_text())
            r.update(run_id=run_id, started_at_unix_ms=int(time.time() * 1000))
            return r

        def message(state, wait, message_id):
            return dict(schema_version=1, run_id=state["run_id"], run_digest=state["run_digest"],
                        message=dict(schema_version=1, message_id=message_id, source="reviewer",
                                     target=wait["target"], correlation_id=wait["correlation_id"],
                                     decision="approve", reason="Reviewed exact frozen subject",
                                     outputs={}, expires_at_unix_ms=wait["deadline_unix_ms"]))

        def request(target):
            return dict(migration_id="reviewed-v2", target_bundle=target["bundle"],
                        target_inputs=target["inputs"], execution_policy="restart_with_fresh_evidence",
                        timer_policy="cancel_and_rearm_on_resume", node_mapping=[],
                        decision_summary="Remove inspection, replace review subject and rearm approval")

        db = temp / "runs.sqlite"
        registry = temp / "definitions.sqlite"
        v1, v2 = fixture(1, "old-run"), fixture(2, "new-run")
        local("init", db)
        run("draft", "create", registry, "v1", save("definition-v1.json", v1["bundle"]["workflows"][0]))
        run("draft", "publish", registry, "v1", 1)
        old = local("start", db, save("v1.json", v1))["snapshot"]
        run("draft", "create", registry, "v2", save("definition-v2.json", v2["bundle"]["workflows"][0]))
        run("draft", "publish", registry, "v2", 1)
        assert local("status", db, v1["run_id"]) == old
        new = local("start", db, save("v2.json", v2))["snapshot"]
        assert old["bundle_digest"] != new["bundle_digest"]
        wait = local("waits", db, v1["run_id"], 0, 100)["items"][0]
        local("receive", db, save("approval.json", message(old, wait, "old-approved")))
        driven = local("drive", db, v1["run_id"], "compatible-worker", 100)
        assert driven["executed_tasks"] == 1 and driven["snapshot"]["status"] == "succeeded"
        assert local("history-at", db, v1["run_id"], old["revision"]) == old

        migrating = fixture(1, "migrating")
        old = local("start", db, save("migrating.json", migrating))["snapshot"]
        wait = local("waits", db, "migrating", 0, 100)["items"][0]
        local("pause", db, "migrating", "pause", old["revision"], int(time.time() * 1000), "review upgrade")
        queued = local("receive", db, save("queued.json", message(old, wait, "old-pending")))
        assert queued["entry"]["status"]["status"] == "pending"
        before = local("status", db, "migrating")
        plan = local("migration-plan", db, "migrating", save("migration-request.json", request(v2)))
        assert plan["inputs_changed"] and plan["invalidated_messages"] == ["old-pending"]
        assert any(n["target"] is None for n in plan["nodes"]) and plan["timers"]
        lease = local("acquire", db, save("lease-request.json", dict(run_id="migrating", owner="operator", acquisition_id="upgrade", ttl_ms=120000)))
        lease_file, plan_file = save("lease.json", lease), save("plan.json", plan)
        migrated = local("migration-apply", db, lease_file, plan_file, "operator")
        assert migrated["snapshot"]["pause"] and migrated["snapshot"]["bundle_digest"] == new["bundle_digest"]
        assert local("migration-apply", db, lease_file, plan_file, "operator")["transition"]["duplicate"]
        assert local("history-at", db, "migrating", before["revision"]) == before
        inbox = local("inbox", db, "migrating", 0, 100)["items"]
        assert inbox[0]["status"]["reason"] == "definition_mismatch"
        local("resume", db, "migrating", "resume", migrated["snapshot"]["revision"], int(time.time() * 1000), "review accepted")
        state = local("status", db, "migrating")
        wait2 = local("waits", db, "migrating", 0, 100)["items"][0]
        assert wait2["target"] != wait["target"] and wait2["subjects"] == v2["inputs"]
        stale = local("receive", db, save("stale.json", message(state, wait, "stale-new-id")))
        assert stale["entry"]["status"]["status"] == "rejected"
        local("receive", db, save("new-approved.json", message(state, wait2, "new-approved")))
        assert local("status", db, "migrating")["status"] == "succeeded"
        local("verify", db, "migrating")

        storage = temp / "old-storage.sqlite"
        executable = legacy or binary
        local("init", storage, executable=executable)
        storage_start = fixture(1, "storage")
        stored = local("start", storage, save("storage-start.json", storage_start), executable=executable)["snapshot"]
        if legacy is None:
            with sqlite3.connect(storage) as c:
                checkpoint = run("kernel", "replay", save("legacy-scenario.json", dict({k: v for k, v in storage_start.items() if k != "schema_version"}, events=[])))["checkpoint"]
                document = json.dumps(checkpoint, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
                digest = "sha256:" + hashlib.sha256(document.encode()).hexdigest()
                c.execute("INSERT INTO checkpoints VALUES (?,1,?,?)", ("storage", document, digest))
                c.executescript("DROP TABLE state_checkpoints; DROP TABLE storage_migrations; PRAGMA user_version=10;")
        with sqlite3.connect(storage) as c:
            source_version = c.execute("PRAGMA user_version").fetchone()[0]
        preview = local("storage-plan", storage)
        backup = temp / "pre-upgrade.sqlite"
        receipt = local("migrate", storage, backup)
        assert receipt == preview and local("status", storage, "storage") == stored
        restored = temp / "restored.sqlite"
        local("storage-restore", backup, restored, save("storage-plan.json", preview))
        with sqlite3.connect(restored) as c:
            assert c.execute("PRAGMA user_version").fetchone()[0] == source_version
        if legacy:
            assert local("status", restored, "storage", executable=legacy) == stored
            local("verify", restored, "storage", executable=legacy)
        # Corruption is rejected before schema conversion; the source version
        # and retained verified backup provide an actual recovery path.
        with sqlite3.connect(restored) as c:
            c.execute("UPDATE heads SET revision=revision+1")
        run("run", "migrate", restored, temp / "invalid-backup.sqlite", code=1)
        with sqlite3.connect(restored) as c:
            assert c.execute("PRAGMA user_version").fetchone()[0] == source_version
        recovered = temp / "recovered.sqlite"
        local("storage-restore", backup, recovered, save("storage-plan.json", preview))
        if legacy:
            assert local("status", recovered, "storage", executable=legacy) == stored
        else:
            local("migrate", recovered, temp / "second-backup.sqlite")
            assert local("status", recovered, "storage") == stored

        if args.https:
            https(binary, temp, save, run, fixture, message, request)
    print(json.dumps(dict(status="pass", actual_v1_tasks=1, publish_keeps_old_run=True,
                         reviewed_migration=True, old_approval_rejected=True,
                         storage_backup_restored=True, retained_binary_verified=bool(legacy),
                         https=args.https, binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest()), indent=2))


def https(binary, temp, save, run, fixture, message, migration_request):
    def openssl(*argv):
        subprocess.run(["openssl", *map(str, argv)], check=True, capture_output=True, timeout=20)
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", temp / "key.pem",
            "-out", temp / "ca.pem", "-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost")
    openssl("req", "-new", "-key", temp / "key.pem", "-out", temp / "server.csr", "-subj", "/CN=localhost")
    ext = save("server.ext", "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
    openssl("x509", "-req", "-in", temp / "server.csr", "-CA", temp / "ca.pem", "-CAkey", temp / "key.pem",
            "-CAcreateserial", "-out", temp / "server.pem", "-days", "1", "-extfile", ext)
    (temp / "key.pem").chmod(0o600)
    database = save("database", os.environ["WORKFLOW_TEST_POSTGRES"], private=True)
    server = save("server.json", dict(listen="127.0.0.1:0", certificate_file=str(temp / "server.pem"),
                  private_key=dict(type="file", path=str(temp / "key.pem")),
                  database=dict(connection=dict(type="file", path=database), transport=dict(type="local")),
                  max_connections=8, max_operations=4))
    admin = str(temp / "admin")
    run("service", "bootstrap", server, "migration-" + uuid.uuid4().hex, "project", "operator", admin)
    admin_ref = save("admin-ref.json", dict(type="file", path=admin))
    tokens = {"administrator": admin}
    for role in ["definition_maintainer", "runner", "approver"]:
        tokens[role] = str(temp / role)
        run("service", "issue", server, admin_ref,
            save(role + ".json", dict(actor="reviewer" if role == "approver" else role, role=role, capabilities=[], ttl_ms=300000)), tokens[role])
    child = subprocess.Popen([str(binary), "service", "serve", server], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            assert selector.select(20), "service bind timeout"
        port = json.loads(child.stdout.readline())["bound_address"].rsplit(":", 1)[1]
        clients = {role: save(role + "-client.json", dict(endpoint=f"https://localhost:{port}/v1/operations", ca_file=str(temp / "ca.pem"),
                   credential=dict(type="file", path=token), timeout_ms=20000)) for role, token in tokens.items()}
        def call(role, operation, code=0):
            response = run("remote", "call", clients[role], save("remote-request.json", dict(protocol_version=1, request_id="migration", operation=operation)), code=code)
            return response.get("value") if code == 0 else response
        v1, v2 = fixture(1, "remote-upgrade"), fixture(2, "remote-new")
        call("definition_maintainer", dict(type="publish", bundle=v1["bundle"]))
        old = call("runner", dict(type="start", request=v1))["snapshot"]
        call("definition_maintainer", dict(type="publish", bundle=v2["bundle"]))
        assert call("runner", dict(type="get", run_id=v1["run_id"])) == old
        new = call("runner", dict(type="start", request=v2))["snapshot"]
        assert new["bundle_digest"] != old["bundle_digest"]
        old_wait = call("runner", dict(type="waits", run_id=v1["run_id"], after=0, limit=100))["items"][0]
        paused = call("runner", dict(type="control", request=dict(run_id=v1["run_id"], event_id="pause", expected_revision=old["revision"], control=dict(type="pause", reason="review upgrade"))))["snapshot"]
        operation = dict(type="plan_migration", run_id=v1["run_id"], request=migration_request(v2))
        call("runner", operation, code=2)
        plan = call("administrator", operation)
        lease = call("administrator", dict(type="acquire", run_id=v1["run_id"], acquisition_id="admin-upgrade", ttl_ms=120000))
        operation = dict(type="migrate_definition", lease=lease, plan=plan)
        upgraded = call("administrator", operation)
        assert call("administrator", operation)["transition"]["duplicate"]
        assert call("administrator", dict(type="historical_snapshot", run_id=v1["run_id"], revision=paused["revision"])) == paused
        state = call("runner", dict(type="control", request=dict(run_id=v1["run_id"], event_id="resume", expected_revision=upgraded["snapshot"]["revision"], control=dict(type="resume", reason="review complete"))))["snapshot"]
        call("approver", dict(type="approve", request=message(state, old_wait, "old-version")), code=2)
        wait = call("runner", dict(type="waits", run_id=v1["run_id"], after=0, limit=100))["items"][0]
        call("approver", dict(type="approve", request=message(state, wait, "new-version")))
        assert call("runner", dict(type="get", run_id=v1["run_id"]))["status"] == "succeeded"
        child.send_signal(signal.SIGTERM)
        out, err = child.communicate(timeout=20)
        assert child.returncode == 0 and json.loads(out)["stopped"]
        assert all(Path(token).read_text() not in out + err for token in tokens.values())
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()


if __name__ == "__main__":
    main()
