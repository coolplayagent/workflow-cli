#!/usr/bin/env python3
"""Reproduce portable, reviewable SDLC definitions; never executes their tasks."""
import copy
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def digest(value):
    return "sha256:" + hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()


def ref(name, version="1.0.0"):
    return {"id": name, "version": version}


S, B = {"type": "string"}, {"type": "boolean"}
LINK = {"type": "object", "fields": {"artifact_id": S, "digest": S}}
LINKS = {"type": "array", "items": LINK}
SUBJECT = {"type": "object", "fields": {"source_revision": {"type": "object", "fields": {"repository": S, "revision": S}}, "input_digest": S, "artifacts": LINKS}}
WORKSPACE = {"identity": ref("sdlc.workspace"), "content": {"format": "json", "value_type": {"type": "object", "fields": {"purpose": S, "files": S, "notes": S}}}}
REPORT = {"identity": ref("sdlc.test-report"), "content": {"format": "json", "value_type": {"type": "object", "fields": {"passed": B, "command": S, "output": S}}}}
WAIT_MS = 300000


def fields(**items):
    return {k: {"value_type": v, "required": True} for k, v in items.items()}


def inp(name):
    return {"source": "workflow_input", "field": name}


def out(node, field):
    return {"source": "node_output", "node": node, "field": field}


def lit(value):
    return {"source": "literal", "value": value}


BASE = fields(request=S, repository=S, revision=S)
ATTEMPT_INPUT = {**BASE, **fields(context=LINK, feedback=S)}
ATTEMPT_OUTPUT = fields(candidate=LINK, passed=B, diagnostics=S)
CAPS = {}


def capability(name, inputs, outputs, usage, write=False, compensation=None):
    effects = {"type": "read_only"}
    if write:
        effects = {"type": "write", "idempotency": {"mode": "key", "scope": "sdlc.delivery", "retention_ms": 86400000}, "query": ref("delivery.query"), "compensation": compensation}
    c = {"schema_version": 1, "capability": ref(name), "inputs": inputs, "outputs": outputs, "timeout_ms": 60000, "error_codes": {"invalid": "invalid_input", "forbidden": "permission_denied", "rejected": "business_rejected", "retry": "transient"}, "effects": effects, "usage": usage, "skill": None}
    CAPS[name] = c
    return c


for name in ["diagnose", "design", "prepare-release"]:
    capability("sdlc." + name, BASE, fields(context=LINK), "Read the pinned source and request; return an immutable workspace/design artifact. No writes to the source repository.")
capability("sdlc.propose", ATTEMPT_INPUT, fields(candidate=LINK), "Produce an immutable candidate workspace from the context and previous test diagnostics. Never merge or publish.")
capability("sdlc.test", {**BASE, **fields(candidate=LINK)}, fields(passed=B, diagnostics=S), "Run the candidate tests in an isolated workspace; return the observed Boolean, diagnostics and a typed test report.")
capability("sdlc.verify", {**BASE, **fields(candidate=LINK, gate_policy_json=S, action_json=S)}, fields(passed=B, diagnostics=S, delivery=SUBJECT, review_digest=S, target_artifacts=LINKS), "Independently execute candidate tests; bind the typed report, release subject and review digest to these exact inputs and artifacts. No model or external write.")
capability("delivery.pr-create", fields(name=S, delivery=SUBJECT), fields(resource_id=S), "Create a provider-backed PR from reviewed immutable proposal artifacts; return its stable identity. Leave it awaiting merge.", True)
capability("delivery.publish", fields(name=S, delivery=SUBJECT), fields(resource_id=S), "Publish reviewed artifacts using provider idempotency, query and atomic source comparison; return the deployment identity.", True, ref("delivery.rollback"))
capability("delivery.rollback", fields(resource_id=S), fields(rolled_back=B), "Reverse only the original durable deployment receipt named by the compensating intent; retain its provider-backed compensation receipt.", True)
capability("delivery.health", {**BASE, **fields(candidate=LINK, resource_id=S)}, fields(passed=B, diagnostics=S, target_artifacts=LINKS), "Independently inspect the actual deployment and run a smoke test; persist the observed typed report.")
capability("delivery.query", {}, {}, "Query the immutable original operation key; return provider-backed effect protocol observations, never execute a write.")


def task(node, cap, bindings):
    c = CAPS[cap]
    return {"id": node, "kind": {"type": "task", "capability": c["capability"], "policy": None}, "inputs": c["inputs"], "outputs": c["outputs"], "bindings": bindings}


def edge(source, target, route="next", **extra):
    return {"id": source + "-" + target, "from": source, "to": target, "route": {"type": route, **extra}}


def terminal(name, outcome, inputs=None, bindings=None):
    return {"id": name, "kind": {"type": "terminal", "outcome": outcome}, "inputs": inputs or {}, "bindings": bindings or {}}


CHILD = {"schema_version": 1, **ref("sdlc.implementation-attempt"), "entry": "propose", "inputs": ATTEMPT_INPUT, "nodes": [
    task("propose", "sdlc.propose", {k: inp(k) for k in ATTEMPT_INPUT}),
    task("test", "sdlc.test", {**{k: inp(k) for k in BASE}, "candidate": out("propose", "candidate")}),
    terminal("observed", "succeeded", ATTEMPT_OUTPUT, {"candidate": out("propose", "candidate"), "passed": out("test", "passed"), "diagnostics": out("test", "diagnostics")}),
], "edges": [edge("propose", "test"), edge("test", "observed")]}


def make(kind, entry, goal):
    root = ref("sdlc." + kind)
    root_inputs = {**BASE, **fields(name=S)}
    nodes = [task("prepare", "sdlc." + entry, {k: inp(k) for k in BASE})]
    edges = [edge("prepare", "initial")]
    gates, waits, effects = [], [], []
    action = ref({"defect": "delivery.pr-create", "feature": "delivery.accept", "release": "delivery.publish"}[kind])

    def gate(node, checker, cap, action_ref):
        policy = {"schema_version": 1, "identity": ref(root["id"] + "." + checker + ".quality"), "requirements": [{"id": "tests", "node_id": checker, "capability": ref(cap), "contract_digest": digest(CAPS[cap]), "report_type": REPORT, "pass_field": "passed", "max_age_ms": 600000}]}
        return {"workflow": root, "node_id": node, "policy": policy, "action": action_ref, "repository": inp("repository"), "revision": inp("revision"), "input_node": checker, "artifacts": out(checker, "target_artifacts")}

    def effect(node, compensates=None, verify=None, review=None):
        binding = {"workflow": root, "node_id": node, "policy": {"identity": ref("sdlc.delivery-policy"), "target": ref("sdlc-delivery"), "call_identity": ref("sdlc-publisher"), "retry": {"max_calls": 6, "initial_backoff_ms": 100, "max_backoff_ms": 1000, "total_write_ms": 120000}}}
        if compensates:
            binding["compensates"] = compensates
        else:
            binding["release"] = {"gate_nodes": [verify], "subject_field": "delivery", "approvals": [{"gate_node": verify, "approval": {"node_id": review, "subject_field": "subject"}}], "target_check": {"type": "atomic_compare"}}
        effects.append(binding)

    for phase in ["initial", "rework"]:
        verify, review, done = [x + "_" + phase for x in ["verify", "review", "done"]]
        nodes.append({"id": phase, "kind": {"type": "subworkflow", "workflow": ref(CHILD["id"])}, "inputs": ATTEMPT_INPUT, "outputs": ATTEMPT_OUTPUT, "bindings": {**{k: inp(k) for k in BASE}, "context": out("prepare", "context") if phase == "initial" else out("initial", "candidate"), "feedback": lit("") if phase == "initial" else out("initial", "diagnostics")}})
        decide = "tested_" + phase
        nodes.append({"id": decide, "kind": {"type": "decision", "mode": "exclusive"}, "inputs": fields(passed=B), "bindings": {"passed": out(phase, "passed")}})
        edges += [edge(phase, decide), edge(decide, verify, "case", when={"op": "eq", "field": "passed", "value": True}), edge(decide, "rework" if phase == "initial" else "failed", "otherwise")]
        quality = gate(verify, verify, "sdlc.verify", action)
        gates.append(quality)
        nodes.append(task(verify, "sdlc.verify", {**{k: inp(k) for k in BASE}, "candidate": out(phase, "candidate"), "gate_policy_json": lit(json.dumps(quality["policy"], sort_keys=True, separators=(",", ":"))), "action_json": lit(json.dumps(action, sort_keys=True, separators=(",", ":")))}))
        nodes.append({"id": review, "kind": {"type": "wait", "event": "delivery-review", "timeout_ms": WAIT_MS}, "inputs": fields(subject=S), "bindings": {"subject": out(verify, "review_digest")}})
        waits.append({"workflow": root, "node_id": review, "policy": {"identity": ref(root["id"] + "." + review), "kind": "human_approval", "responders": ["delivery-reviewer"], "subjects": {"subject": "digest"}, "max_validity_ms": WAIT_MS, "exception": None}})
        edges += [edge(verify, review), edge(review, "rejected", "rejected"), edge(review, "expired", "timed_out")]
        nodes.append(terminal(done, "succeeded", fields(candidate=LINK), {"candidate": out(phase, "candidate")}))
        if kind == "feature":
            edges.append(edge(review, done, "accepted"))
            gates.append(gate(done, verify, "sdlc.verify", action))
        else:
            publish = "publish_" + phase
            nodes.append(task(publish, action["id"], {"name": inp("name"), "delivery": out(verify, "delivery")}))
            edges.append(edge(review, publish, "accepted"))
            effect(publish, verify=verify, review=review)
            if kind == "defect":
                edges.append(edge(publish, done))
                gates.append(gate(done, verify, "sdlc.verify", action))
            else:
                health, decision, rollback = [x + "_" + phase for x in ["health", "healthy", "rollback"]]
                nodes.append(task(health, "delivery.health", {**{k: inp(k) for k in BASE}, "candidate": out(phase, "candidate"), "resource_id": out(publish, "resource_id")}))
                nodes.append({"id": decision, "kind": {"type": "decision", "mode": "exclusive"}, "inputs": fields(passed=B), "bindings": {"passed": out(health, "passed")}})
                nodes.append(task(rollback, "delivery.rollback", {"resource_id": out(publish, "resource_id")}))
                edges += [edge(publish, health), edge(health, decision), edge(decision, done, "case", when={"op": "eq", "field": "passed", "value": True}), edge(decision, rollback, "otherwise"), edge(rollback, "failed")]
                effect(rollback, compensates=publish)
                gates.append(gate(done, health, "delivery.health", ref("delivery.accept")))
    nodes += [terminal("rejected", "cancelled"), terminal("expired", "failed"), terminal("failed", "failed")]
    flow = {"schema_version": 1, **root, "entry": "prepare", "inputs": root_inputs, "nodes": nodes, "edges": edges}
    used = {n["kind"]["capability"]["id"] for w in [flow, CHILD] for n in w["nodes"] if n["kind"]["type"] == "task"}
    if effects:
        used.add("delivery.query")
    bundle = {"schema_version": 1, "root": root, "workflows": [flow, CHILD], "capabilities": [CAPS[k] for k in sorted(used)], "postconditions": gates, "wait_policies": waits, "effect_bindings": effects, "model_policies": []}
    return {"schema_version": 1, "identity": ref("template." + kind), "owner_role": "sdlc-owner", "goal": goal,
            "applicability": {"requires": {"pinned-source": "The repository and immutable revision are available to authorized workers.", "isolated-tests": "The project has a bounded test command and isolated candidate workspace."}, "excludes": {"emergency-bypass": "An emergency exception needs a separately reviewed definition and approval policy."}},
            "roles": {"sdlc-owner": "Reviews exact template versions and regression evidence before publication.", "delivery-reviewer": "Reviews the bound candidate, checks and source before authorizing delivery.", "worker": "Uses scoped capability grants to prepare artifacts, execute tests and deliver only authorized effects."},
            "parameters": {k: {"field": v, "description": {"request": "Concrete business request and its testable acceptance criterion.", "repository": "Logical authorized source repository identity.", "revision": "Immutable source revision; provider must compare it at delivery.", "name": "Project-selected delivery name."}[k], "project_overridable": True, "default": None, "allowed_values": []} for k, v in root_inputs.items()},
            "deliverables": {"candidate": "Immutable workspace, design/context lineage and repair diagnostics.", "tests": "Typed independent test reports bound to current inputs and source.", "acceptance": "Verified run acceptance manifest, approvals and any provider receipts."},
            "exception_paths": {"success": "Tests pass, independent verification passes and the bound human review approves; delivery reaches its terminal acceptance gate.", "rework": "A failed first test sends its actual candidate and diagnostics to one repair attempt; repeated failure ends the run.", "rejected": "The reviewer rejects the candidate; cancel without delivery.", "timed_out": "The five-minute timeboxed review expires durably; fail without delivery. Longer review windows require a new reviewed template version.", "failed": "Repeated test failure terminates. Release smoke-test failure compensates the exact original deployment receipt; ambiguous effects remain queryable for recovery."},
            "budget": {"max_task_timeout_ms": 60000, "max_loop_iterations": 2, "max_loop_duration_ms": 600000, "max_wait_ms": WAIT_MS, "max_model_calls_per_task": 4, "max_effect_calls_per_attempt": 6}, "bundle": bundle}


def main():
    for kind, entry, goal in [("defect", "diagnose", "Turn a reproducible defect into a verified PR awaiting merge."), ("feature", "design", "Turn a requirement into reviewed design, implementation and test artifacts."), ("release", "prepare-release", "Verify and approve a release, publish it and compensate a failed deployment smoke test.")]:
        value = make(kind, entry, goal)
        (ROOT / (kind + ".json")).write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")
        environment = {"mode": "local", "capabilities": [{"capability": c["capability"], "contract_digest": digest(c), "host_binding": "project-adapters"} for c in value["bundle"]["capabilities"]], "effect_targets": ["sdlc-delivery@1.0.0"] if value["bundle"]["effect_bindings"] else [], "approvers": ["delivery-reviewer"], "model_policies": []}
        instance = {"run_id": "example-" + kind, "parameters": {"request": "Return the sum of two integers and pass the candidate test.", "repository": "project-source", "revision": "a" * 40, "name": "candidate-1"}, "business_facts": ["pinned-source", "isolated-tests"], "environment": environment, "started_at_unix_ms": 1000}
        (ROOT / (kind + "-local.json")).write_text(json.dumps(instance, indent=2) + "\n")
        shared = copy.deepcopy(instance)
        shared["environment"]["mode"] = "shared"
        for binding in shared["environment"]["capabilities"]:
            binding["host_binding"] = "cluster-adapters"
        (ROOT / (kind + "-shared.json")).write_text(json.dumps(shared, indent=2) + "\n")
    (ROOT / "owners.json").write_text(json.dumps({"owners": {"sdlc-owner": ["process-owner"]}}, indent=2) + "\n")


if __name__ == "__main__":
    main()
